use crate::engine::domain::RelativePath;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireMutation,
    WireMutationKind, WirePath,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::rooted_fs::{RootedFs, RootedFsError};
use bytes::Bytes;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum RemoteMutationError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("namespace mutation worker failed: {0}")]
    Worker(String),

    #[error("namespace mutation stream {stream_id} ended before acknowledgement")]
    UnexpectedStreamEnd { stream_id: u32 },

    #[error("expected namespace-mutation frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("namespace-mutation frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("Mutation must use FINAL|ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    MutationFlags { flags: u8 },

    #[error("namespace mutation acknowledgement must be an empty unflagged frame")]
    InvalidAck,

    #[error("replace-symlink mutation is missing its target")]
    MissingSymlinkTarget,

    #[error("copy-file mutation is missing its source path")]
    MissingCopySource,

    #[error("native symlink target encoding is unsupported for peer platform {0:?}")]
    UnsupportedTargetEncoding(PlatformOs),

    #[error("native symlink target contains a NUL code unit")]
    TargetContainsNul,
}

impl From<SharedRouterError> for RemoteMutationError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteMutationError>;

pub async fn request_create_directory(
    sender: &RouterSender,
    path: &RelativePath,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    request_mutation(
        sender,
        WireMutation::create_directory(encode_relative_path(path.as_path())?),
    )
    .await
}

pub async fn request_replace_symlink(
    sender: &RouterSender,
    path: &RelativePath,
    target: &Path,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    request_mutation(
        sender,
        WireMutation::replace_symlink(
            encode_relative_path(path.as_path())?,
            encode_native_target(target, peer)?,
        ),
    )
    .await
}

pub async fn request_remove(
    sender: &RouterSender,
    path: &RelativePath,
    is_directory: bool,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let path = encode_relative_path(path.as_path())?;
    let mutation = if is_directory {
        WireMutation::remove_directory(path)
    } else {
        WireMutation::remove_file_like(path)
    };
    request_mutation(sender, mutation).await
}

/// Request a server-side copy beneath the pinned root (`--backup`: copy the
/// soon-to-be-replaced or deleted destination file before the mutation).
pub async fn request_copy_file(
    sender: &RouterSender,
    source: &RelativePath,
    destination: &RelativePath,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let source = encode_relative_path(source.as_path())?;
    let destination = encode_relative_path(destination.as_path())?;
    request_mutation(sender, WireMutation::copy_file(source, destination)).await
}

async fn request_mutation(sender: &RouterSender, mutation: WireMutation) -> Result<()> {
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::Mutation,
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            stream_id,
            mutation.encode()?,
        )?)
        .await?;
    receive_ack(&mut inbox, stream_id).await
}

pub async fn serve_incoming_mutation_rooted(
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
    require_mutation_flags(frame)?;
    if frame.kind() != FrameKind::Mutation {
        return Err(RemoteMutationError::UnexpectedFrame {
            expected: FrameKind::Mutation,
            actual: frame.kind(),
        });
    }

    let mutation = WireMutation::decode(frame.payload())?;
    let relative = decode_relative_path(mutation.path.clone(), peer)?;
    let kind = mutation.kind();
    let target = mutation
        .symlink_target()
        .cloned()
        .map(|target| decode_native_target(target, peer))
        .transpose()?;
    let copy_source = if kind == WireMutationKind::CopyFile {
        let source = mutation
            .copy_source()
            .cloned()
            .ok_or(RemoteMutationError::Protocol(ProtocolError::InvalidField {
                field: "copy_source",
                reason: "copy-file mutation requires a source path",
            }))?;
        Some(decode_relative_path(source, peer)?)
    } else {
        None
    };
    drop(first);

    tokio::task::spawn_blocking(move || {
        apply_mutation(&rooted, relative, kind, target, copy_source)
    })
    .await
    .map_err(|error| RemoteMutationError::Worker(error.to_string()))??;

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

fn apply_mutation(
    rooted: &RootedFs,
    path: RelativePath,
    kind: WireMutationKind,
    target: Option<PathBuf>,
    copy_source: Option<RelativePath>,
) -> Result<()> {
    match kind {
        WireMutationKind::CreateDirectory => rooted.create_directory_blocking(&path)?,
        WireMutationKind::ReplaceSymlink => {
            let target = target.ok_or(RemoteMutationError::MissingSymlinkTarget)?;
            rooted.replace_symlink_blocking(&path, &target)?;
        }
        WireMutationKind::RemoveFileLike => rooted.remove_blocking(&path, false)?,
        WireMutationKind::RemoveDirectory => rooted.remove_blocking(&path, true)?,
        WireMutationKind::CopyFile => {
            let source = copy_source.ok_or(RemoteMutationError::MissingCopySource)?;
            rooted.copy_file_blocking(&source, &path)?;
        }
    }
    Ok(())
}

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteMutationError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() || !frame.payload().is_empty() {
        return Err(RemoteMutationError::InvalidAck);
    }
    Ok(())
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteMutationError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_mutation_flags(frame: &Frame) -> Result<()> {
    let expected = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
    if frame.flags() == expected {
        Ok(())
    } else {
        Err(RemoteMutationError::MutationFlags {
            flags: frame.flags().bits(),
        })
    }
}

#[cfg(unix)]
fn encode_native_target(path: &Path, peer: PlatformOs) -> Result<WirePath> {
    use std::os::unix::ffi::OsStrExt;

    ensure_compatible_path_encoding(peer)?;
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(RemoteMutationError::TargetContainsNul);
    }
    WirePath::new(Bytes::copy_from_slice(bytes)).map_err(Into::into)
}

