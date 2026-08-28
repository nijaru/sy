use crate::engine::domain::{
    Entry, EntryIdentity, EntryKind, InvalidRelativePath, InvalidTimestamp, RelativePath, Timestamp,
};
use crate::engine::reconcile::{BoxError, EntryStream};
use crate::engine::scan::{EntryMetadataRequest, ScanRequest};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, Platform, PlatformOs, ProtocolError, RelativeWirePath, StreamId,
    WireEntry, WireEntryKind, WirePath, WireScanRequest,
};
use crate::remote_router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use futures::StreamExt;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum RemoteScanError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    InvalidRelativePath(#[from] InvalidRelativePath),

    #[error(transparent)]
    InvalidTimestamp(#[from] InvalidTimestamp),

    #[error("scan stream id must be non-zero")]
    ControlStream,

    #[error("expected scan frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("scan frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("scan frame {kind:?} used unsupported flags 0x{flags:02x}")]
    FrameFlags { kind: FrameKind, flags: u8 },

    #[error("EntryEnd must use ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    EntryEndFlags { flags: u8 },

    #[error("EntryEnd payload must be empty")]
    NonEmptyEntryEnd,

    #[error("scan acknowledgement payload must be empty")]
    NonEmptyAck,

    #[error("scan stream {stream_id} ended before {expected:?}")]
    UnexpectedStreamEnd { stream_id: u32, expected: FrameKind },

    #[error("scan depth {0} exceeds protocol u32 range")]
    DepthTooLarge(usize),

    #[error("wire scan depth {0} cannot be represented by this target")]
    UnsupportedDepth(u32),

    #[error(
        "peer path encoding {peer:?} cannot preserve local ordered-path semantics on {local:?}"
    )]
    UnsupportedPathEncoding { local: PlatformOs, peer: PlatformOs },

    #[error("wire path component is not one native relative-name component")]
    InvalidPathComponent,

    #[error("wire path contains a NUL code unit")]
    PathContainsNul,

    #[error("local metadata scan failed")]
    LocalScan(#[source] BoxError),
}

impl From<SharedRouterError> for RemoteScanError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteScanError>;

/// Open a locally initiated scan stream and expose its ordered metadata directly
/// as the engine's `EntryStream`.
///
/// The caller never reads the transport directly. The central frame router owns
/// transport I/O and keeps all active streams under one bounded memory budget.
pub async fn request_scan(
    sender: &RouterSender,
    request: ScanRequest,
    peer: PlatformOs,
) -> Result<EntryStream> {
    ensure_compatible_path_encoding(peer)?;
    let inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    require_data_stream(stream_id)?;

    let wire = scan_request_to_wire(request)?;
    let frame = Frame::new(
        FrameKind::ScanRequest,
        FrameFlags::empty(),
        stream_id,
        wire.encode(),
    )?;
    sender.send(frame).await?;

    remote_entry_stream(inbox, sender.clone(), peer)
}

/// Serve one peer-opened scan stream.
///
/// `EntryEnd` is an acknowledged stream boundary. Keeping the peer inbox alive
/// until its ACK arrives prevents a short-lived server/session from tearing down
/// the router while the final metadata frames are still queued for transport.
pub async fn serve_incoming_scan(
    root: &Path,
    incoming: IncomingStream,
    sender: &RouterSender,
) -> Result<()> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream_id = inbox.stream_id();
    let first_frame = first.frame();
    require_stream(first_frame, stream_id)?;
    let request = decode_scan_request(first_frame)?;
    drop(first);

    serve_scan(root, request, sender, stream_id).await?;
    receive_scan_ack(&mut inbox, stream_id).await
}

async fn serve_scan(
    root: &Path,
    request: ScanRequest,
    sender: &RouterSender,
    stream_id: StreamId,
) -> Result<()> {
    require_data_stream(stream_id)?;
    let mut entries =
        crate::endpoint::local_entry_scan::local_entry_stream(root.to_path_buf(), request);

    while let Some(entry) = entries.next().await {
        let entry = entry.map_err(RemoteScanError::LocalScan)?;
        let wire = entry_to_wire(&entry)?;
        let frame = Frame::new(
            FrameKind::Entry,
            FrameFlags::empty(),
            stream_id,
            wire.encode()?,
        )?;
        sender.send(frame).await?;
    }

    let end = Frame::new(
        FrameKind::EntryEnd,
        FrameFlags::ACK_REQUIRED,
        stream_id,
        bytes::Bytes::new(),
    )?;
    sender.send(end).await?;
    Ok(())
}

