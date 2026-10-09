use crate::engine::domain::{EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireAcl, WireEntryKind,
    WireMetadata, WireMetadataTarget, WireXattr, WireXattrResult,
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

    #[error("metadata acknowledgement must be an unflagged observed identity")]
    InvalidAck,
}

impl From<SharedRouterError> for RemoteMetadataError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteMetadataError>;

pub(crate) fn observed_metadata(
    path: &RelativePath,
    kind: EntryKind,
    expected: EntryIdentity,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
    preservation: &crate::rooted_fs::MetadataPreservation<'_>,
) -> Result<WireMetadata> {
    preservation.validate()?;
    let xattrs = preservation
        .xattrs
        .map(|entries| {
            entries
                .iter()
                .map(|(name, value)| {
                    WireXattr::new(crate::remote::xattr::name_bytes(name), value.clone())
                })
                .collect::<crate::protocol::Result<Vec<_>>>()
                .and_then(WireXattrResult::new)
        })
        .transpose()?;
    Ok(WireMetadata::new(
        encode_relative_path(path.as_path())?,
        WireMetadataTarget::Observed {
            kind: wire_kind(kind),
            identity: *expected.as_bytes(),
        },
        unix_mode,
        modified.map(|value| (value.seconds(), value.nanoseconds())),
        xattrs,
        preservation
            .acl
            .map(|acl| WireAcl::new(acl.to_owned()))
            .transpose()?,
        preservation.bsd_flags,
    )?)
}

