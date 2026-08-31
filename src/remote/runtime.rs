#[path = "client.rs"]
mod client;
#[path = "metadata.rs"]
mod metadata;
#[path = "mutation.rs"]
mod mutation;
pub use client::ClientRemoteHandle;

use crate::engine::domain::{Entry, EntryKind, RelativePath, Timestamp};
use crate::engine::reconcile::EntryStream;
use crate::engine::scan::ScanRequest;
use crate::protocol::{
    CapabilitySet, ClientHello, FrameKind, Operation, PlatformOs, ServerHello, SessionReady,
};
use crate::remote::hash::{request_content_hash, serve_incoming_hash_rooted, RemoteHashError};
use crate::remote::router::{
    FrameRouter, IncomingStream, RouterConfig, RouterError, RouterRole, RouterSender,
    SharedRouterError,
};
use crate::remote::scan::{request_scan, serve_incoming_scan_rooted};
use crate::remote::signature::{
    choose_signature_block_size, request_signatures, serve_incoming_signatures_rooted,
    RemoteSignatureError, SignatureEvent, SignatureStream,
};
use crate::remote::transfer::{
    request_file_transfer, serve_incoming_file_rooted, RemoteDeltaBasis, RemoteTransferError,
    TransferSummary,
};
use crate::remote::{client_handshake, server_handshake, OpenedServerSession};
use crate::rooted_fs::RootedFs;
use crate::transfer::delta::{
    BasisBlock, BasisIndex, BasisIndexBuilder, BasisIndexError, BasisIndexLimits,
};
use futures::StreamExt;
use metadata::{request_metadata, serve_incoming_metadata_rooted, RemoteMetadataError};
use mutation::{
    request_create_directory, request_remove, request_replace_symlink,
    serve_incoming_mutation_rooted, RemoteMutationError,
};
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
    Hash(#[from] RemoteHashError),

    #[error(transparent)]
    Signature(#[from] RemoteSignatureError),

    #[error(transparent)]
    Transfer(#[from] RemoteTransferError),

    #[error(transparent)]
    Metadata(#[from] RemoteMetadataError),

    #[error(transparent)]
    Mutation(#[from] RemoteMutationError),

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

    pub async fn content_hash(
        &self,
        basis: &Entry,
    ) -> crate::remote::hash::Result<[u8; crate::protocol::HASH_DIGEST_LEN]> {
        crate::remote::hash::require_blake3(self.ready.capabilities)?;
        request_content_hash(&self.router.sender(), basis, self.server.platform.os).await
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

    pub async fn transfer_file(
        &self,
        source_root: PathBuf,
        source: Entry,
        delta_basis: Option<RemoteDeltaBasis>,
    ) -> Result<TransferSummary> {
        self.require_push(FrameKind::FileBegin)?;
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

    pub async fn apply_metadata(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> Result<()> {
        self.require_push(FrameKind::Metadata)?;
        request_metadata(
            &self.router.sender(),
            path,
            kind,
            unix_mode,
            modified,
            self.server.platform.os,
        )
        .await
        .map_err(Into::into)
    }

    pub async fn create_directory(&self, path: &RelativePath) -> Result<()> {
        self.require_push(FrameKind::Mutation)?;
        request_create_directory(&self.router.sender(), path, self.server.platform.os)
            .await
            .map_err(Into::into)
    }

    pub async fn replace_symlink(&self, path: &RelativePath, target: &Path) -> Result<()> {
        self.require_push(FrameKind::Mutation)?;
        request_replace_symlink(&self.router.sender(), path, target, self.server.platform.os)
            .await
            .map_err(Into::into)
    }

    pub async fn remove(&self, path: &RelativePath, is_directory: bool) -> Result<()> {
        self.require_push(FrameKind::Mutation)?;
        request_remove(
            &self.router.sender(),
            path,
            is_directory,
            self.server.platform.os,
        )
        .await
        .map_err(Into::into)
    }

    fn require_push(&self, kind: FrameKind) -> Result<()> {
        if self.operation == Operation::Push {
            Ok(())
        } else {
            Err(RemoteSessionError::OperationMismatch {
                operation: self.operation,
                kind,
            })
        }
    }
}

pub enum IncomingRequest {
    Scan(IncomingStream),
    Hash(IncomingStream),
    Signatures(IncomingStream),
    File(IncomingStream),
    Metadata(IncomingStream),
    Mutation(IncomingStream),
}

#[derive(Clone)]
pub struct ServerScanHandler {
    rooted: RootedFs,
    sender: RouterSender,
}

impl ServerScanHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> crate::remote::scan::Result<()> {
        serve_incoming_scan_rooted(self.rooted.clone(), incoming, &self.sender).await
    }
}

#[derive(Clone)]
pub struct ServerHashHandler {
    rooted: RootedFs,
    sender: RouterSender,
    peer: PlatformOs,
}

impl ServerHashHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> crate::remote::hash::Result<()> {
        serve_incoming_hash_rooted(self.rooted.clone(), incoming, &self.sender, self.peer).await
    }
}

/// Signature requests share the root descriptor opened at SessionOpen. Cloning
/// this handler duplicates authority to that same held directory, not the root
/// pathname.
#[derive(Clone)]
pub struct ServerSignatureHandler {
    rooted: RootedFs,
    sender: RouterSender,
    peer: PlatformOs,
}

impl ServerSignatureHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> crate::remote::signature::Result<()> {
        serve_incoming_signatures_rooted(self.rooted.clone(), incoming, &self.sender, self.peer)
            .await
    }
}

/// File reconstruction shares the root descriptor opened at SessionOpen, so a
/// later rename or symlink replacement of the root pathname cannot redirect a
/// transfer.
#[derive(Clone)]
pub struct ServerFileHandler {
    rooted: RootedFs,
    sender: RouterSender,
    peer: PlatformOs,
}

impl ServerFileHandler {
    pub async fn serve(
        &self,
        incoming: IncomingStream,
    ) -> crate::remote::transfer::Result<TransferSummary> {
        serve_incoming_file_rooted(self.rooted.clone(), incoming, &self.sender, self.peer).await
    }
}

/// Metadata-only updates use the same session-pinned root descriptor and never
/// resolve the root pathname after SessionOpen.
#[derive(Clone)]
pub struct ServerMetadataHandler {
    rooted: RootedFs,
    sender: RouterSender,
    peer: PlatformOs,
}

impl ServerMetadataHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> metadata::Result<()> {
        serve_incoming_metadata_rooted(self.rooted.clone(), incoming, &self.sender, self.peer).await
    }
}

/// Namespace mutations use the same session-pinned root descriptor as file
/// reconstruction. The mutation request never reopens the root pathname.
#[derive(Clone)]
pub struct ServerMutationHandler {
    rooted: RootedFs,
    sender: RouterSender,
    peer: PlatformOs,
}

impl ServerMutationHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> mutation::Result<()> {
        serve_incoming_mutation_rooted(self.rooted.clone(), incoming, &self.sender, self.peer).await
    }
}

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
            rooted: self.opened.rooted.clone(),
            sender: self.router.sender(),
        }
    }

    pub fn hash_handler(&self) -> ServerHashHandler {
        ServerHashHandler {
            rooted: self.opened.rooted.clone(),
            sender: self.router.sender(),
            peer: self.opened.client.platform.os,
        }
    }

    pub fn signature_handler(&self) -> ServerSignatureHandler {
        ServerSignatureHandler {
            rooted: self.opened.rooted.clone(),
            sender: self.router.sender(),
            peer: self.opened.client.platform.os,
        }
    }

    pub fn file_handler(&self) -> ServerFileHandler {
        ServerFileHandler {
            rooted: self.opened.rooted.clone(),
            sender: self.router.sender(),
            peer: self.opened.client.platform.os,
        }
    }

    pub fn metadata_handler(&self) -> ServerMetadataHandler {
        ServerMetadataHandler {
            rooted: self.opened.rooted.clone(),
            sender: self.router.sender(),
            peer: self.opened.client.platform.os,
        }
    }

    pub fn mutation_handler(&self) -> ServerMutationHandler {
        ServerMutationHandler {
            rooted: self.opened.rooted.clone(),
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
            FrameKind::HashRequest => Ok(Some(IncomingRequest::Hash(incoming))),
            FrameKind::SignatureRequest => Ok(Some(IncomingRequest::Signatures(incoming))),
            FrameKind::FileBegin if self.opened.operation == Operation::Push => {
                Ok(Some(IncomingRequest::File(incoming)))
            }
            FrameKind::Metadata if self.opened.operation == Operation::Push => {
                Ok(Some(IncomingRequest::Metadata(incoming)))
            }
            FrameKind::Mutation if self.opened.operation == Operation::Push => {
                Ok(Some(IncomingRequest::Mutation(incoming)))
            }
            FrameKind::FileBegin | FrameKind::Metadata | FrameKind::Mutation => {
                Err(RemoteSessionError::OperationMismatch {
                    operation: self.opened.operation,
                    kind: incoming.first.frame().kind(),
                })
            }
            actual => Err(RemoteSessionError::UnsupportedRequest(actual)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::engine::domain::{EntryKind, Timestamp};
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
        let first_handle = session.request_handle();
        let second_handle = first_handle.clone();
        let first =
            tokio::spawn(
                async move { collect_paths(first_handle.scan(request).await.unwrap()).await },
            );
        let second =
            tokio::spawn(
                async move { collect_paths(second_handle.scan(request).await.unwrap()).await },
            );
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();
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
                    IncomingRequest::Hash(_)
                    | IncomingRequest::File(_)
                    | IncomingRequest::Metadata(_)
                    | IncomingRequest::Mutation(_) => {
                        panic!("unexpected mutation request")
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
        let signature_basis = file_entry(root.path(), "data.bin");

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

    #[cfg(unix)]
    #[tokio::test]
    async fn session_runtime_dispatches_metadata_update() {
        use std::os::unix::fs::MetadataExt;

        let destination_root = tempfile::TempDir::new().unwrap();
        std::fs::write(destination_root.path().join("file"), b"data").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let handler = session.metadata_handler();
            let request = session.next_request().await.unwrap().unwrap();
            let IncomingRequest::Metadata(incoming) = request else {
                panic!("expected metadata request");
            };
            handler.serve(incoming).await.unwrap();
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
        let modified = Timestamp::new(1_600_000_020, 0).unwrap();
        session
            .apply_metadata(
                &RelativePath::new("file").unwrap(),
                EntryKind::File,
                Some(0o640),
                Some(modified),
            )
            .await
            .unwrap();
        server.await.unwrap();

        let metadata = std::fs::metadata(destination_root.path().join("file")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o640);
        assert_eq!(metadata.mtime(), modified.seconds());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_runtime_dispatches_namespace_mutations() {
        let destination_root = tempfile::TempDir::new().unwrap();
        std::fs::write(destination_root.path().join("old"), b"old").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let handler = session.mutation_handler();
            for _ in 0..4 {
                let request = session.next_request().await.unwrap().unwrap();
                let IncomingRequest::Mutation(incoming) = request else {
                    panic!("expected mutation request");
                };
                handler.serve(incoming).await.unwrap();
            }
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
        let dir = RelativePath::new("dir").unwrap();
        session.create_directory(&dir).await.unwrap();
        let link = RelativePath::new("link").unwrap();
        session
            .replace_symlink(&link, Path::new("../target"))
            .await
            .unwrap();
        session
            .remove(&RelativePath::new("old").unwrap(), false)
            .await
            .unwrap();
        session.remove(&dir, true).await.unwrap();
        server.await.unwrap();

        assert!(!destination_root.path().join("old").exists());
        assert!(!destination_root.path().join("dir").exists());
        assert_eq!(
            std::fs::read_link(destination_root.path().join("link")).unwrap(),
            Path::new("../target")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_file_handler_survives_root_path_swap() {
        use std::os::unix::fs::symlink;

        let source_root = tempfile::TempDir::new().unwrap();
        let parent = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let root_path = parent.path().join("root");
        let moved_path = parent.path().join("moved");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(root_path.join("file.bin"), b"pinned-old").unwrap();
        std::fs::write(outside.path().join("file.bin"), b"outside").unwrap();
        std::fs::write(source_root.path().join("file.bin"), b"new").unwrap();
        let source = file_entry(source_root.path(), "file.bin");

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let handler = session.file_handler();
            ready_tx.send(()).unwrap();
            let request = session.next_request().await.unwrap().unwrap();
            let IncomingRequest::File(incoming) = request else {
                panic!("expected file request");
            };
            handler.serve(incoming).await.unwrap();
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            &root_path,
            RouterConfig::default(),
        )
        .await
        .unwrap();
        ready_rx.await.unwrap();
        std::fs::rename(&root_path, &moved_path).unwrap();
        symlink(outside.path(), &root_path).unwrap();

        session
            .transfer_file(source_root.path().to_path_buf(), source, None)
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(std::fs::read(moved_path.join("file.bin")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(outside.path().join("file.bin")).unwrap(),
            b"outside"
        );
    }
}