async fn receive_scan_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteScanError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::Ack,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    require_empty_flags(frame)?;
    if frame.kind() != FrameKind::Ack {
        return Err(RemoteScanError::UnexpectedFrame {
            expected: FrameKind::Ack,
            actual: frame.kind(),
        });
    }
    if !frame.payload().is_empty() {
        return Err(RemoteScanError::NonEmptyAck);
    }
    Ok(())
}

fn decode_scan_request(frame: &Frame) -> Result<ScanRequest> {
    require_data_stream(frame.stream_id())?;
    require_empty_flags(frame)?;
    if frame.kind() != FrameKind::ScanRequest {
        return Err(RemoteScanError::UnexpectedFrame {
            expected: FrameKind::ScanRequest,
            actual: frame.kind(),
        });
    }
    wire_to_scan_request(WireScanRequest::decode(frame.payload())?)
}

/// Convert one routed metadata inbox into the engine's ordered entry stream.
///
/// Frames release their router permits immediately after conversion. `EntryEnd`
/// is acknowledged only after every earlier entry has been yielded to the
/// consumer, making the ACK a meaningful ordered-stream completion boundary.
fn remote_entry_stream(
    inbox: StreamInbox,
    sender: RouterSender,
    peer: PlatformOs,
) -> Result<EntryStream> {
    let stream_id = inbox.stream_id();
    require_data_stream(stream_id)?;
    ensure_compatible_path_encoding(peer)?;

    let stream =
        futures::stream::try_unfold((inbox, sender), move |(mut inbox, sender)| async move {
            let routed = inbox
                .recv()
                .await?
                .ok_or(RemoteScanError::UnexpectedStreamEnd {
                    stream_id: stream_id.get(),
                    expected: FrameKind::EntryEnd,
                })?;
            let frame = routed.frame();
            require_stream(frame, stream_id)?;

            match frame.kind() {
                FrameKind::Entry => {
                    require_empty_flags(frame)?;
                    let wire = WireEntry::decode(frame.payload())?;
                    let entry = wire_to_entry(wire, peer)?;
                    Ok(Some((entry, (inbox, sender))))
                }
                FrameKind::EntryEnd => {
                    require_entry_end_flags(frame)?;
                    if !frame.payload().is_empty() {
                        return Err(RemoteScanError::NonEmptyEntryEnd);
                    }
                    let ack = Frame::new(
                        FrameKind::Ack,
                        FrameFlags::empty(),
                        stream_id,
                        bytes::Bytes::new(),
                    )?;
                    sender.send(ack).await?;
                    Ok(None)
                }
                actual => Err(RemoteScanError::UnexpectedFrame {
                    expected: FrameKind::Entry,
                    actual,
                }),
            }
        })
        .map(|result| result.map_err(|error| Box::new(error) as BoxError));

    Ok(Box::pin(stream))
}

fn scan_request_to_wire(request: ScanRequest) -> Result<WireScanRequest> {
    let max_depth = request
        .max_depth
        .map(|depth| u32::try_from(depth).map_err(|_| RemoteScanError::DepthTooLarge(depth)))
        .transpose()?;
    Ok(WireScanRequest {
        respect_gitignore: request.respect_gitignore,
        include_git_dir: request.include_git_dir,
        max_depth,
        unix_mode: request.metadata.unix_mode,
        symlink_target: request.metadata.symlink_target,
        identity: request.metadata.identity,
        hardlink_group: request.metadata.hardlink_group,
    })
}

fn wire_to_scan_request(request: WireScanRequest) -> Result<ScanRequest> {
    let max_depth = request
        .max_depth
        .map(|depth| usize::try_from(depth).map_err(|_| RemoteScanError::UnsupportedDepth(depth)))
        .transpose()?;
    Ok(ScanRequest {
        respect_gitignore: request.respect_gitignore,
        include_git_dir: request.include_git_dir,
        max_depth,
        metadata: EntryMetadataRequest {
            unix_mode: request.unix_mode,
            symlink_target: request.symlink_target,
            identity: request.identity,
            hardlink_group: request.hardlink_group,
        },
    })
}

fn entry_to_wire(entry: &Entry) -> Result<WireEntry> {
    Ok(WireEntry {
        path: encode_relative_path(entry.path.as_path())?,
        kind: match entry.kind {
            EntryKind::File => WireEntryKind::File,
            EntryKind::Directory => WireEntryKind::Directory,
            EntryKind::Symlink => WireEntryKind::Symlink,
        },
        size: entry.size,
        modified_seconds: entry.modified.seconds(),
        modified_nanoseconds: entry.modified.nanoseconds(),
        unix_mode: entry.unix_mode,
        identity: entry.identity.map(|identity| *identity.as_bytes()),
        hardlink_group: entry.hardlink_group.map(|group| *group.as_bytes()),
        symlink_target: entry
            .symlink_target
            .as_deref()
            .map(encode_native_path)
            .transpose()?,
    })
}

