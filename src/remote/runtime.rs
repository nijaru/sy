use crate::engine::domain::Entry;
use crate::engine::reconcile::EntryStream;
use crate::engine::scan::ScanRequest;
use crate::protocol::{
    CapabilitySet, ClientHello, FrameKind, Operation, PlatformOs, ServerHello, SessionReady,
};
use crate::remote::router::{
    FrameRouter, IncomingStream, RouterConfig, RouterError, RouterRole, RouterSender,
    SharedRouterError,
};
use crate::remote::scan::{request_scan, serve_incoming_scan};
use crate::remote::signature::{
    choose_signature_block_size, request_signatures, serve_incoming_signatures,
    RemoteSignatureError, SignatureEvent, SignatureStream,
};
use crate::remote::transfer::{
    request_file_transfer, serve_incoming_file, RemoteDeltaBasis, RemoteTransferError,
    TransferSummary,
};
use crate::remote::{client_handshake, server_handshake, OpenedServerSession};
use crate::transfer::delta::{
    BasisBlock, BasisIndex, BasisIndexBuilder, BasisIndexError, BasisIndexLimits,
};
use futures::StreamExt;
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

    #[error("v3 request {kind:?} is invalid for {operation:?} session direction")]
    OperationMismatch {
        operation: Operation,
        kind: FrameKind,
    },

    #[error(transparent)]
    Signature(#[from] RemoteSignatureError),

    #[error(transparent)]
    Transfer(#[from] RemoteTransferError),

    #[error(transparent)]
    BasisIndex(#[from] BasisIndexError),

    #[error("remote signature stream failed: {0}")]
    SignatureStream(String),

    #[error("remote signature stream ended without SignatureEnd")]
    MissingSignatureEnd,

    #[error(
        "signature block-size selection changed unexpectedly: expected {expected}, got {actual}"
    )]
    SignatureBlockSizeMismatch { expected: u32, actual: u32 },
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
    operation: Operation,
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
            operation,
            server: negotiated.server,
            ready: negotiated.ready,
            router,
        })
    }

    pub const fn operation(&self) -> Operation {
        self.operation
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
        basis: &Entry,
    ) -> crate::remote::signature::Result<(u32, SignatureStream)> {
        if !self
            .ready
            .capabilities
            .contains(CapabilitySet::ROLLING_SIGNATURES)
        {
            return Err(RemoteSignatureError::UnsupportedByPeer);
        }
        if !basis.is_file() {
            return Err(RemoteSignatureError::InvalidBasis);
        }
        let identity = basis
            .identity
            .ok_or(RemoteSignatureError::MissingBasisIdentity)?;
        request_signatures(
            &self.router.sender(),
            &basis.path,
            basis.size,
            identity,
            self.server.platform.os,
        )
        .await
    }

    /// Build the bounded rolling-delta basis index directly from the validated
    /// remote signature stream. If an honest basis would exceed the configured
    /// signature budget, return `None` before opening a remote stream so the
    /// planner can fall back to whole-file transfer without wasting bandwidth.
    pub async fn delta_basis(
        &self,
        basis: &Entry,
        limits: BasisIndexLimits,
    ) -> Result<Option<BasisIndex>> {
        if !self
            .ready
            .capabilities
            .contains(CapabilitySet::ROLLING_SIGNATURES)
        {
            return Err(RemoteSignatureError::UnsupportedByPeer.into());
        }
        if !basis.is_file() {
            return Err(RemoteSignatureError::InvalidBasis.into());
        }
        if basis.identity.is_none() {
            return Err(RemoteSignatureError::MissingBasisIdentity.into());
        }

        let block_size = choose_signature_block_size(basis.size);
        let mut builder = BasisIndexBuilder::new(block_size, limits)?;
        let max_blocks = u64::try_from(limits.max_blocks).unwrap_or(u64::MAX);
        let expected_blocks = basis.size.div_ceil(u64::from(block_size));
        if expected_blocks > max_blocks {
            return Ok(None);
        }

        let (actual_block_size, mut signatures) = self.signatures(basis).await?;
        if actual_block_size != block_size {
            return Err(RemoteSessionError::SignatureBlockSizeMismatch {
                expected: block_size,
                actual: actual_block_size,
            });
        }

        let mut over_limit = false;
        loop {
            let Some(event) = signatures.next().await else {
                return Err(RemoteSessionError::MissingSignatureEnd);
            };
            let event =
                event.map_err(|error| RemoteSessionError::SignatureStream(error.to_string()))?;
            match event {
                SignatureEvent::Block(block) if !over_limit => {
                    let block = BasisBlock {
                        index: block.index,
                        size: block.size,
                        weak: block.weak,
                        strong: block.strong,
                    };
                    match builder.push(block) {
                        Ok(()) => {}
                        Err(BasisIndexError::TooManyBlocks { .. }) => over_limit = true,
                        Err(error) => return Err(error.into()),
                    }
                }
                SignatureEvent::Block(_) => {}
                SignatureEvent::End(_) => break,
            }
        }

        if over_limit {
            Ok(None)
        } else {
            Ok(Some(builder.finish()))
        }
    }

    /// Transfer one regular file from a pinned local source root into this
    /// remote Push session. Delta mode reuses a previously validated remote
    /// basis/index without collecting transfer operations in memory.
    pub async fn transfer_file(
        &self,
        source_root: PathBuf,
        source: Entry,
        delta_basis: Option<RemoteDeltaBasis>,
    ) -> Result<TransferSummary> {
        if self.operation != Operation::Push {
            return Err(RemoteSessionError::OperationMismatch {
                operation: self.operation,
                kind: FrameKind::FileBegin,
            });
        }
        request_file_transfer(
            &self.router.sender(),
            source_root,
            source,
            delta_basis,
            self.server.platform.os,
        )
        .await
        .map_err(Into::into)
    }
}

