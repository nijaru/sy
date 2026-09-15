//! Bounded extended-attribute read/write RPCs and the shared preservation
//! step used by every v3 executor.
//!
//! `-X/--preserve-xattrs` is demand-driven: the scan never carries attribute
//! bytes, and the executor reads the source's attributes only for entries it is
//! already mutating (a create/update/replace or a metadata-only update). The
//! wire request carries a whole bounded set in one frame so mirror semantics
//! (set the desired attributes, remove the rest) stay atomic per entry, and an
//! oversized set is refused loudly rather than truncated.
//!
//! Holds: the server serves xattr requests only in a Push session. In a Pull
//! session the remote root is the read-only source, so a write request is
//! rejected before it can mutate source data.

use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::engine::domain::{EntryKind, RelativePath};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, Operation, PlatformOs, ProtocolError, StreamId, WireEntryKind,
    WireXattr, WireXattrRequest, WireXattrResult, XattrMode,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::remote::runtime::ClientRemoteHandle;
use crate::rooted_fs::{RootedFs, RootedFsError};
use bytes::Bytes;
use std::ffi::OsString;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoteXattrError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("extended-attribute worker failed: {0}")]
    Worker(String),

    #[error("xattr stream {stream_id} ended before {expected:?}")]
    UnexpectedStreamEnd { stream_id: u32, expected: FrameKind },

    #[error("expected xattr frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("xattr frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("XattrRequest must use FINAL and no other flags, got 0x{flags:02x}")]
    RequestFlags { flags: u8 },

    #[error("XattrResult must use FINAL|ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    ResultFlags { flags: u8 },

    #[error("xattr acknowledgement must be an empty unflagged frame")]
    InvalidAck,

    #[error("xattr write requests are refused in a {0:?} session: the remote root is source-only")]
    WriteInSourceSession(Operation),

    #[error("local extended-attribute access failed: {0}")]
    Local(String),
}

impl From<SharedRouterError> for RemoteXattrError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteXattrError>;

/// Where one side of an xattr preservation step lives. Local roots are
/// addressed through `LocalEndpoint`; a remote peer is addressed through the
/// session's bounded request handle.
pub enum XattrLocation<'a> {
    Local(&'a Path),
    Remote(&'a ClientRemoteHandle),
}

/// Request the peer's current attributes for one entry.
pub async fn request_read_xattrs(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    peer: PlatformOs,
) -> Result<Vec<(OsString, Vec<u8>)>> {
    ensure_compatible_path_encoding(peer)?;
    let request = WireXattrRequest::read(encode_relative_path(path.as_path())?, wire_kind(kind))?;
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::XattrRequest,
            FrameFlags::FINAL,
            stream_id,
            request.encode()?,
        )?)
        .await?;

    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteXattrError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::XattrResult,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    require_result_flags(frame)?;
    if frame.kind() != FrameKind::XattrResult {
        return Err(RemoteXattrError::UnexpectedFrame {
            expected: FrameKind::XattrResult,
            actual: frame.kind(),
        });
    }
    let result = WireXattrResult::decode(frame.payload())?;
    let entries = result
        .entries()
        .iter()
        .map(|entry| (os_string_from_name(entry.name()), entry.value().to_vec()))
        .collect();
    drop(routed);
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            Bytes::new(),
        )?)
        .await?;
    Ok(entries)
}

/// Replace the peer's attributes for one entry with `xattrs` (an empty set
/// clears every attribute). The acknowledgement is sent only after the mirror
/// completes.
pub async fn request_write_xattrs(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    xattrs: &[(OsString, Vec<u8>)],
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let entries = xattrs
        .iter()
        .map(|(name, value)| WireXattr::new(name_bytes(name), value.clone()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let request = WireXattrRequest::write(
        encode_relative_path(path.as_path())?,
        wire_kind(kind),
        entries,
    )?;
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::XattrRequest,
            FrameFlags::FINAL,
            stream_id,
            request.encode()?,
        )?)
        .await?;
    receive_ack(&mut inbox, stream_id).await
}