fn wire_to_entry(entry: WireEntry, peer: PlatformOs) -> Result<Entry> {
    ensure_compatible_path_encoding(peer)?;
    let path = decode_relative_path(entry.path, peer)?;
    let modified = Timestamp::new(entry.modified_seconds, entry.modified_nanoseconds)?;
    let symlink_target = entry
        .symlink_target
        .map(|target| decode_native_path(target, peer))
        .transpose()?;

    Ok(Entry {
        path,
        kind: match entry.kind {
            WireEntryKind::File => EntryKind::File,
            WireEntryKind::Directory => EntryKind::Directory,
            WireEntryKind::Symlink => EntryKind::Symlink,
        },
        size: entry.size,
        modified,
        unix_mode: entry.unix_mode,
        symlink_target,
        identity: entry.identity.map(EntryIdentity::from_bytes),
        hardlink_group: entry.hardlink_group.map(EntryIdentity::from_bytes),
    })
}

fn require_data_stream(stream_id: StreamId) -> Result<()> {
    if stream_id.is_control() {
        Err(RemoteScanError::ControlStream)
    } else {
        Ok(())
    }
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteScanError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_empty_flags(frame: &Frame) -> Result<()> {
    if frame.flags().is_empty() {
        Ok(())
    } else {
        Err(RemoteScanError::FrameFlags {
            kind: frame.kind(),
            flags: frame.flags().bits(),
        })
    }
}

fn require_entry_end_flags(frame: &Frame) -> Result<()> {
    if frame.flags() == FrameFlags::ACK_REQUIRED {
        Ok(())
    } else {
        Err(RemoteScanError::EntryEndFlags {
            flags: frame.flags().bits(),
        })
    }
}

fn ensure_compatible_path_encoding(peer: PlatformOs) -> Result<()> {
    let local = Platform::current().os;
    if compatible_path_encoding(local, peer) {
        Ok(())
    } else {
        Err(RemoteScanError::UnsupportedPathEncoding { local, peer })
    }
}

fn compatible_path_encoding(local: PlatformOs, peer: PlatformOs) -> bool {
    matches!(
        (local, peer),
        (
            PlatformOs::Linux | PlatformOs::Macos,
            PlatformOs::Linux | PlatformOs::Macos
        ) | (PlatformOs::Windows, PlatformOs::Windows)
    )
}

#[cfg(unix)]
fn encode_relative_path(path: &Path) -> Result<RelativeWirePath> {
    use std::os::unix::ffi::OsStrExt;

    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RemoteScanError::InvalidPathComponent);
    }
    RelativeWirePath::from_components(path.components().filter_map(|component| match component {
        Component::Normal(name) => Some(name.as_bytes()),
        _ => None,
    }))
    .map_err(RemoteScanError::from)
}

#[cfg(windows)]
fn encode_relative_path(path: &Path) -> Result<RelativeWirePath> {
    use std::os::windows::ffi::OsStrExt;

    let mut encoded = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(RemoteScanError::InvalidPathComponent);
        };
        let mut bytes = Vec::new();
        for unit in name.encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        encoded.push(bytes);
    }
    RelativeWirePath::from_components(encoded).map_err(RemoteScanError::from)
}

#[cfg(not(any(unix, windows)))]
fn encode_relative_path(_path: &Path) -> Result<RelativeWirePath> {
    Err(RemoteScanError::UnsupportedPathEncoding {
        local: Platform::current().os,
        peer: Platform::current().os,
    })
}

fn decode_relative_path(path: RelativeWirePath, peer: PlatformOs) -> Result<RelativePath> {
    let mut native = PathBuf::new();
    for component in path.components() {
        let name = decode_native_component(component, peer)?;
        validate_native_component(&name)?;
        native.push(name);
    }
    RelativePath::new(native).map_err(RemoteScanError::from)
}

fn validate_native_component(component: &OsStr) -> Result<()> {
    let mut components = Path::new(component).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(RemoteScanError::InvalidPathComponent),
    }
}

#[cfg(unix)]
fn decode_native_component(bytes: &[u8], peer: PlatformOs) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt;

    ensure_compatible_path_encoding(peer)?;
    if bytes.contains(&0) {
        return Err(RemoteScanError::PathContainsNul);
    }
    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
fn decode_native_component(bytes: &[u8], peer: PlatformOs) -> Result<OsString> {
    use std::os::windows::ffi::OsStringExt;

    ensure_compatible_path_encoding(peer)?;
    if bytes.len() % 2 != 0 {
        return Err(RemoteScanError::InvalidPathComponent);
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(RemoteScanError::PathContainsNul);
    }
    Ok(OsString::from_wide(&units))
}

