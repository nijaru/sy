use crate::engine::domain::RelativePath;
use crate::engine::reconcile::EntryStream;
use crate::engine::scan::ScanRequest;
use crate::protocol::{ClientHello, FrameKind, Operation, PlatformOs, ServerHello, SessionReady};
use crate::remote::router::{
    FrameRouter, IncomingStream, RouterConfig, RouterError, RouterRole, RouterSender,
    SharedRouterError,
};
use crate::remote::scan::{request_scan, serve_incoming_scan};
use crate::remote::signature::{request_signatures, serve_incoming_signatures, SignatureStream};
use crate::remote::{client_handshake, server_handshake, OpenedServerSession};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, thiserror::Error)]
pub enum RemoteSessionError {
    #[error(transparent)]
    Control(#[from] crate::remote::RemoteError),

    #[error(transparent)]
    RouterStart(#[from] RouterError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error("unsupported v3 request opener: {0:?}")]
    UnsupportedRequest(FrameKind),
}

impl From<SharedRouterError> for RemoteSessionError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteSessionError>;

/// Client-side owner for one negotiated v3 remote session.
///
/// Construction consumes the transport halves: after the control-plane
/// handshake succeeds, only the central `FrameRouter` can read or write frames.
/// This makes raw post-handshake transport I/O impossible by construction.
pub struct ClientRemoteSession {
    server: ServerHello,
    ready: SessionReady,
    router: FrameRouter,
}

impl ClientRemoteSession {
    pub async fn connect<R, W>(
        mut reader: R,
        mut writer: W,
        operation: Operation,
        root: &Path,
        config: RouterConfig,
    ) -> Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let negotiated = client_handshake(&mut reader, &mut writer, operation, root).await?;
        let router = FrameRouter::start(reader, writer, RouterRole::Client, config)?;
        Ok(Self {
            server: negotiated.server,
            ready: negotiated.ready,
            router,
        })
    }

    pub fn server(&self) -> &ServerHello {
        &self.server
    }

    pub const fn ready(&self) -> SessionReady {
        self.ready
    }

    pub fn sender(&self) -> RouterSender {
        self.router.sender()
    }

    pub async fn scan(&self, request: ScanRequest) -> crate::remote::scan::Result<EntryStream> {
        request_scan(&self.router.sender(), request, self.server.platform.os).await
    }

    pub async fn signatures(
        &self,
        path: &RelativePath,
        file_size: u64,
    ) -> crate::remote::signature::Result<(u32, SignatureStream)> {
        request_signatures(
            &self.router.sender(),
            path,
            file_size,
            self.server.platform.os,
        )
        .await
    }
}

/// Peer-opened operations currently implemented by the v3 session runtime.
pub enum IncomingRequest {
    Scan(IncomingStream),
    Signatures(IncomingStream),
}

/// Cloneable server-side context for servicing scan streams concurrently.
#[derive(Clone)]
pub struct ServerScanHandler {
    root: PathBuf,
    sender: RouterSender,
}

impl ServerScanHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> crate::remote::scan::Result<()> {
        serve_incoming_scan(&self.root, incoming, &self.sender).await
    }
}

/// Cloneable server-side context for demand-driven rolling signatures.
#[derive(Clone)]
pub struct ServerSignatureHandler {
    root: PathBuf,
    sender: RouterSender,
    peer: PlatformOs,
}

impl ServerSignatureHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> crate::remote::signature::Result<()> {
        serve_incoming_signatures(&self.root, incoming, &self.sender, self.peer).await
    }
}

/// Server-side owner for one negotiated v3 remote session.
///
/// The session owns the transport router while cloneable operation handlers own
/// only endpoint context plus a router sender. This lets the daemon accept new
/// streams while existing operations run without exposing the raw transport.
pub struct ServerRemoteSession {
    opened: OpenedServerSession,
    router: FrameRouter,
}

