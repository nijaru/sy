use crate::endpoint::local_identity::metadata_identity;
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath};
use crate::protocol::{
    CapabilitySet, Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId,
    WireHashRequest, WireHashResult, HASH_DIGEST_LEN,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::rooted_fs::{RootedFs, RootedFsError};
use bytes::Bytes;
use std::io::Read;

const HASH_BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RemoteHashError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("content-hash worker failed: {0}")]
    Worker(String),

    #[error("peer did not advertise BLAKE3 support")]
    UnsupportedByPeer,

    #[error("content hashing requires a regular-file entry")]
    InvalidBasis,

    #[error("content hashing requires a scanned file identity")]
    MissingBasisIdentity,

    #[error("opened file did not provide a stable identity")]
    MissingObservedIdentity,

    #[error(
        "content-hash file size changed since scan (expected {expected} bytes, observed {actual} bytes)"
    )]
    SizeChanged { expected: u64, actual: u64 },

    #[error("content-hash file identity changed since scan")]
    IdentityChanged,

    #[error("content-hash stream {stream_id} ended before {expected:?}")]
    UnexpectedStreamEnd { stream_id: u32, expected: FrameKind },

    #[error("expected content-hash frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("content-hash frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("HashRequest must use FINAL and no other flags, got 0x{flags:02x}")]
    RequestFlags { flags: u8 },

    #[error("HashResult must use FINAL|ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    ResultFlags { flags: u8 },

    #[error("content-hash acknowledgement must be an empty unflagged frame")]
    InvalidAck,

    #[error("content-hash result did not match the requested scan snapshot")]
    ResultSnapshotMismatch,
}

impl From<SharedRouterError> for RemoteHashError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteHashError>;

pub async fn request_content_hash(
    sender: &RouterSender,
    basis: &Entry,
    peer: PlatformOs,
) -> Result<[u8; HASH_DIGEST_LEN]> {
    ensure_compatible_path_encoding(peer)?;
    if !basis.is_file() {
        return Err(RemoteHashError::InvalidBasis);
    }
    let identity = basis
        .identity
        .ok_or(RemoteHashError::MissingBasisIdentity)?;
    let request = WireHashRequest::new(
        encode_relative_path(basis.path.as_path())?,
        basis.size,
        *identity.as_bytes(),
    );
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::HashRequest,
            FrameFlags::FINAL,
            stream_id,
            request.encode()?,
        )?)
        .await?;

    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteHashError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::HashResult,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    require_result_flags(frame)?;
    if frame.kind() != FrameKind::HashResult {
        return Err(RemoteHashError::UnexpectedFrame {
            expected: FrameKind::HashResult,
            actual: frame.kind(),
        });
    }
    let result = WireHashResult::decode(frame.payload())?;
    if result.file_size() != basis.size || result.identity() != *identity.as_bytes() {
        return Err(RemoteHashError::ResultSnapshotMismatch);
    }
    let digest = result.digest();
    drop(routed);
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            Bytes::new(),
        )?)
        .await?;
    Ok(digest)
}

pub async fn serve_incoming_hash_rooted(
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
    if frame.kind() != FrameKind::HashRequest {
        return Err(RemoteHashError::UnexpectedFrame {
            expected: FrameKind::HashRequest,
            actual: frame.kind(),
        });
    }
    let request = WireHashRequest::decode(frame.payload())?;
    let path = decode_relative_path(request.path.clone(), peer)?;
    let expected_size = request.file_size();
    let expected_identity = EntryIdentity::from_bytes(*request.identity());
    drop(first);

    let digest = hash_rooted_file(rooted, path, expected_size, expected_identity).await?;
    let result = WireHashResult::new(expected_size, *expected_identity.as_bytes(), digest);
    sender
        .send(Frame::new(
            FrameKind::HashResult,
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            stream_id,
            result.encode(),
        )?)
        .await?;
    receive_ack(&mut inbox, stream_id).await
}

