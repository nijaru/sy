use crate::engine::domain::{EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireEntryKind, WireMutation,
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

    #[error("namespace mutation acknowledgement has invalid payload kind, size, or flags")]
    InvalidAck,

    #[error("replace-symlink mutation is missing its target")]
    MissingSymlinkTarget,

    #[error("copy-file mutation is missing its source path")]
    MissingCopySource,

    #[error("copy/hardlink mutation is missing its source identity")]
    MissingCopySourceIdentity,

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
) -> Result<EntryIdentity> {
    ensure_compatible_path_encoding(peer)?;
    let bytes = request_mutation_payload(
        sender,
        WireMutation::create_directory(encode_relative_path(path.as_path())?),
    )
    .await?;
    let identity = bytes
        .as_ref()
        .try_into()
        .map_err(|_| RemoteMutationError::InvalidAck)?;
    Ok(EntryIdentity::from_bytes(identity))
}

pub async fn request_replace_symlink(
    sender: &RouterSender,
    path: &RelativePath,
    target: &Path,
    expected_identity: Option<EntryIdentity>,
    modified: Option<Timestamp>,
    peer: PlatformOs,
) -> Result<crate::rooted_fs::PublishedEntryProof> {
    ensure_compatible_path_encoding(peer)?;
    let payload = request_mutation_payload(
        sender,
        WireMutation::replace_symlink(
            encode_relative_path(path.as_path())?,
            encode_native_target(target, peer)?,
            expected_identity.map(|identity| *identity.as_bytes()),
            modified.map(|time| (time.seconds(), time.nanoseconds())),
        ),
    )
    .await?;
    decode_publication_ack(path, EntryKind::Symlink, &payload)
}

pub async fn request_remove(
    sender: &RouterSender,
    path: &RelativePath,
    is_directory: bool,
    expected_identity: Option<EntryIdentity>,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let path = encode_relative_path(path.as_path())?;
    let expected_identity = expected_identity.map(|id| *id.as_bytes());
    let mutation = if is_directory {
        WireMutation::remove_directory(path, expected_identity)
    } else {
        WireMutation::remove_file_like(path, expected_identity)
    };
    request_mutation(sender, mutation).await
}

/// Request a server-side copy beneath the pinned root (`--backup`: copy the
/// soon-to-be-replaced or deleted destination file before the mutation).
pub async fn request_copy_file(
    sender: &RouterSender,
    source: &RelativePath,
    destination: &RelativePath,
    expected_source_identity: EntryIdentity,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let source = encode_relative_path(source.as_path())?;
    let destination = encode_relative_path(destination.as_path())?;
    request_mutation(
        sender,
        WireMutation::copy_file(source, destination, *expected_source_identity.as_bytes()),
    )
    .await
}

/// Request a server-side hardlink beneath the pinned root
/// (`-H/--preserve-hardlinks`: link `destination` to existing `source`).
pub async fn request_hardlink(
    sender: &RouterSender,
    source: &RelativePath,
    destination: &RelativePath,
    source_identity: EntryIdentity,
    expected_destination: Option<EntryIdentity>,
    peer: PlatformOs,
) -> Result<crate::rooted_fs::PublishedEntryProof> {
    ensure_compatible_path_encoding(peer)?;
    let payload = request_mutation_payload(
        sender,
        WireMutation::hardlink(
            encode_relative_path(source.as_path())?,
            encode_relative_path(destination.as_path())?,
            *source_identity.as_bytes(),
            expected_destination.map(|id| *id.as_bytes()),
        ),
    )
    .await?;
    decode_publication_ack(destination, EntryKind::File, &payload)
}

pub(crate) async fn request_verify_publication(
    sender: &RouterSender,
    proof: &crate::rooted_fs::PublishedEntryProof,
    peer: PlatformOs,
) -> Result<()> {
    request_verify_destination(sender, &proof.path, proof.kind, proof.identity, peer).await
}

pub(crate) async fn request_verify_destination(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    identity: EntryIdentity,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    request_mutation(
        sender,
        WireMutation::verify_publication(
            encode_relative_path(path.as_path())?,
            *identity.as_bytes(),
            wire_kind(kind),
        ),
    )
    .await
}

async fn request_mutation(sender: &RouterSender, mutation: WireMutation) -> Result<()> {
    if !request_mutation_payload(sender, mutation).await?.is_empty() {
        return Err(RemoteMutationError::InvalidAck);
    }
    Ok(())
}

