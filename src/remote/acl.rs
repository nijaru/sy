//! Bounded access-control read/write RPCs and the shared preservation step
//! used by every v3 executor.
//!
//! `-A/--preserve-acls` is demand-driven: the scan never carries ACL bytes,
//! and the executor reads the source's list only for entries it is already
//! mutating (a create/update/replace or a metadata-only update). The wire
//! request carries the whole exacl unified-entries text in one frame so
//! mirror semantics (set the desired list, clear when empty) stay atomic per
//! entry, and an oversized list is refused loudly rather than truncated.
//!
//! Holds: the server serves ACL requests only in a Push session. In a Pull
//! session the remote root is the read-only source, so a write request is
//! rejected before it can mutate source data.

use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::engine::domain::{EntryKind, RelativePath};
use crate::protocol::{
    AclMode, Frame, FrameFlags, FrameKind, Operation, PlatformOs, ProtocolError, StreamId, WireAcl,
    WireAclRequest, WireAclResult, MAX_ACL_TEXT_BYTES,
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

    #[error("acl write requests are refused in a {0:?} session: the remote root is source-only")]
    WriteInSourceSession(Operation),

    #[error("local access-control access failed: {0}")]
    Local(String),
}

impl From<SharedRouterError> for RemoteAclError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteAclError>;

/// Where one side of an ACL preservation step lives. Local roots are
/// addressed through `LocalEndpoint`; a remote peer is addressed through the
/// session's bounded request handle.
pub enum AclLocation<'a> {
    Local(&'a Path),
    Remote(&'a ClientRemoteHandle),
}

/// Request the peer's current access-control list for one entry (`None` when
/// the entry carries no ACL).
pub async fn request_read_acls(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    peer: PlatformOs,
) -> Result<Option<String>> {
    ensure_compatible_path_encoding(peer)?;
    let request = WireAclRequest::read(encode_relative_path(path.as_path())?, wire_kind(kind))?;
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

/// Replace the peer's access-control list for one entry with `acl` (an empty
/// string clears the list). The acknowledgement is sent only after the mirror
/// completes.
pub async fn request_write_acls(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    acl: &str,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let request = WireAclRequest::write(
        encode_relative_path(path.as_path())?,
        wire_kind(kind),
        WireAcl::new(acl.to_string())?,
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
    receive_ack(&mut inbox, stream_id).await
}

pub async fn serve_incoming_acl_rooted(
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
    if frame.kind() != FrameKind::AclRequest {
        return Err(RemoteAclError::UnexpectedFrame {
            expected: FrameKind::AclRequest,
            actual: frame.kind(),
        });
    }
    let request = WireAclRequest::decode(frame.payload())?;
    let path = decode_relative_path(request.path().clone(), peer)?;
    let kind = domain_kind(request.kind());
    let mode = request.mode();
    let text = request
        .acl()
        .map(|acl| acl.text().to_string())
        .unwrap_or_default();
    drop(first);

    if mode == AclMode::Write && operation != Operation::Push {
        return Err(RemoteAclError::WriteInSourceSession(operation));
    }

    match mode {
        AclMode::Read => {
            let acl = tokio::task::spawn_blocking(move || rooted.read_acl_blocking(&path, kind))
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
        AclMode::Write => {
            tokio::task::spawn_blocking(move || rooted.write_acl_blocking(&path, kind, &text))
                .await
                .map_err(|error| RemoteAclError::Worker(error.to_string()))??;
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

/// Read the requested entry's access-control list from the source side of one
/// sync.
///
/// The text is bounded by the protocol cap so every direction (including the
/// local-only path, which never crosses the wire) fails loudly on an
/// oversized list instead of allocating without limit.
pub async fn read_preserved_acls(
    location: &AclLocation<'_>,
    path: &RelativePath,
    kind: EntryKind,
) -> Result<Option<String>> {
    let acl = match location {
        AclLocation::Local(root) => {
            let root = root.to_path_buf();
            LocalEndpoint::new(root)
                .read_acl(path.as_path())
                .await
                .map_err(|error| RemoteAclError::Local(error.to_string()))?
        }
        AclLocation::Remote(handle) => {
            request_read_acls(&handle.sender(), path, kind, handle.peer_platform()).await?
        }
    };
    check_acl_text_bounded(acl.as_deref().unwrap_or(""))?;
    Ok(acl)
}

/// Mirror a previously read access-control list onto the destination side. An
/// empty string clears the destination list (on Linux the mode-derived base
/// entries are restored, matching `LocalEndpoint::write_acl` and the legacy
/// executor's `unwrap_or_default`).
pub async fn apply_preserved_acls(
    location: &AclLocation<'_>,
    path: &RelativePath,
    kind: EntryKind,
    acl: &str,
) -> Result<()> {
    check_acl_text_bounded(acl)?;
    match location {
        AclLocation::Local(root) => {
            let root = root.to_path_buf();
            LocalEndpoint::new(root)
                .write_acl(path.as_path(), acl)
                .await
                .map_err(|error| RemoteAclError::Local(error.to_string()))
        }
        AclLocation::Remote(handle) => {
            request_write_acls(&handle.sender(), path, kind, acl, handle.peer_platform()).await
        }
    }
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
        operation: Operation,
        requests: usize,
    ) -> tokio::task::JoinHandle<()> {
        let sender = server.sender();
        tokio::spawn(async move {
            for _ in 0..requests {
                let incoming = server.incoming().recv().await.unwrap().unwrap();
                serve_incoming_acl_rooted(rooted.clone(), incoming, &sender, peer, operation)
                    .await
                    .unwrap();
            }
        })
    }

    fn planted_text() -> String {
        let uid = unsafe { libc::getuid() };
        exacl::to_string(&[exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ,
            exacl::Flag::empty(),
        )])
        .unwrap()
    }

    #[tokio::test]
    async fn acl_round_trip_reads_and_mirrors_write() {
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
        let text = planted_text();
        request_write_acls(&client.sender(), &path, EntryKind::File, &text, peer)
            .await
            .unwrap();
        let read = request_read_acls(&client.sender(), &path, EntryKind::File, peer)
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
            serve_incoming_acl_rooted(rooted, incoming, &sender, peer, Operation::Pull).await
        });

        let path = RelativePath::new("file").unwrap();
        // Send a raw write request because the fallible server closes the
        // stream without an acknowledgement, which the client surfaces as an
        // unexpected end rather than a clean RPC result.
        let request = WireAclRequest::write(
            encode_relative_path(path.as_path()).unwrap(),
            crate::protocol::WireEntryKind::File,
            WireAcl::new(String::new()).unwrap(),
        )
        .unwrap();
        let inbox = client.sender().open_stream().unwrap();
        let stream_id = inbox.stream_id();
        client
            .sender()
            .send(
                Frame::new(
                    FrameKind::AclRequest,
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
            RemoteAclError::WriteInSourceSession(Operation::Pull)
        ));
        // The source file keeps whatever list it had (the write never ran).
        assert!(exacl::getfacl(root.path().join("file"), None)
            .unwrap()
            .is_empty());
    }
}
