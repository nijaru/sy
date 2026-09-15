//! Bounded BSD-flags read/write RPCs and the shared preservation step used
//! by every v3 executor.
//!
//! `-F/--preserve-flags` is demand-driven and macOS-only: the scan never
//! carries flag words, and the executor reads the source's flags only for
//! entries it is already mutating. The value is a single `u32`, so one
//! request/result pair per entry is fixed-size and needs no bound beyond the
//! frame payload cap.
//!
//! Holds: the server serves flag requests only in a Push session. In a Pull
//! session the remote root is the read-only source, so a write request is
//! rejected before it can mutate source data.

use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::engine::domain::{EntryKind, RelativePath};
use crate::protocol::{
    BsdFlagsMode, Frame, FrameFlags, FrameKind, Operation, PlatformOs, ProtocolError, StreamId,
    WireBsdFlagsRequest, WireBsdFlagsResult,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::remote::runtime::ClientRemoteHandle;
use crate::rooted_fs::{RootedFs, RootedFsError};
use bytes::Bytes;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoteBsdFlagsError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("bsd flags worker failed: {0}")]
    Worker(String),

    #[error("bsd flags stream {stream_id} ended before {expected:?}")]
    UnexpectedStreamEnd { stream_id: u32, expected: FrameKind },

    #[error("expected bsd flags frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("bsd flags frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("BsdFlagsRequest must use FINAL and no other flags, got 0x{flags:02x}")]
    RequestFlags { flags: u8 },

    #[error("BsdFlagsResult must use FINAL|ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    ResultFlags { flags: u8 },

    #[error("bsd flags acknowledgement must be an empty unflagged frame")]
    InvalidAck,

    #[error(
        "bsd flags write requests are refused in a {0:?} session: the remote root is source-only"
    )]
    WriteInSourceSession(Operation),

    #[error("local bsd flags access failed: {0}")]
    Local(String),
}

impl From<SharedRouterError> for RemoteBsdFlagsError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteBsdFlagsError>;

/// Where one side of a BSD-flags preservation step lives. Local roots are
/// addressed through `LocalEndpoint`; a remote peer is addressed through the
/// session's bounded request handle.
pub enum BsdFlagsLocation<'a> {
    Local(&'a Path),
    Remote(&'a ClientRemoteHandle),
}

/// Request the peer's current BSD-flags value for one entry.
pub async fn request_read_bsd_flags(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    peer: PlatformOs,
) -> Result<u32> {
    ensure_compatible_path_encoding(peer)?;
    let request =
        WireBsdFlagsRequest::read(encode_relative_path(path.as_path())?, wire_kind(kind))?;
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::BsdFlagsRequest,
            FrameFlags::FINAL,
            stream_id,
            request.encode()?,
        )?)
        .await?;

    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteBsdFlagsError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::BsdFlagsResult,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    require_result_flags(frame)?;
    if frame.kind() != FrameKind::BsdFlagsResult {
        return Err(RemoteBsdFlagsError::UnexpectedFrame {
            expected: FrameKind::BsdFlagsResult,
            actual: frame.kind(),
        });
    }
    let result = WireBsdFlagsResult::decode(frame.payload())?;
    drop(routed);
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            Bytes::new(),
        )?)
        .await?;
    Ok(result.flags())
}

/// Replace the peer's BSD-flags value for one entry (0 clears every flag).
/// The acknowledgement is sent only after the mirror completes.
pub async fn request_write_bsd_flags(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    flags: u32,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let request = WireBsdFlagsRequest::write(
        encode_relative_path(path.as_path())?,
        wire_kind(kind),
        flags,
    )?;
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::BsdFlagsRequest,
            FrameFlags::FINAL,
            stream_id,
            request.encode()?,
        )?)
        .await?;
    receive_ack(&mut inbox, stream_id).await
}