async fn request_mutation_payload(sender: &RouterSender, mutation: WireMutation) -> Result<Bytes> {
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
    let copy_source = if kind == WireMutationKind::CopyFile || kind == WireMutationKind::Hardlink {
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
    let expected_identity = mutation
        .expected_identity()
        .copied()
        .map(EntryIdentity::from_bytes);
    let source_identity = mutation.source_identity().map(EntryIdentity::from_bytes);
    let entry_kind = mutation.entry_kind().map(domain_kind);
    let modified = mutation
        .modified()
        .map(|(seconds, nanos)| Timestamp::new(seconds, nanos))
        .transpose()
        .map_err(|_| ProtocolError::InvalidField {
            field: "modified_nanoseconds",
            reason: "nanoseconds must be less than one second",
        })?;
    drop(first);

    let payload = tokio::task::spawn_blocking(move || {
        apply_mutation(
            &rooted,
            relative,
            kind,
            target,
            copy_source,
            expected_identity,
            modified,
            source_identity,
            entry_kind,
        )
    })
    .await
    .map_err(|error| RemoteMutationError::Worker(error.to_string()))??;

    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            payload,
        )?)
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_mutation(
    rooted: &RootedFs,
    path: RelativePath,
    kind: WireMutationKind,
    target: Option<PathBuf>,
    copy_source: Option<RelativePath>,
    expected_identity: Option<EntryIdentity>,
    modified: Option<Timestamp>,
    source_identity: Option<EntryIdentity>,
    entry_kind: Option<EntryKind>,
) -> Result<Bytes> {
    match kind {
        WireMutationKind::CreateDirectory => {
            let identity = rooted.create_directory_blocking(&path)?;
            return Ok(Bytes::copy_from_slice(identity.as_bytes()));
        }
        WireMutationKind::ReplaceSymlink => {
            let target = target.ok_or(RemoteMutationError::MissingSymlinkTarget)?;
            let expected = expected_identity.map_or(
                crate::endpoint::ExpectedDestination::Absent,
                crate::endpoint::ExpectedDestination::Unchanged,
            );
            return encode_publication_ack(
                rooted.publish_symlink_blocking(&path, &target, expected, modified)?,
            );
        }
        WireMutationKind::RemoveFileLike => {
            rooted.remove_blocking(&path, false, expected_identity)?
        }
        WireMutationKind::RemoveDirectory => {
            rooted.remove_blocking(&path, true, expected_identity)?
        }
        WireMutationKind::CopyFile => {
            let source = copy_source.ok_or(RemoteMutationError::MissingCopySource)?;
            let expected =
                expected_identity.ok_or(RemoteMutationError::MissingCopySourceIdentity)?;
            rooted.copy_file_blocking(&source, &path, expected)?;
        }
        WireMutationKind::Hardlink => {
            let source = copy_source.ok_or(RemoteMutationError::MissingCopySource)?;
            let identity = source_identity.ok_or(RemoteMutationError::MissingCopySourceIdentity)?;
            let expected = expected_identity.map_or(
                crate::endpoint::ExpectedDestination::Absent,
                crate::endpoint::ExpectedDestination::Unchanged,
            );
            return encode_publication_ack(
                rooted.publish_hardlink_blocking(&source, &path, identity, expected)?,
            );
        }
        WireMutationKind::VerifyPublication => {
            let identity = expected_identity.ok_or(RemoteMutationError::Protocol(
                ProtocolError::InvalidField {
                    field: "expected_identity",
                    reason: "publication verification requires identity",
                },
            ))?;
            let kind = entry_kind.ok_or(RemoteMutationError::InvalidAck)?;
            crate::rooted_fs::PublishedEntryProof {
                path,
                kind,
                identity,
            }
            .revalidate_blocking(rooted)?;
        }
    }
    Ok(Bytes::new())
}

fn wire_kind(kind: EntryKind) -> WireEntryKind {
    match kind {
        EntryKind::File => WireEntryKind::File,
        EntryKind::Symlink => WireEntryKind::Symlink,
        EntryKind::Directory => WireEntryKind::Directory,
    }
}

fn domain_kind(kind: WireEntryKind) -> EntryKind {
    match kind {
        WireEntryKind::File => EntryKind::File,
        WireEntryKind::Symlink => EntryKind::Symlink,
        WireEntryKind::Directory => EntryKind::Directory,
    }
}

// Publication acknowledgements are fixed-size typed proofs; the request binds
// their physical address. No path lookup on the requester can mint authority.
fn encode_publication_ack(proof: crate::rooted_fs::PublishedEntryProof) -> Result<Bytes> {
    let mut bytes = Vec::with_capacity(33);
    bytes.push(wire_kind(proof.kind) as u8);
    bytes.extend_from_slice(proof.identity.as_bytes());
    Ok(Bytes::from(bytes))
}

