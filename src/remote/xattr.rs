//! Bounded demand-driven source xattr reads, bound to scanned observations.
//! Destination writes belong to the single observed metadata owner.
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireEntryKind,
    WireSourceMetadataRead, WireXattr, WireXattrResult,
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

    #[error("source preservation requires a scanned identity for {0}")]
    MissingSourceIdentity(RelativePath),
}

impl From<SharedRouterError> for RemoteXattrError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteXattrError>;

/// Source-side rooted observation or bounded remote request handle.
pub enum XattrLocation<'a> {
    Local(&'a Path),
    Remote(&'a ClientRemoteHandle),
}

/// Request the peer's current attributes for one entry.
pub async fn request_read_xattrs(
    sender: &RouterSender,
    path: &RelativePath,
    kind: EntryKind,
    expected: EntryIdentity,
    peer: PlatformOs,
) -> Result<Vec<(OsString, Vec<u8>)>> {
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

pub async fn serve_incoming_xattr_rooted(
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
    if frame.kind() != FrameKind::XattrRequest {
        return Err(RemoteXattrError::UnexpectedFrame {
            expected: FrameKind::XattrRequest,
            actual: frame.kind(),
        });
    }
    let request = WireSourceMetadataRead::decode(frame.payload())?;
    let path = decode_relative_path(request.path().clone(), peer)?;
    let kind = domain_kind(request.kind());
    let expected = EntryIdentity::from_bytes(request.identity());
    drop(first);
    let xattrs = tokio::task::spawn_blocking(move || {
        rooted.read_observed_xattrs_blocking(&path, kind, expected)
    })
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

/// Read the requested entry's attributes from the source side of one sync.
///
/// The set is bounded by the protocol cap so every direction (including the
/// local-only path, which never crosses the wire) fails loudly on an oversized
/// set instead of allocating without limit.
pub async fn read_preserved_xattrs(
    location: &XattrLocation<'_>,
    source: &Entry,
) -> Result<Vec<(OsString, Vec<u8>)>> {
    let expected = source
        .identity
        .ok_or_else(|| RemoteXattrError::MissingSourceIdentity(source.path.clone()))?;
    let xattrs = match location {
        XattrLocation::Local(root) => {
            let root = root.to_path_buf();
            let path = source.path.clone();
            let kind = source.kind;
            tokio::task::spawn_blocking(move || {
                RootedFs::open_blocking_for_worker(root)?
                    .read_observed_xattrs_blocking(&path, kind, expected)
            })
            .await
            .map_err(|error| RemoteXattrError::Worker(error.to_string()))??
        }
        XattrLocation::Remote(handle) => {
            request_read_xattrs(
                &handle.sender(),
                &source.path,
                source.kind,
                expected,
                handle.peer_platform(),
            )
            .await?
        }
    };
    check_xattr_set_bounded(&xattrs)?;
    Ok(xattrs)
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
pub(crate) fn name_bytes(name: &OsString) -> Bytes {
    use std::os::unix::ffi::OsStrExt;
    Bytes::copy_from_slice(name.as_os_str().as_bytes())
}

#[cfg(not(unix))]
pub(crate) fn name_bytes(name: &OsString) -> Bytes {
    Bytes::copy_from_slice(name.to_string_lossy().as_bytes())
}

#[cfg(unix)]
pub(crate) fn os_string_from_name(name: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(name.to_vec())
}

#[cfg(not(unix))]
pub(crate) fn os_string_from_name(name: &[u8]) -> OsString {
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
        requests: usize,
    ) -> tokio::task::JoinHandle<()> {
        let sender = server.sender();
        tokio::spawn(async move {
            for _ in 0..requests {
                let incoming = server.incoming().recv().await.unwrap().unwrap();
                serve_incoming_xattr_rooted(rooted.clone(), incoming, &sender, peer)
                    .await
                    .unwrap();
            }
        })
    }

    #[tokio::test]
    async fn observed_xattr_read_returns_native_attributes() {
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
        let server_task = serve(server, rooted, peer, 1);

        let path = RelativePath::new("file").unwrap();
        let name = OsString::from("user.sy-test");
        xattr::set(root.path().join("file"), &name, b"wire").unwrap();
        let expected = crate::endpoint::local_identity::metadata_identity(
            &std::fs::metadata(root.path().join("file")).unwrap(),
            EntryKind::File,
        )
        .unwrap();
        let read = request_read_xattrs(&client.sender(), &path, EntryKind::File, expected, peer)
            .await
            .unwrap();
        server_task.await.unwrap();

        assert_eq!(user_xattrs(read), vec![(name, b"wire".to_vec())]);
    }
}