pub async fn serve_incoming_bsd_flags_rooted(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
    operation: Operation,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let IncomingStream { first, mut inbox } = incoming;
    let stream_id = inbox.stream_id();
    let frame = first.frame();
    require_stream(frame, stream_id)?;
    require_request_flags(frame)?;
    if frame.kind() != FrameKind::BsdFlagsRequest {
        return Err(RemoteBsdFlagsError::UnexpectedFrame {
            expected: FrameKind::BsdFlagsRequest,
            actual: frame.kind(),
        });
    }
    let request = WireBsdFlagsRequest::decode(frame.payload())?;
    let path = decode_relative_path(request.path().clone(), peer)?;
    let kind = domain_kind(request.kind());
    let mode = request.mode();
    let flags = request.flags().unwrap_or(0);
    drop(first);

    if mode == BsdFlagsMode::Write && operation != Operation::Push {
        return Err(RemoteBsdFlagsError::WriteInSourceSession(operation));
    }

    match mode {
        BsdFlagsMode::Read => {
            let flags =
                tokio::task::spawn_blocking(move || rooted.read_bsd_flags_blocking(&path, kind))
                    .await
                    .map_err(|error| RemoteBsdFlagsError::Worker(error.to_string()))??;
            let result = WireBsdFlagsResult::new(flags);
            sender
                .send(Frame::new(
                    FrameKind::BsdFlagsResult,
                    FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                    stream_id,
                    result.encode()?,
                )?)
                .await?;
            receive_ack(&mut inbox, stream_id).await
        }
        BsdFlagsMode::Write => {
            tokio::task::spawn_blocking(move || {
                rooted.write_bsd_flags_blocking(&path, kind, flags)
            })
            .await
            .map_err(|error| RemoteBsdFlagsError::Worker(error.to_string()))??;
            sender
                .send(Frame::new(
                    FrameKind::Ack,
                    FrameFlags::empty(),
                    stream_id,
                    Bytes::new(),
                )?)
                .await?;
            Ok(())
        }
    }
}

/// Read the requested entry's BSD-flags value from the source side of one
/// sync. A missing list reads as 0 (no flags), matching the local endpoint.
pub async fn read_preserved_bsd_flags(
    location: &BsdFlagsLocation<'_>,
    path: &RelativePath,
    kind: EntryKind,
) -> Result<u32> {
    match location {
        BsdFlagsLocation::Local(root) => {
            let root = root.to_path_buf();
            Ok(LocalEndpoint::new(root)
                .read_bsd_flags(path.as_path())
                .await
                .map_err(|error| RemoteBsdFlagsError::Local(error.to_string()))?
                .unwrap_or(0))
        }
        BsdFlagsLocation::Remote(handle) => {
            request_read_bsd_flags(&handle.sender(), path, kind, handle.peer_platform()).await
        }
    }
}

/// Mirror a previously read BSD-flags value onto the destination side.
pub async fn apply_preserved_bsd_flags(
    location: &BsdFlagsLocation<'_>,
    path: &RelativePath,
    kind: EntryKind,
    flags: u32,
) -> Result<()> {
    match location {
        BsdFlagsLocation::Local(root) => {
            let root = root.to_path_buf();
            LocalEndpoint::new(root)
                .write_bsd_flags(path.as_path(), flags)
                .await
                .map_err(|error| RemoteBsdFlagsError::Local(error.to_string()))
        }
        BsdFlagsLocation::Remote(handle) => {
            request_write_bsd_flags(&handle.sender(), path, kind, flags, handle.peer_platform())
                .await
        }
    }
}

fn wire_kind(kind: EntryKind) -> crate::protocol::WireEntryKind {
    match kind {
        EntryKind::File => crate::protocol::WireEntryKind::File,
        EntryKind::Directory => crate::protocol::WireEntryKind::Directory,
        EntryKind::Symlink => crate::protocol::WireEntryKind::Symlink,
    }
}

fn domain_kind(kind: crate::protocol::WireEntryKind) -> EntryKind {
    match kind {
        crate::protocol::WireEntryKind::File => EntryKind::File,
        crate::protocol::WireEntryKind::Directory => EntryKind::Directory,
        crate::protocol::WireEntryKind::Symlink => EntryKind::Symlink,
    }
}

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteBsdFlagsError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::Ack,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() || !frame.payload().is_empty() {
        return Err(RemoteBsdFlagsError::InvalidAck);
    }
    Ok(())
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteBsdFlagsError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_request_flags(frame: &Frame) -> Result<()> {
    if frame.flags() == FrameFlags::FINAL {
        Ok(())
    } else {
        Err(RemoteBsdFlagsError::RequestFlags {
            flags: frame.flags().bits(),
        })
    }
}