pub async fn serve_incoming_xattr_rooted(
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
    if frame.kind() != FrameKind::XattrRequest {
        return Err(RemoteXattrError::UnexpectedFrame {
            expected: FrameKind::XattrRequest,
            actual: frame.kind(),
        });
    }
    let request = WireXattrRequest::decode(frame.payload())?;
    let path = decode_relative_path(request.path().clone(), peer)?;
    let kind = domain_kind(request.kind());
    let mode = request.mode();
    let entries = request.entries().to_vec();
    drop(first);

    if mode == XattrMode::Write && operation != Operation::Push {
        return Err(RemoteXattrError::WriteInSourceSession(operation));
    }

    match mode {
        XattrMode::Read => {
            let xattrs =
                tokio::task::spawn_blocking(move || rooted.read_xattrs_blocking(&path, kind))
                    .await
                    .map_err(|error| RemoteXattrError::Worker(error.to_string()))??;
            let wire = xattrs
                .iter()
                .map(|(name, value)| WireXattr::new(name_bytes(name), value.clone()))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let result = WireXattrResult::new(wire)?;
            sender
                .send(Frame::new(
                    FrameKind::XattrResult,
                    FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                    stream_id,
                    result.encode()?,
                )?)
                .await?;
            receive_ack(&mut inbox, stream_id).await
        }
        XattrMode::Write => {
            let xattrs = entries
                .iter()
                .map(|entry| (os_string_from_name(entry.name()), entry.value().to_vec()))
                .collect::<Vec<_>>();
            tokio::task::spawn_blocking(move || rooted.write_xattrs_blocking(&path, kind, &xattrs))
                .await
                .map_err(|error| RemoteXattrError::Worker(error.to_string()))??;
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

/// Read the requested entry's attributes from the source side of one sync.
///
/// The set is bounded by the protocol cap so every direction (including the
/// local-only path, which never crosses the wire) fails loudly on an oversized
/// set instead of allocating without limit.
pub async fn read_preserved_xattrs(
    location: &XattrLocation<'_>,
    path: &RelativePath,
    kind: EntryKind,
) -> Result<Vec<(OsString, Vec<u8>)>> {
    let xattrs = match location {
        XattrLocation::Local(root) => {
            let root = root.to_path_buf();
            LocalEndpoint::new(root)
                .read_xattrs(path.as_path())
                .await
                .map_err(|error| RemoteXattrError::Local(error.to_string()))?
        }
        XattrLocation::Remote(handle) => {
            request_read_xattrs(&handle.sender(), path, kind, handle.peer_platform()).await?
        }
    };
    check_xattr_set_bounded(&xattrs)?;
    Ok(xattrs)
}

/// Mirror a previously read attribute set onto the destination side.
pub async fn apply_preserved_xattrs(
    location: &XattrLocation<'_>,
    path: &RelativePath,
    kind: EntryKind,
    xattrs: &[(OsString, Vec<u8>)],
) -> Result<()> {
    check_xattr_set_bounded(xattrs)?;
    match location {
        XattrLocation::Local(root) => {
            let root = root.to_path_buf();
            LocalEndpoint::new(root)
                .write_xattrs(path.as_path(), xattrs)
                .await
                .map_err(|error| RemoteXattrError::Local(error.to_string()))
        }
        XattrLocation::Remote(handle) => {
            request_write_xattrs(&handle.sender(), path, kind, xattrs, handle.peer_platform()).await
        }
    }
}

fn check_xattr_set_bounded(xattrs: &[(OsString, Vec<u8>)]) -> Result<()> {
    let mut total = 0_usize;
    for (name, value) in xattrs {
        total = total
            .checked_add(name_bytes(name).len())
            .and_then(|value_total| value_total.checked_add(value.len()))
            .ok_or(ProtocolError::XattrPayloadTooLarge {
                len: usize::MAX,
                max: crate::protocol::MAX_XATTR_TOTAL_BYTES,
            })?;
        // Validate each name through the same constructor the wire uses so
        // empty/NUL/oversized names fail identically on every direction.
        WireXattr::new(name_bytes(name), Bytes::new())?;
    }
    if total > crate::protocol::MAX_XATTR_TOTAL_BYTES {
        return Err(ProtocolError::XattrPayloadTooLarge {
            len: total,
            max: crate::protocol::MAX_XATTR_TOTAL_BYTES,
        }
        .into());
    }
    Ok(())
}

#[cfg(unix)]
fn name_bytes(name: &OsString) -> Bytes {
    use std::os::unix::ffi::OsStrExt;
    Bytes::copy_from_slice(name.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn name_bytes(name: &OsString) -> Bytes {
    Bytes::copy_from_slice(name.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn os_string_from_name(name: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(name.to_vec())
}

#[cfg(not(unix))]
fn os_string_from_name(name: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(name).into_owned())
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

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteXattrError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::Ack,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    if frame.kind() != FrameKind::Ack || !frame.flags().is_empty() || !frame.payload().is_empty() {
        return Err(RemoteXattrError::InvalidAck);
    }
    Ok(())
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteXattrError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_request_flags(frame: &Frame) -> Result<()> {
    if frame.flags() == FrameFlags::FINAL {
        Ok(())
    } else {
        Err(RemoteXattrError::RequestFlags {
            flags: frame.flags().bits(),
        })
    }
}

fn require_result_flags(frame: &Frame) -> Result<()> {
    let expected = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
    if frame.flags() == expected {
        Ok(())
    } else {
        Err(RemoteXattrError::ResultFlags {
            flags: frame.flags().bits(),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};

    /// Keep only the caller's attribute namespace. macOS attaches
    /// `com.apple.provenance` to freshly written files, so a read-back must not
    /// be compared against exactly the set the caller wrote.
    fn user_xattrs(xattrs: Vec<(OsString, Vec<u8>)>) -> Vec<(OsString, Vec<u8>)> {
        xattrs
            .into_iter()
            .filter(|(name, _)| name.to_string_lossy().starts_with("user."))
            .collect()
    }

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
                serve_incoming_xattr_rooted(rooted.clone(), incoming, &sender, peer, operation)
                    .await
                    .unwrap();
            }
        })
    }

    #[tokio::test]
    async fn xattr_round_trip_reads_and_mirrors_write() {
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
        let name = OsString::from("user.sy-test");
        request_write_xattrs(
            &client.sender(),
            &path,
            EntryKind::File,
            &[(name.clone(), b"wire".to_vec())],
            peer,
        )
        .await
        .unwrap();
        let read = request_read_xattrs(&client.sender(), &path, EntryKind::File, peer)
            .await
            .unwrap();
        server_task.await.unwrap();

        assert_eq!(user_xattrs(read), vec![(name, b"wire".to_vec())]);
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
            serve_incoming_xattr_rooted(rooted, incoming, &sender, peer, Operation::Pull).await
        });

        let path = RelativePath::new("file").unwrap();
        // Send a raw write request because the fallible server closes the
        // stream without an acknowledgement, which the client surfaces as an
        // unexpected end rather than a clean RPC result.
        let request = WireXattrRequest::write(
            encode_relative_path(path.as_path()).unwrap(),
            WireEntryKind::File,
            vec![WireXattr::new(
                Bytes::from_static(b"user.sy-test"),
                Bytes::from_static(b"x"),
            )
            .unwrap()],
        )
        .unwrap();
        let inbox = client.sender().open_stream().unwrap();
        let stream_id = inbox.stream_id();
        client
            .sender()
            .send(
                Frame::new(
                    FrameKind::XattrRequest,
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
            RemoteXattrError::WriteInSourceSession(Operation::Pull)
        ));
        // The source file was never mutated.
        assert!(xattr::get(root.path().join("file"), "user.sy-test")
            .unwrap()
            .is_none());
    }
}