pub async fn hash_rooted_file(
    rooted: RootedFs,
    path: RelativePath,
    expected_size: u64,
    expected_identity: EntryIdentity,
) -> Result<[u8; HASH_DIGEST_LEN]> {
    tokio::task::spawn_blocking(move || {
        hash_rooted_file_blocking(&rooted, &path, expected_size, expected_identity)
    })
    .await
    .map_err(|error| RemoteHashError::Worker(error.to_string()))?
}

fn hash_rooted_file_blocking(
    rooted: &RootedFs,
    path: &RelativePath,
    expected_size: u64,
    expected_identity: EntryIdentity,
) -> Result<[u8; HASH_DIGEST_LEN]> {
    let mut file = rooted.open_regular_blocking(path)?;
    validate_snapshot(&file, expected_size, expected_identity)?;

    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];
    loop {
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(RootedFsError::Io(error).into()),
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    validate_snapshot(&file, expected_size, expected_identity)?;
    Ok(*hasher.finalize().as_bytes())
}

fn validate_snapshot(
    file: &std::fs::File,
    expected_size: u64,
    expected_identity: EntryIdentity,
) -> Result<()> {
    let metadata = file.metadata().map_err(RootedFsError::Io)?;
    if metadata.len() != expected_size {
        return Err(RemoteHashError::SizeChanged {
            expected: expected_size,
            actual: metadata.len(),
        });
    }
    let actual = metadata_identity(&metadata, EntryKind::File)
        .ok_or(RemoteHashError::MissingObservedIdentity)?;
    if actual != expected_identity {
        return Err(RemoteHashError::IdentityChanged);
    }
    Ok(())
}

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteHashError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::Ack,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() || !frame.payload().is_empty() {
        return Err(RemoteHashError::InvalidAck);
    }
    Ok(())
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteHashError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_request_flags(frame: &Frame) -> Result<()> {
    if frame.flags() == FrameFlags::FINAL {
        Ok(())
    } else {
        Err(RemoteHashError::RequestFlags {
            flags: frame.flags().bits(),
        })
    }
}

fn require_result_flags(frame: &Frame) -> Result<()> {
    let expected = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
    if frame.flags() == expected {
        Ok(())
    } else {
        Err(RemoteHashError::ResultFlags {
            flags: frame.flags().bits(),
        })
    }
}

pub fn require_blake3(capabilities: CapabilitySet) -> Result<()> {
    if capabilities.contains(CapabilitySet::BLAKE3) {
        Ok(())
    } else {
        Err(RemoteHashError::UnsupportedByPeer)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::engine::domain::Timestamp;
    use crate::protocol::Platform;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};

    fn file_entry(root: &std::path::Path, relative: &str) -> Entry {
        let metadata = std::fs::metadata(root.join(relative)).unwrap();
        let identity = metadata_identity(&metadata, EntryKind::File).unwrap();
        let mut entry = Entry::file(
            RelativePath::new(relative).unwrap(),
            metadata.len(),
            Timestamp::UNIX_EPOCH,
        );
        entry.identity = Some(identity);
        entry
    }

    #[tokio::test]
    async fn content_hash_stream_round_trips_confined_digest() {
        let root = tempfile::TempDir::new().unwrap();
        let data = b"whole-file-content-hash";
        std::fs::write(root.path().join("file"), data).unwrap();
        let basis = file_entry(root.path(), "file");
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
        let peer = Platform::current().os;
        let server_task = tokio::spawn(async move {
            let incoming = server.incoming().recv().await.unwrap().unwrap();
            serve_incoming_hash_rooted(rooted, incoming, &sender, peer)
                .await
                .unwrap();
        });

        let digest = request_content_hash(&client.sender(), &basis, peer)
            .await
            .unwrap();
        server_task.await.unwrap();
        assert_eq!(digest, *blake3::hash(data).as_bytes());
    }

    #[tokio::test]
    async fn rooted_hash_rejects_scan_snapshot_change() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"before").unwrap();
        let basis = file_entry(root.path(), "file");
        std::fs::write(root.path().join("file"), b"after-change").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let error = hash_rooted_file(rooted, basis.path, basis.size, basis.identity.unwrap())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RemoteHashError::SizeChanged { .. } | RemoteHashError::IdentityChanged
        ));
    }
}