/// Peer-opened operations currently implemented by the v3 session runtime.
pub enum IncomingRequest {
    Scan(IncomingStream),
    Signatures(IncomingStream),
    File(IncomingStream),
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

/// Cloneable server-side context for verified staged file reconstruction.
#[derive(Clone)]
pub struct ServerFileHandler {
    root: PathBuf,
    sender: RouterSender,
    peer: PlatformOs,
}

impl ServerFileHandler {
    pub async fn serve(
        &self,
        incoming: IncomingStream,
    ) -> crate::remote::transfer::Result<TransferSummary> {
        serve_incoming_file(self.root.clone(), incoming, &self.sender, self.peer).await
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

    pub fn file_handler(&self) -> ServerFileHandler {
        ServerFileHandler {
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
            FrameKind::FileBegin if self.opened.operation == Operation::Push => {
                Ok(Some(IncomingRequest::File(incoming)))
            }
            FrameKind::FileBegin => Err(RemoteSessionError::OperationMismatch {
                operation: self.opened.operation,
                kind: FrameKind::FileBegin,
            }),
            actual => Err(RemoteSessionError::UnsupportedRequest(actual)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::engine::domain::{EntryKind, RelativePath, Timestamp};
    use crate::engine::scan::EntryMetadataRequest;
    use crate::transfer::delta::BasisIndexLimits;
    use tokio::task::JoinSet;

    #[cfg(unix)]
    fn file_entry(root: &Path, relative: &str) -> Entry {
        let path = root.join(relative);
        let metadata = std::fs::metadata(&path).unwrap();
        let identity =
            crate::endpoint::local_identity::metadata_identity(&metadata, EntryKind::File).unwrap();
        let mut entry = Entry::file(
            RelativePath::new(PathBuf::from(relative)).unwrap(),
            metadata.len(),
            Timestamp::UNIX_EPOCH,
        );
        entry.identity = Some(identity);
        entry
    }

    async fn collect_paths(mut entries: EntryStream) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        while let Some(entry) = entries.next().await {
            paths.push(entry.unwrap().path.as_path().to_path_buf());
        }
        paths
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

    #[test]
    fn signature_budget_preflight_avoids_unbounded_index_growth() {
        let block_size = choose_signature_block_size(80 * 1024 * 1024 * 1024);
        assert_eq!(block_size, 1024 * 1024);
        let expected_blocks = (80_u64 * 1024 * 1024 * 1024).div_ceil(u64::from(block_size));
        assert!(expected_blocks > u64::try_from(BasisIndexLimits::default().max_blocks).unwrap());
    }

    #[cfg(unix)]
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
                    IncomingRequest::File(_) => panic!("unexpected file request"),
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
        let signature_basis = file_entry(root.path(), "data.bin");

        // Open both operation types before consuming either. This proves the
        // runtime can interleave metadata and demand-driven signature traffic on
        // the same negotiated transport.
        let entries = session.scan(scan_request).await.unwrap();
        let (paths, delta_basis) = tokio::join!(
            collect_paths(entries),
            session.delta_basis(&signature_basis, BasisIndexLimits::default())
        );
        server.await.unwrap();

        let delta_basis = delta_basis.unwrap().unwrap();
        assert_eq!(delta_basis.block_size(), 4 * 1024);
        assert_eq!(delta_basis.block_count(), 3);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("a"),
                PathBuf::from("data.bin"),
                PathBuf::from("dir")
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_runtime_dispatches_verified_file_transfer() {
        let source_root = tempfile::TempDir::new().unwrap();
        let destination_root = tempfile::TempDir::new().unwrap();
        let data = b"runtime-transfer";
        std::fs::write(source_root.path().join("file.bin"), data).unwrap();
        std::fs::write(destination_root.path().join("file.bin"), b"old").unwrap();
        let source = file_entry(source_root.path(), "file.bin");

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let handler = session.file_handler();
            let request = session.next_request().await.unwrap().unwrap();
            let IncomingRequest::File(incoming) = request else {
                panic!("expected file request");
            };
            handler.serve(incoming).await.unwrap()
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let sent = session
            .transfer_file(source_root.path().to_path_buf(), source, None)
            .await
            .unwrap();
        let received = server.await.unwrap();

        assert_eq!(sent, received);
        assert_eq!(
            std::fs::read(destination_root.path().join("file.bin")).unwrap(),
            data
        );
    }
}