pub async fn request_metadata(
    sender: &RouterSender,
    metadata: WireMetadata,
    peer: PlatformOs,
) -> Result<EntryIdentity> {
    ensure_compatible_path_encoding(peer)?;
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
    if incoming.first.frame().payload().first().is_some_and(|tag| {
        *tag == crate::protocol::DIRECTORY_READ || *tag == crate::protocol::DIRECTORY_FINALIZE
    }) {
        return super::directory::serve(rooted, incoming, sender, peer).await;
    }
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
    let target = metadata.target();
    let unix_mode = metadata.unix_mode();
    let xattrs = metadata.xattrs().map(|attrs| {
        attrs
            .entries()
            .iter()
            .map(|entry| {
                (
                    crate::remote::xattr::os_string_from_name(entry.name()),
                    entry.value().to_vec(),
                )
            })
            .collect::<Vec<_>>()
    });
    let acl = metadata.acl().map(|acl| acl.text().to_owned());
    let bsd_flags = metadata.bsd_flags();
    let modified = metadata
        .modified()
        .map(|(seconds, nanoseconds)| Timestamp::new(seconds, nanoseconds))
        .transpose()
        .map_err(|_| ProtocolError::InvalidField {
            field: "modified_nanoseconds",
            reason: "nanoseconds must be below 1,000,000,000",
        })?;
    drop(first);

    let identity = tokio::task::spawn_blocking(move || match target {
        WireMetadataTarget::Observed { kind, identity } => rooted
            .apply_observed_preservation_blocking(
                &relative,
                domain_kind(kind),
                EntryIdentity::from_bytes(identity),
                unix_mode,
                modified,
                &crate::rooted_fs::MetadataPreservation {
                    xattrs: xattrs.as_deref(),
                    acl: acl.as_deref(),
                    bsd_flags,
                },
            ),
    })
    .await
    .map_err(|error| RemoteMetadataError::Worker(error.to_string()))??;

    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            Bytes::copy_from_slice(identity.as_bytes()),
        )?)
        .await?;
    Ok(())
}

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<EntryIdentity> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteMetadataError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() {
        return Err(RemoteMetadataError::InvalidAck);
    }
    let identity = frame
        .payload()
        .as_ref()
        .try_into()
        .map_err(|_| RemoteMetadataError::InvalidAck)?;
    Ok(EntryIdentity::from_bytes(identity))
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
    async fn observed_preservation_rpc_refuses_foreign_destination_and_closed_admission_before_any_field(
    ) {
        for close_admission in [false, true] {
            let source = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let source_path = source.path().join("file");
            let destination_path = destination.path().join("file");
            std::fs::write(&source_path, b"source").unwrap();
            std::fs::write(&destination_path, b"target").unwrap();
            let path = RelativePath::new("file").unwrap();
            let mut rooted = RootedFs::open(destination.path().to_path_buf())
                .await
                .unwrap();
            let expected = rooted.path_identity_blocking(&path).unwrap().unwrap().1;
            let attrs = vec![(
                std::ffi::OsString::from("user.sy-owner"),
                b"preserved".to_vec(),
            )];
            #[cfg(feature = "acl")]
            let acl = Some(exacl::to_string(&exacl::getfacl(&source_path, None).unwrap()).unwrap());
            #[cfg(not(feature = "acl"))]
            let acl: Option<String> = None;
            #[cfg(target_os = "macos")]
            let bsd_flags = Some(libc::UF_NODUMP);
            #[cfg(not(target_os = "macos"))]
            let bsd_flags = None;
            let metadata = observed_metadata(
                &path,
                EntryKind::File,
                expected,
                Some(0o600),
                Some(Timestamp::UNIX_EPOCH),
                &crate::rooted_fs::MetadataPreservation {
                    xattrs: Some(&attrs),
                    acl: acl.as_deref(),
                    bsd_flags,
                },
            )
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
            let admission = sender.publication_admission();
            rooted.bind_session_mutations(admission.clone(), false);
            let peer = crate::protocol::Platform::current().os;
            let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let incoming = server.incoming().recv().await.unwrap().unwrap();
                arrived_tx.send(()).unwrap();
                resume_rx.await.unwrap();
                serve_incoming_metadata_rooted(rooted, incoming, &sender, peer).await
            });
            let inbox = client.sender().open_stream().unwrap();
            client
                .sender()
                .send(
                    Frame::new(
                        FrameKind::Metadata,
                        FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                        inbox.stream_id(),
                        metadata.encode().unwrap(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), arrived_rx)
                .await
                .unwrap()
                .unwrap();
            if close_admission {
                admission.close();
            } else {
                std::fs::rename(&destination_path, destination.path().join("observed")).unwrap();
                std::fs::write(&destination_path, b"foreign").unwrap();
            }
            let before = crate::endpoint::local_identity::metadata_identity(
                &std::fs::metadata(&destination_path).unwrap(),
                EntryKind::File,
            );
            resume_tx.send(()).unwrap();
            let result = task.await.unwrap();
            assert!(
                if close_admission {
                    matches!(
                        result,
                        Err(RemoteMetadataError::RootedFs(
                            RootedFsError::CommitCancelled
                        ))
                    )
                } else {
                    matches!(
                        result,
                        Err(RemoteMetadataError::RootedFs(
                            RootedFsError::DestinationChanged(_)
                        ))
                    )
                },
                "{result:?}"
            );
            assert_eq!(
                crate::endpoint::local_identity::metadata_identity(
                    &std::fs::metadata(&destination_path).unwrap(),
                    EntryKind::File
                ),
                before
            );
            assert_eq!(
                std::fs::read(&destination_path).unwrap(),
                if close_admission {
                    b"target".as_slice()
                } else {
                    b"foreign".as_slice()
                }
            );
            assert!(xattr::get(&destination_path, "user.sy-owner")
                .unwrap()
                .is_none());
            assert_eq!(std::fs::read(&source_path).unwrap(), b"source");
        }
    }

    #[tokio::test]
    async fn metadata_stream_applies_fields_and_acks() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let expected = rooted
            .path_identity_blocking(&RelativePath::new("file").unwrap())
            .unwrap()
            .unwrap()
            .1;
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
        let attrs = vec![(std::ffi::OsString::from("user.sy-owner"), b"value".to_vec())];
        #[cfg(feature = "acl")]
        let acl = Some(
            exacl::to_string(&exacl::getfacl(root.path().join("file"), None).unwrap()).unwrap(),
        );
        #[cfg(not(feature = "acl"))]
        let acl: Option<String> = None;
        #[cfg(target_os = "macos")]
        let bsd_flags = Some(libc::UF_NODUMP);
        #[cfg(not(target_os = "macos"))]
        let bsd_flags = None;
        let identity = request_metadata(
            &client.sender(),
            observed_metadata(
                &path,
                EntryKind::File,
                expected,
                Some(0o640),
                Some(modified),
                &crate::rooted_fs::MetadataPreservation {
                    xattrs: Some(&attrs),
                    acl: acl.as_deref(),
                    bsd_flags,
                },
            )
            .unwrap(),
            peer,
        )
        .await
        .unwrap();
        server_task.await.unwrap();

        let metadata = std::fs::metadata(root.path().join("file")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o640);
        assert_eq!(metadata.mtime(), modified.seconds());
        assert_eq!(
            crate::endpoint::local_identity::metadata_identity(&metadata, EntryKind::File),
            Some(identity)
        );
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"data");
        assert_eq!(
            xattr::get(root.path().join("file"), "user.sy-owner").unwrap(),
            Some(b"value".to_vec())
        );
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            assert_eq!(metadata.st_flags(), libc::UF_NODUMP);
        }
    }
}