impl ServerRemoteSession {
    pub async fn accept<R, W>(mut reader: R, mut writer: W, config: RouterConfig) -> Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let opened = server_handshake(&mut reader, &mut writer).await?;
        let router = FrameRouter::start(reader, writer, RouterRole::Server, config)?;
        Ok(Self { opened, router })
    }

    pub fn client(&self) -> &ClientHello {
        &self.opened.client
    }

    pub const fn operation(&self) -> Operation {
        self.opened.operation
    }

    pub fn root(&self) -> &Path {
        &self.opened.root
    }

    pub const fn ready(&self) -> SessionReady {
        self.opened.ready
    }

    pub fn sender(&self) -> RouterSender {
        self.router.sender()
    }

    pub fn scan_handler(&self) -> ServerScanHandler {
        ServerScanHandler {
            root: self.opened.root.clone(),
            sender: self.router.sender(),
        }
    }

    pub fn signature_handler(&self) -> ServerSignatureHandler {
        ServerSignatureHandler {
            root: self.opened.root.clone(),
            sender: self.router.sender(),
            peer: self.opened.client.platform.os,
        }
    }

    pub async fn next_request(&mut self) -> Result<Option<IncomingRequest>> {
        let Some(incoming) = self.router.incoming().recv().await? else {
            return Ok(None);
        };

        match incoming.first.frame().kind() {
            FrameKind::ScanRequest => Ok(Some(IncomingRequest::Scan(incoming))),
            FrameKind::SignatureRequest => Ok(Some(IncomingRequest::Signatures(incoming))),
            actual => Err(RemoteSessionError::UnsupportedRequest(actual)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan::EntryMetadataRequest;
    use crate::remote::signature::{SignatureEvent, SignatureSummary};
    use futures::StreamExt;
    use tokio::task::JoinSet;

    async fn collect_paths(mut entries: EntryStream) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        while let Some(entry) = entries.next().await {
            paths.push(entry.unwrap().path.as_path().to_path_buf());
        }
        paths
    }

    async fn collect_signature_summary(
        mut signatures: SignatureStream,
    ) -> (usize, SignatureSummary) {
        let mut block_count = 0_usize;
        let mut summary = None;
        while let Some(event) = signatures.next().await {
            match event.unwrap() {
                SignatureEvent::Block(_) => block_count += 1,
                SignatureEvent::End(end) => summary = Some(end),
            }
        }
        (block_count, summary.unwrap())
    }

    #[tokio::test]
    async fn session_runtime_multiplexes_two_ordered_scans() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("a"), b"a").unwrap();
        std::fs::write(root.path().join("dir").join("b"), b"b").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);

        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let handler = session.scan_handler();
            let mut tasks = JoinSet::new();
            for _ in 0..2 {
                let request = session.next_request().await.unwrap().unwrap();
                let IncomingRequest::Scan(incoming) = request else {
                    panic!("expected scan request");
                };
                let handler = handler.clone();
                tasks.spawn(async move {
                    handler.serve(incoming).await.unwrap();
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let request = ScanRequest {
            respect_gitignore: false,
            include_git_dir: false,
            max_depth: None,
            metadata: EntryMetadataRequest {
                unix_mode: cfg!(unix),
                symlink_target: true,
                identity: true,
                hardlink_group: false,
            },
        };

        // Both streams are opened before either is consumed. The server also
        // services them concurrently, proving the session runtime owns genuine
        // multiplexing rather than merely serial stream IDs.
        let first = session.scan(request).await.unwrap();
        let second = session.scan(request).await.unwrap();
        let (first, second) = tokio::join!(collect_paths(first), collect_paths(second));
        server.await.unwrap();

        let expected = vec![
            PathBuf::from("a"),
            PathBuf::from("dir"),
            PathBuf::from("dir/b"),
        ];
        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }

    #[tokio::test]
    async fn session_runtime_multiplexes_scan_and_signatures() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("a"), b"a").unwrap();
        let data = (0..10_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(root.path().join("data.bin"), &data).unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);

        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan_handler = session.scan_handler();
            let signature_handler = session.signature_handler();
            let mut tasks = JoinSet::new();

            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => {
                        let handler = scan_handler.clone();
                        tasks.spawn(async move {
                            handler.serve(incoming).await.unwrap();
                        });
                    }
                    IncomingRequest::Signatures(incoming) => {
                        let handler = signature_handler.clone();
                        tasks.spawn(async move {
                            handler.serve(incoming).await.unwrap();
                        });
                    }
                }
            }

            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let scan_request = ScanRequest {
            respect_gitignore: false,
            include_git_dir: false,
            max_depth: None,
            metadata: EntryMetadataRequest {
                unix_mode: cfg!(unix),
                symlink_target: true,
                identity: true,
                hardlink_group: false,
            },
        };
        let signature_path = RelativePath::new(PathBuf::from("data.bin")).unwrap();

        // Open both operation types before consuming either. This proves the
        // runtime can interleave metadata and demand-driven signature traffic on
        // the same negotiated transport.
        let entries = session.scan(scan_request).await.unwrap();
        let (block_size, signatures) = session
            .signatures(&signature_path, data.len() as u64)
            .await
            .unwrap();
        let (paths, (blocks, summary)) = tokio::join!(
            collect_paths(entries),
            collect_signature_summary(signatures)
        );
        server.await.unwrap();

        assert_eq!(block_size, 4 * 1024);
        assert_eq!(blocks, 3);
        assert_eq!(
            summary,
            SignatureSummary {
                file_size: 10_000,
                block_count: 3
            }
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("a"),
                PathBuf::from("data.bin"),
                PathBuf::from("dir")
            ]
        );
    }
}