#[cfg(not(any(unix, windows)))]
fn decode_native_component(_bytes: &[u8], peer: PlatformOs) -> Result<OsString> {
    Err(RemoteScanError::UnsupportedPathEncoding {
        local: Platform::current().os,
        peer,
    })
}

#[cfg(unix)]
fn encode_native_path(path: &Path) -> Result<WirePath> {
    use std::os::unix::ffi::OsStrExt;

    WirePath::new(path.as_os_str().as_bytes().to_vec()).map_err(RemoteScanError::from)
}

#[cfg(windows)]
fn encode_native_path(path: &Path) -> Result<WirePath> {
    use std::os::windows::ffi::OsStrExt;

    let mut bytes = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    WirePath::new(bytes).map_err(RemoteScanError::from)
}

#[cfg(not(any(unix, windows)))]
fn encode_native_path(_path: &Path) -> Result<WirePath> {
    Err(RemoteScanError::UnsupportedPathEncoding {
        local: Platform::current().os,
        peer: Platform::current().os,
    })
}

#[cfg(unix)]
fn decode_native_path(path: WirePath, peer: PlatformOs) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    ensure_compatible_path_encoding(peer)?;
    let bytes = path.into_bytes();
    if bytes.contains(&0) {
        return Err(RemoteScanError::PathContainsNul);
    }
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn decode_native_path(path: WirePath, peer: PlatformOs) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    ensure_compatible_path_encoding(peer)?;
    let bytes = path.into_bytes();
    if bytes.len() % 2 != 0 {
        return Err(RemoteScanError::InvalidPathComponent);
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(RemoteScanError::PathContainsNul);
    }
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(not(any(unix, windows)))]
fn decode_native_path(_path: WirePath, peer: PlatformOs) -> Result<PathBuf> {
    Err(RemoteScanError::UnsupportedPathEncoding {
        local: Platform::current().os,
        peer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Operation;
    use crate::remote::{client_handshake, server_handshake};
    use crate::remote_router::{FrameRouter, RouterConfig, RouterRole};

    #[test]
    fn scan_request_round_trip_matches_engine_request() {
        let request = ScanRequest {
            respect_gitignore: true,
            include_git_dir: true,
            max_depth: Some(7),
            metadata: EntryMetadataRequest {
                unix_mode: true,
                symlink_target: true,
                identity: true,
                hardlink_group: true,
            },
        };
        let decoded = wire_to_scan_request(scan_request_to_wire(request).unwrap()).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn rejects_cross_family_ordering() {
        let peer = match Platform::current().os {
            PlatformOs::Windows => PlatformOs::Linux,
            _ => PlatformOs::Windows,
        };
        assert!(matches!(
            ensure_compatible_path_encoding(peer),
            Err(RemoteScanError::UnsupportedPathEncoding { .. })
        ));
    }

    #[tokio::test]
    async fn handshake_and_routed_scan_stream_ordered_entries() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("a"), b"a").unwrap();
        std::fs::write(root.path().join("dir").join("b"), b"b").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_io);

        let server = tokio::spawn(async move {
            let opened = server_handshake(&mut server_reader, &mut server_writer)
                .await
                .unwrap();
            let mut router = FrameRouter::start(
                server_reader,
                server_writer,
                RouterRole::Server,
                RouterConfig::default(),
            )
            .unwrap();
            let incoming = router.incoming().recv().await.unwrap().unwrap();
            let sender = router.sender();
            serve_incoming_scan(&opened.root, incoming, &sender)
                .await
                .unwrap();
        });

        let session = client_handshake(
            &mut client_reader,
            &mut client_writer,
            Operation::Push,
            root.path(),
        )
        .await
        .unwrap();
        let router = FrameRouter::start(
            client_reader,
            client_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();
        let sender = router.sender();

        let request = ScanRequest {
            respect_gitignore: false,
            include_git_dir: false,
            max_depth: None,
            metadata: EntryMetadataRequest {
                unix_mode: cfg!(unix),
                symlink_target: true,
                identity: true,
                hardlink_group: false,
            },
        };
        let mut entries = request_scan(&sender, request, session.server.platform.os)
            .await
            .unwrap();
        let mut paths = Vec::new();
        while let Some(entry) = entries.next().await {
            paths.push(entry.unwrap().path.as_path().to_path_buf());
        }
        server.await.unwrap();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("a"),
                PathBuf::from("dir"),
                PathBuf::from("dir/b")
            ]
        );
    }
}