#[cfg(unix)]
fn decode_native_target(path: WirePath, peer: PlatformOs) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    ensure_compatible_path_encoding(peer)?;
    let bytes = path.into_bytes();
    if bytes.contains(&0) {
        return Err(RemoteMutationError::TargetContainsNul);
    }
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn encode_native_target(path: &Path, peer: PlatformOs) -> Result<WirePath> {
    use std::os::windows::ffi::OsStrExt;

    if peer != PlatformOs::Windows {
        return Err(RemoteMutationError::UnsupportedTargetEncoding(peer));
    }
    let mut bytes = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        if unit == 0 {
            return Err(RemoteMutationError::TargetContainsNul);
        }
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    WirePath::new(bytes).map_err(Into::into)
}

#[cfg(windows)]
fn decode_native_target(path: WirePath, peer: PlatformOs) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    if peer != PlatformOs::Windows {
        return Err(RemoteMutationError::UnsupportedTargetEncoding(peer));
    }
    let bytes = path.into_bytes();
    if bytes.len() % 2 != 0 {
        return Err(RemoteMutationError::UnsupportedTargetEncoding(peer));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(RemoteMutationError::TargetContainsNul);
    }
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(not(any(unix, windows)))]
fn encode_native_target(_path: &Path, peer: PlatformOs) -> Result<WirePath> {
    Err(RemoteMutationError::UnsupportedTargetEncoding(peer))
}

#[cfg(not(any(unix, windows)))]
fn decode_native_target(_path: WirePath, peer: PlatformOs) -> Result<PathBuf> {
    Err(RemoteMutationError::UnsupportedTargetEncoding(peer))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::protocol::Platform;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};

    #[test]
    fn native_target_round_trip_preserves_parent_and_absolute_forms() {
        for target in [Path::new("../target"), Path::new("/absolute/target")] {
            let encoded = encode_native_target(target, Platform::current().os).unwrap();
            let decoded = decode_native_target(encoded, Platform::current().os).unwrap();
            assert_eq!(decoded, target);
        }
    }

    #[tokio::test]
    async fn mutation_stream_applies_confined_operations_and_acks() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("old"), b"old").unwrap();
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
            for _ in 0..4 {
                let incoming = server.incoming().recv().await.unwrap().unwrap();
                serve_incoming_mutation_rooted(rooted.clone(), incoming, &sender, peer)
                    .await
                    .unwrap();
            }
        });

        let dir = RelativePath::new("dir").unwrap();
        request_create_directory(&client.sender(), &dir, peer)
            .await
            .unwrap();
        let link = RelativePath::new("link").unwrap();
        request_replace_symlink(&client.sender(), &link, Path::new("../target"), peer)
            .await
            .unwrap();
        let old = RelativePath::new("old").unwrap();
        request_remove(&client.sender(), &old, false, peer)
            .await
            .unwrap();
        request_remove(&client.sender(), &dir, true, peer)
            .await
            .unwrap();
        server_task.await.unwrap();

        assert!(!root.path().join("old").exists());
        assert!(!root.path().join("dir").exists());
        assert_eq!(
            std::fs::read_link(root.path().join("link")).unwrap(),
            Path::new("../target")
        );
    }
}
