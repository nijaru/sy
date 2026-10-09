//! Bounded observation-bound source ACL reads. Destination preservation
//! belongs to the single observed metadata owner, never a path-only write.

use crate::endpoint::source_root::SourceRoot;
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireAcl, WireAclResult,
    WireSourceMetadataRead, MAX_ACL_TEXT_BYTES,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::remote::runtime::ClientRemoteHandle;
use crate::rooted_fs::{RootedFs, RootedFsError};
use bytes::Bytes;

#[derive(Debug, thiserror::Error)]
pub enum RemoteAclError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("acl worker failed: {0}")]
    Worker(String),

    #[error("acl stream {stream_id} ended before {expected:?}")]
    UnexpectedStreamEnd { stream_id: u32, expected: FrameKind },

    #[error("expected acl frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("acl frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("AclRequest must use FINAL and no other flags, got 0x{flags:02x}")]
    RequestFlags { flags: u8 },

    #[error("AclResult must use FINAL|ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    ResultFlags { flags: u8 },

    #[error("acl acknowledgement must be an empty unflagged frame")]
    InvalidAck,

    #[error("source preservation requires a scanned identity for {0}")]
    MissingSourceIdentity(RelativePath),
}

impl From<SharedRouterError> for RemoteAclError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteAclError>;

/// Source-side rooted observation or bounded remote request handle.
pub enum AclLocation<'a> {
    Local(&'a SourceRoot),
    Remote(&'a ClientRemoteHandle),
}

/// Request the peer's current access-control list for one entry (`None` when
/// the entry carries no ACL).
pub async fn request_read_acls(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    expected: EntryIdentity,
    peer: PlatformOs,
) -> Result<Option<String>> {
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
            FrameKind::AclRequest,
            FrameFlags::FINAL,
            stream_id,
            request.encode()?,
        )?)
        .await?;

    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteAclError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::AclResult,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    require_result_flags(frame)?;
    if frame.kind() != FrameKind::AclResult {
        return Err(RemoteAclError::UnexpectedFrame {
            expected: FrameKind::AclResult,
            actual: frame.kind(),
        });
    }
    let result = WireAclResult::decode(frame.payload())?;
    let text = result.acl().text().to_string();
    drop(routed);
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            Bytes::new(),
        )?)
        .await?;
    Ok(if text.is_empty() { None } else { Some(text) })
}

pub async fn serve_incoming_acl_rooted(
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
    if frame.kind() != FrameKind::AclRequest {
        return Err(RemoteAclError::UnexpectedFrame {
            expected: FrameKind::AclRequest,
            actual: frame.kind(),
        });
    }
    let request = WireSourceMetadataRead::decode(frame.payload())?;
    let path = decode_relative_path(request.path().clone(), peer)?;
    let kind = domain_kind(request.kind());
    let expected = EntryIdentity::from_bytes(request.identity());
    drop(first);
    let acl = tokio::task::spawn_blocking(move || {
        rooted.read_observed_acl_blocking(&path, kind, expected)
    })
    .await
    .map_err(|error| RemoteAclError::Worker(error.to_string()))??;
    let result = WireAclResult::new(WireAcl::new(acl.unwrap_or_default())?);
    sender
        .send(Frame::new(
            FrameKind::AclResult,
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            stream_id,
            result.encode()?,
        )?)
        .await?;
    receive_ack(&mut inbox, stream_id).await
}

/// Read the requested entry's access-control list from the source side of one
/// sync.
///
/// The text is bounded by the protocol cap so every direction (including the
/// local-only path, which never crosses the wire) fails loudly on an
/// oversized list instead of allocating without limit.
pub async fn read_preserved_acls(
    location: &AclLocation<'_>,
    source: &Entry,
) -> Result<Option<String>> {
    let expected = source
        .identity
        .ok_or_else(|| RemoteAclError::MissingSourceIdentity(source.path.clone()))?;
    let acl = match location {
        AclLocation::Local(root) => {
            let root = (*root).clone();
            let path = source.path.clone();
            let kind = source.kind;
            tokio::task::spawn_blocking(move || {
                root.validate_blocking()?;
                let value = root
                    .rooted()
                    .read_observed_acl_blocking(&path, kind, expected)?;
                root.validate_blocking()?;
                Ok::<_, RootedFsError>(value)
            })
            .await
            .map_err(|error| RemoteAclError::Worker(error.to_string()))??
        }
        AclLocation::Remote(handle) => {
            request_read_acls(
                &handle.sender(),
                &source.path,
                source.kind,
                expected,
                handle.peer_platform(),
            )
            .await?
        }
    };
    check_acl_text_bounded(acl.as_deref().unwrap_or(""))?;
    Ok(acl)
}

fn check_acl_text_bounded(acl: &str) -> Result<()> {
    if acl.len() > MAX_ACL_TEXT_BYTES {
        return Err(ProtocolError::AclTextTooLarge {
            len: acl.len(),
            max: MAX_ACL_TEXT_BYTES,
        }
        .into());
    }
    Ok(())
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
        .ok_or(RemoteAclError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::Ack,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() || !frame.payload().is_empty() {
        return Err(RemoteAclError::InvalidAck);
    }
    Ok(())
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteAclError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_request_flags(frame: &Frame) -> Result<()> {
    if frame.flags() == FrameFlags::FINAL {
        Ok(())
    } else {
        Err(RemoteAclError::RequestFlags {
            flags: frame.flags().bits(),
        })
    }
}

fn require_result_flags(frame: &Frame) -> Result<()> {
    let expected = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
    if frame.flags() == expected {
        Ok(())
    } else {
        Err(RemoteAclError::ResultFlags {
            flags: frame.flags().bits(),
        })
    }
}

#[cfg(all(test, unix, feature = "acl"))]
mod tests {
    use super::*;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};

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
                serve_incoming_acl_rooted(rooted.clone(), incoming, &sender, peer)
                    .await
                    .unwrap();
            }
        })
    }

    fn planted_text(file: &std::path::Path) -> String {
        // SAFETY: getuid has no arguments or caller obligations.
        let uid = unsafe { libc::getuid() };
        let mut entries = exacl::getfacl(file, None).unwrap();
        entries.push(exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        exacl::to_string(&entries).unwrap()
    }

    #[tokio::test]
    async fn observed_acl_read_returns_native_access_list() {
        let root = tempfile::TempDir::new().unwrap();
        let file_path = root.path().join("file");
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
        let server = FrameRouter::start(
            server_reader,
            server_writer,
            RouterRole::Server,
            RouterConfig::default(),
        )
        .unwrap();
        let peer = crate::protocol::Platform::current().os;
        let server_task = serve(server, rooted, peer, 1);

        let path = RelativePath::new("file").unwrap();
        let text = planted_text(&file_path);
        let entries = exacl::from_str(&text).unwrap();
        exacl::setfacl(&[&file_path], &entries, None).unwrap();
        let expected = crate::endpoint::local_identity::metadata_identity(
            &std::fs::metadata(&file_path).unwrap(),
            EntryKind::File,
        )
        .unwrap();
        let read = request_read_acls(&client.sender(), &path, EntryKind::File, expected, peer)
            .await
            .unwrap();
        server_task.await.unwrap();

        // The OS user database canonicalizes the numeric uid to its name on
        // read, so compare against exacl's own path read of the same file
        // rather than the planted literal.
        let via_path =
            exacl::to_string(&exacl::getfacl(root.path().join("file"), None).unwrap()).unwrap();
        assert!(!via_path.is_empty());
        assert_eq!(read.as_deref(), Some(via_path.as_str()));
    }
}