fn require_result_flags(frame: &Frame) -> Result<()> {
    let expected = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
    if frame.flags() == expected {
        Ok(())
    } else {
        Err(RemoteBsdFlagsError::ResultFlags {
            flags: frame.flags().bits(),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};

    fn serve(
        mut server: FrameRouter,
        rooted: RootedFs,
        peer: PlatformOs,
        operation: Operation,
        requests: usize,
    ) -> tokio::task::JoinHandle<()> {
        let sender = server.sender();
        tokio::spawn(async move {
            for _ in 0..requests {
                let incoming = server.incoming().recv().await.unwrap().unwrap();
                serve_incoming_bsd_flags_rooted(rooted.clone(), incoming, &sender, peer, operation)
                    .await
                    .unwrap();
            }
        })
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bsd_flags_round_trip_reads_and_mirrors_write() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let client = FrameRouter::start(
            client_reader,
            client_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();
        let server = FrameRouter::start(
            server_reader,
            server_writer,
            RouterRole::Server,
            RouterConfig::default(),
        )
        .unwrap();
        let peer = crate::protocol::Platform::current().os;
        let server_task = serve(server, rooted, peer, Operation::Push, 2);

        let path = RelativePath::new("file").unwrap();
        // UF_NODUMP (0x1) is visible in st_flags and has no side effects on
        // the test's own execution.
        request_write_bsd_flags(&client.sender(), &path, EntryKind::File, 0x1, peer)
            .await
            .unwrap();
        let read = request_read_bsd_flags(&client.sender(), &path, EntryKind::File, peer)
            .await
            .unwrap();
        server_task.await.unwrap();

        assert_eq!(read, 0x1);
    }

    #[tokio::test]
    async fn write_into_a_pull_session_is_refused() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let client = FrameRouter::start(
            client_reader,
            client_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();
        let mut server = FrameRouter::start(
            server_reader,
            server_writer,
            RouterRole::Server,
            RouterConfig::default(),
        )
        .unwrap();
        let sender = server.sender();
        let peer = crate::protocol::Platform::current().os;
        let server_task = tokio::spawn(async move {
            let incoming = server.incoming().recv().await.unwrap().unwrap();
            serve_incoming_bsd_flags_rooted(rooted, incoming, &sender, peer, Operation::Pull).await
        });

        let path = RelativePath::new("file").unwrap();
        // Send a raw write request because the fallible server closes the
        // stream without an acknowledgement, which the client surfaces as an
        // unexpected end rather than a clean RPC result.
        let request = WireBsdFlagsRequest::write(
            encode_relative_path(path.as_path()).unwrap(),
            crate::protocol::WireEntryKind::File,
            0x1,
        )
        .unwrap();
        let inbox = client.sender().open_stream().unwrap();
        let stream_id = inbox.stream_id();
        client
            .sender()
            .send(
                Frame::new(
                    FrameKind::BsdFlagsRequest,
                    FrameFlags::FINAL,
                    stream_id,
                    request.encode().unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let error = server_task.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            RemoteBsdFlagsError::WriteInSourceSession(Operation::Pull)
        ));
        // The source file was never mutated (on macOS the flag word is
        // checked; elsewhere the refusal precedes any filesystem touch by
        // construction, as the macOS case proves).
        #[cfg(target_os = "macos")]
        assert_eq!(
            rooted_flags(root.path().join("file")),
            0,
            "pull-session write must not touch the source"
        );
    }

    #[cfg(target_os = "macos")]
    fn rooted_flags(path: std::path::PathBuf) -> u32 {
        use std::os::macos::fs::MetadataExt;
        std::fs::metadata(path).unwrap().st_flags()
    }
}