fn decode_publication_ack(
    path: &RelativePath,
    kind: EntryKind,
    bytes: &[u8],
) -> Result<crate::rooted_fs::PublishedEntryProof> {
    if bytes.len() != 33 || domain_kind(WireEntryKind::try_from(bytes[0])?) != kind {
        return Err(RemoteMutationError::InvalidAck);
    }
    let identity = bytes[1..]
        .try_into()
        .map_err(|_| RemoteMutationError::InvalidAck)?;
    Ok(crate::rooted_fs::PublishedEntryProof {
        path: path.clone(),
        kind,
        identity: EntryIdentity::from_bytes(identity),
    })
}

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<Bytes> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteMutationError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() {
        return Err(RemoteMutationError::InvalidAck);
    }
    Ok(frame.payload().clone())
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
            for _ in 0..6 {
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
        request_replace_symlink(
            &client.sender(),
            &link,
            Path::new("../target"),
            None,
            None,
            peer,
        )
        .await
        .unwrap();
        let link_id = crate::endpoint::local_identity::metadata_identity(
            &std::fs::symlink_metadata(root.path().join("link")).unwrap(),
            crate::engine::domain::EntryKind::Symlink,
        )
        .unwrap();
        let modified = Timestamp::new(1_600_000_000, 123_456_789).unwrap();
        request_replace_symlink(
            &client.sender(),
            &link,
            Path::new("../updated"),
            Some(link_id),
            Some(modified),
            peer,
        )
        .await
        .unwrap();
        let old = RelativePath::new("old").unwrap();
        let old_meta = std::fs::symlink_metadata(root.path().join("old")).unwrap();
        let old_id = crate::endpoint::local_identity::metadata_identity(
            &old_meta,
            crate::engine::domain::EntryKind::File,
        );
        request_remove(&client.sender(), &old, false, old_id, peer)
            .await
            .unwrap();
        // -H: the server links through held descriptors; the linked path
        // shares the source inode.
        std::fs::write(root.path().join("basis"), b"basis").unwrap();
        let basis = RelativePath::new("basis").unwrap();
        let linked = RelativePath::new("linked").unwrap();
        let basis_id = crate::endpoint::local_identity::metadata_identity(
            &std::fs::symlink_metadata(root.path().join("basis")).unwrap(),
            EntryKind::File,
        )
        .unwrap();
        request_hardlink(&client.sender(), &basis, &linked, basis_id, None, peer)
            .await
            .unwrap();
        let dir_meta = std::fs::symlink_metadata(root.path().join("dir")).unwrap();
        let dir_id = crate::endpoint::local_identity::metadata_identity(
            &dir_meta,
            crate::engine::domain::EntryKind::Directory,
        );
        request_remove(&client.sender(), &dir, true, dir_id, peer)
            .await
            .unwrap();
        server_task.await.unwrap();

        assert!(!root.path().join("old").exists());
        assert!(!root.path().join("dir").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let basis = std::fs::metadata(root.path().join("basis")).unwrap();
            let linked = std::fs::metadata(root.path().join("linked")).unwrap();
            assert_eq!(basis.ino(), linked.ino());
        }
        assert_eq!(
            std::fs::read_link(root.path().join("link")).unwrap(),
            Path::new("../updated")
        );
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(root.path().join("link")).unwrap();
        assert_eq!(metadata.mtime(), modified.seconds());
        assert_eq!(metadata.mtime_nsec(), i64::from(modified.nanoseconds()));
    }

    #[tokio::test]
    async fn remote_symlink_replace_preserves_racing_entries() {
        for update in [false, true] {
            for directory in [false, true] {
                let root = tempfile::TempDir::new().unwrap();
                let path = root.path().join("target");
                let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
                let expected = if update {
                    std::os::unix::fs::symlink("old", &path).unwrap();
                    let (_, identity) = rooted
                        .path_identity_blocking(&RelativePath::new("target").unwrap())
                        .unwrap()
                        .unwrap();
                    std::fs::remove_file(&path).unwrap();
                    Some(identity)
                } else {
                    None
                };
                if directory {
                    std::fs::create_dir(&path).unwrap();
                    std::fs::write(path.join("child"), b"raced").unwrap();
                } else {
                    std::fs::write(&path, b"raced").unwrap();
                }
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
                    let result =
                        serve_incoming_mutation_rooted(rooted, incoming, &sender, peer).await;
                    drop(server);
                    drop(sender);
                    result
                });
                assert!(request_replace_symlink(
                    &client.sender(),
                    &RelativePath::new("target").unwrap(),
                    Path::new("new"),
                    expected,
                    None,
                    peer
                )
                .await
                .is_err());
                assert!(matches!(
                    server_task.await.unwrap(),
                    Err(RemoteMutationError::RootedFs(
                        RootedFsError::DestinationChanged(_)
                    ))
                ));
                let retained = if directory { path.join("child") } else { path };
                assert_eq!(std::fs::read(retained).unwrap(), b"raced");
                assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
            }
        }
    }

    #[tokio::test]
    async fn remote_remove_validates_destination_identity() {
        let root = tempfile::TempDir::new().unwrap();
        let file_path = root.path().join("target");
        std::fs::write(&file_path, b"data").unwrap();

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
            let result = serve_incoming_mutation_rooted(rooted, incoming, &sender, peer).await;
            drop(server);
            drop(sender);
            result
        });

        let target = RelativePath::new("target").unwrap();

        // Mismatched identity fails and leaves file in place.
        let wrong_id = EntryIdentity::from_bytes([99; 32]);
        let err = request_remove(&client.sender(), &target, false, Some(wrong_id), peer).await;
        assert!(err.is_err());
        assert!(file_path.exists());

        let server_err = server_task.await.unwrap().unwrap_err();
        assert!(matches!(
            server_err,
            RemoteMutationError::RootedFs(RootedFsError::DestinationChanged(_))
        ));
    }
}
