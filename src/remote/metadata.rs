use crate::engine::domain::{EntryKind, RelativePath, Timestamp};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireEntryKind, WireMetadata,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::rooted_fs::{RootedFs, RootedFsError};
use bytes::Bytes;

#[derive(Debug, thiserror::Error)]
pub enum RemoteMetadataError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("metadata worker failed: {0}")]
    Worker(String),

    #[error("metadata stream {stream_id} ended before acknowledgement")]
    UnexpectedStreamEnd { stream_id: u32 },

    #[error("expected metadata frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("metadata frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("Metadata must use FINAL|ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    MetadataFlags { flags: u8 },

    #[error("metadata acknowledgement must be an empty unflagged frame")]
    InvalidAck,
}

impl From<SharedRouterError> for RemoteMetadataError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteMetadataError>;

pub async fn request_metadata(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let metadata = WireMetadata::new(
        encode_relative_path(path.as_path())?,
        wire_kind(kind),
        unix_mode,
        modified.map(|value| (value.seconds(), value.nanoseconds())),
    )?;
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::Metadata,
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            stream_id,
            metadata.encode()?,
        )?)
        .await?;
    receive_ack(&mut inbox, stream_id).await
}

pub async fn serve_incoming_metadata_rooted(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let IncomingStream { first, inbox: _ } = incoming;
    let stream_id = first.frame().stream_id();
    let frame = first.frame();
    require_stream(frame, stream_id)?;
    require_metadata_flags(frame)?;
    if frame.kind() != FrameKind::Metadata {
        return Err(RemoteMetadataError::UnexpectedFrame {
            expected: FrameKind::Metadata,
            actual: frame.kind(),
        });
    }

    let metadata = WireMetadata::decode(frame.payload())?;
    let relative = decode_relative_path(metadata.path.clone(), peer)?;
    let kind = domain_kind(metadata.kind());
    let unix_mode = metadata.unix_mode();
    let modified = metadata
        .modified()
        .map(|(seconds, nanoseconds)| Timestamp::new(seconds, nanoseconds))
        .transpose()
        .map_err(|_| ProtocolError::InvalidField {
            field: "modified_nanoseconds",
            reason: "nanoseconds must be below 1,000,000,000",
        })?;
    drop(first);

    tokio::task::spawn_blocking(move || {
        rooted.apply_metadata_blocking(&relative, kind, unix_mode, modified)
    })
    .await
    .map_err(|error| RemoteMetadataError::Worker(error.to_string()))??;

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

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteMetadataError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() || !frame.payload().is_empty() {
        return Err(RemoteMetadataError::InvalidAck);
    }
    Ok(())
}

fn wire_kind(kind: EntryKind) -> WireEntryKind {
    match kind {
        EntryKind::File => WireEntryKind::File,
        EntryKind::Directory => WireEntryKind::Directory,
        EntryKind::Symlink => WireEntryKind::Symlink,
    }
}

fn domain_kind(kind: WireEntryKind) -> EntryKind {
    match kind {
        WireEntryKind::File => EntryKind::File,
        WireEntryKind::Directory => EntryKind::Directory,
        WireEntryKind::Symlink => EntryKind::Symlink,
    }
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteMetadataError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_metadata_flags(frame: &Frame) -> Result<()> {
    let expected = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
    if frame.flags() == expected {
        Ok(())
    } else {
        Err(RemoteMetadataError::MetadataFlags {
            flags: frame.flags().bits(),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
    use std::os::unix::fs::MetadataExt;

    #[tokio::test]
    async fn metadata_stream_applies_fields_and_acks() {
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
            serve_incoming_metadata_rooted(rooted, incoming, &sender, peer)
                .await
                .unwrap();
        });

        let path = RelativePath::new("file").unwrap();
        let modified = Timestamp::new(1_600_000_010, 0).unwrap();
        request_metadata(
            &client.sender(),
            &path,
            EntryKind::File,
            Some(0o640),
            Some(modified),
            peer,
        )
        .await
        .unwrap();
        server_task.await.unwrap();

        let metadata = std::fs::metadata(root.path().join("file")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o640);
        assert_eq!(metadata.mtime(), modified.seconds());
    }
}
