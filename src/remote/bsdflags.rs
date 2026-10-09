//! Fixed-size observation-bound source BSD-flags reads. Destination flags
//! belong to the single observed metadata owner.

use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireBsdFlagsResult,
    WireSourceMetadataRead,
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

    #[error("source preservation requires a scanned identity for {0}")]
    MissingSourceIdentity(RelativePath),
}

impl From<SharedRouterError> for RemoteBsdFlagsError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteBsdFlagsError>;

/// Source-side rooted observation or bounded remote request handle.
pub enum BsdFlagsLocation<'a> {
    Local(&'a Path),
    Remote(&'a ClientRemoteHandle),
}

/// Request the peer's current BSD-flags value for one entry.
pub async fn request_read_bsd_flags(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    expected: EntryIdentity,
    peer: PlatformOs,
) -> Result<u32> {
    ensure_compatible_path_encoding(peer)?;
    let request = WireSourceMetadataRead::new(
        encode_relative_path(path.as_path())?,
        wire_kind(kind),
        *expected.as_bytes(),
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

pub async fn serve_incoming_bsd_flags_rooted(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
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
    let request = WireSourceMetadataRead::decode(frame.payload())?;
    let path = decode_relative_path(request.path().clone(), peer)?;
    let kind = domain_kind(request.kind());
    let expected = EntryIdentity::from_bytes(request.identity());
    drop(first);
    let flags = tokio::task::spawn_blocking(move || {
        rooted.read_observed_bsd_flags_blocking(&path, kind, expected)
    })
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

/// Read the requested entry's BSD-flags value from the source side of one
/// sync. Unsupported platforms fail rather than silently returning no flags.
pub async fn read_preserved_bsd_flags(
    location: &BsdFlagsLocation<'_>,
    source: &Entry,
) -> Result<u32> {
    let expected = source
        .identity
        .ok_or_else(|| RemoteBsdFlagsError::MissingSourceIdentity(source.path.clone()))?;
    match location {
        BsdFlagsLocation::Local(root) => {
            let root = root.to_path_buf();
            let path = source.path.clone();
            let kind = source.kind;
            tokio::task::spawn_blocking(move || {
                RootedFs::open_blocking_for_worker(root)?
                    .read_observed_bsd_flags_blocking(&path, kind, expected)
            })
            .await
            .map_err(|error| RemoteBsdFlagsError::Worker(error.to_string()))?
            .map_err(Into::into)
        }
        BsdFlagsLocation::Remote(handle) => {
            request_read_bsd_flags(
                &handle.sender(),
                &source.path,
                source.kind,
                expected,
                handle.peer_platform(),
            )
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

    #[cfg(target_os = "macos")]
    fn serve(
        mut server: FrameRouter,
        rooted: RootedFs,
        peer: PlatformOs,
        requests: usize,
    ) -> tokio::task::JoinHandle<()> {
        let sender = server.sender();
        tokio::spawn(async move {
            for _ in 0..requests {
                let incoming = server.incoming().recv().await.unwrap().unwrap();
                serve_incoming_bsd_flags_rooted(rooted.clone(), incoming, &sender, peer)
                    .await
                    .unwrap();
            }
        })
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn observed_flags_read_returns_native_flags() {
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
        let server_task = serve(server, rooted.clone(), peer, 1);

        let path = RelativePath::new("file").unwrap();
        // UF_NODUMP (0x1) is visible in st_flags and has no side effects on
        // the test's own execution.
        rooted
            .write_bsd_flags_blocking(&path, EntryKind::File, 1)
            .unwrap();
        let expected = crate::endpoint::local_identity::metadata_identity(
            &std::fs::metadata(root.path().join("file")).unwrap(),
            EntryKind::File,
        )
        .unwrap();
        let read = request_read_bsd_flags(&client.sender(), &path, EntryKind::File, expected, peer)
            .await
            .unwrap();
        server_task.await.unwrap();

        assert_eq!(read, 0x1);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn source_flags_rpc_rejects_an_ancestor_swap_without_minting_foreign_proof() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/file"), b"A").unwrap();
        let path = RelativePath::new("nested/file").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let expected = rooted.path_identity_blocking(&path).unwrap().unwrap().1;
        std::fs::rename(root.path().join("nested"), root.path().join("original")).unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/file"), b"B").unwrap();
        rooted
            .write_bsd_flags_blocking(&path, EntryKind::File, 1)
            .unwrap();
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
        let request = WireSourceMetadataRead::new(
            encode_relative_path(path.as_path()).unwrap(),
            crate::protocol::WireEntryKind::File,
            *expected.as_bytes(),
        )
        .unwrap();
        let inbox = client.sender().open_stream().unwrap();
        let stream = inbox.stream_id();
        client
            .sender()
            .send(
                Frame::new(
                    FrameKind::BsdFlagsRequest,
                    FrameFlags::FINAL,
                    stream,
                    request.encode().unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        // Drain the legacy success path too, so a regression fails rather than
        // hanging waiting for an acknowledgement after returning foreign flags.
        client
            .sender()
            .send(Frame::new(FrameKind::Ack, FrameFlags::empty(), stream, Bytes::new()).unwrap())
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            let incoming = server.incoming().recv().await.unwrap().unwrap();
            serve_incoming_bsd_flags_rooted(rooted, incoming, &sender, peer).await
        });
        let result = task.await.unwrap();
        std::fs::remove_file(root.path().join("nested/file")).unwrap();
        std::fs::remove_dir(root.path().join("nested")).unwrap();
        std::fs::rename(root.path().join("original"), root.path().join("nested")).unwrap();
        assert_eq!(
            crate::endpoint::local_identity::metadata_identity(
                &std::fs::metadata(root.path().join("nested/file")).unwrap(),
                EntryKind::File
            ),
            Some(expected)
        );
        assert_eq!(rooted_flags(root.path().join("nested/file")), 0);
        assert!(matches!(
            result,
            Err(RemoteBsdFlagsError::RootedFs(
                RootedFsError::SourceMetadataChanged(_)
            ))
        ));
    }

    #[cfg(target_os = "macos")]
    fn rooted_flags(path: std::path::PathBuf) -> u32 {
        use std::os::macos::fs::MetadataExt;
        std::fs::metadata(path).unwrap().st_flags()
    }
}
