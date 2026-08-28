pub mod router;
pub mod runtime;
pub mod scan;

use crate::endpoint::{Capabilities, Endpoint};
use crate::protocol::{
    negotiate_version, read_frame, write_frame, CapabilitySet, ClientHello, Frame, FrameKind,
    Operation, Platform, PlatformOs, ProtocolError, ServerHello, SessionOpen, SessionReady,
    VersionRange, WirePath, PROTOCOL_V3,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

const BUILD_ID: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("expected control frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("control frame {kind:?} used non-zero stream id {stream_id}")]
    NonControlFrame { kind: FrameKind, stream_id: u32 },

    #[error("control frame {kind:?} used unsupported flags 0x{flags:02x}")]
    ControlFlags { kind: FrameKind, flags: u8 },

    #[error("invalid remote root: {0}")]
    InvalidRoot(&'static str),

    #[error("cannot encode a remote root for target platform {0:?}")]
    UnsupportedTargetPlatform(PlatformOs),
}

pub type Result<T> = std::result::Result<T, RemoteError>;

#[derive(Debug, Clone)]
struct ClientSession {
    server: ServerHello,
    ready: SessionReady,
}

#[derive(Debug, Clone)]
struct OpenedServerSession {
    client: ClientHello,
    operation: Operation,
    root: PathBuf,
    ready: SessionReady,
}

/// Perform the v3 client control-plane handshake over an already-connected
/// transport. No file data is exchanged here.
async fn client_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    operation: Operation,
    root: &Path,
) -> Result<ClientSession>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let versions = VersionRange::exact(PROTOCOL_V3);
    let hello = ClientHello::new(
        versions,
        process_capabilities(),
        Platform::current(),
        BUILD_ID,
    )?;
    let frame = Frame::control(FrameKind::ClientHello, hello.encode()?)?;
    write_frame(writer, &frame).await?;
    writer.flush().await?;

    let frame = read_frame(reader).await?;
    expect_control(&frame, FrameKind::ServerHello)?;
    let server = ServerHello::decode(frame.payload())?;
    negotiate_version(versions, VersionRange::exact(server.version))?;

    let root = encode_target_root(root, server.platform.os)?;
    let open = SessionOpen::new(operation, root);
    let frame = Frame::control(FrameKind::SessionOpen, open.encode()?)?;
    write_frame(writer, &frame).await?;
    writer.flush().await?;

    let frame = read_frame(reader).await?;
    expect_control(&frame, FrameKind::SessionReady)?;
    let ready = SessionReady::decode(frame.payload())?;

    Ok(ClientSession { server, ready })
}

/// Perform the server half of the v3 control-plane handshake and open the
/// requested local root. The data plane starts only after this returns.
async fn server_handshake<R, W>(reader: &mut R, writer: &mut W) -> Result<OpenedServerSession>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let frame = read_frame(reader).await?;
    expect_control(&frame, FrameKind::ClientHello)?;
    let client = ClientHello::decode(frame.payload())?;

    let version = negotiate_version(client.versions, VersionRange::exact(PROTOCOL_V3))?;
    let server = ServerHello::new(
        version,
        process_capabilities(),
        Platform::current(),
        BUILD_ID,
    )?;
    let frame = Frame::control(FrameKind::ServerHello, server.encode()?)?;
    write_frame(writer, &frame).await?;
    writer.flush().await?;

    let frame = read_frame(reader).await?;
    expect_control(&frame, FrameKind::SessionOpen)?;
    let open = SessionOpen::decode(frame.payload())?;
    let root = expand_tilde(decode_native_root(open.root)?);
    prepare_root(open.operation, &root).await?;

    let endpoint = crate::endpoint::local::LocalEndpoint::new(root.clone());
    let mut capabilities = process_capabilities();
    // `Capabilities::local()` means reflink support can be probed, not that the
    // concrete filesystem at this root supports it. Keep SessionReady
    // conservative until a real per-filesystem probe is part of endpoint open.
    capabilities.remove(CapabilitySet::REFLINK);
    let precision = endpoint.capabilities().modtime_precision.as_nanos();
    let modtime_precision_ns = u64::try_from(precision).unwrap_or(u64::MAX);
    let ready = SessionReady::new(capabilities, modtime_precision_ns);
    let frame = Frame::control(FrameKind::SessionReady, ready.encode())?;
    write_frame(writer, &frame).await?;
    writer.flush().await?;

    Ok(OpenedServerSession {
        client,
        operation: open.operation,
        root,
        ready,
    })
}

fn expect_control(frame: &Frame, expected: FrameKind) -> Result<()> {
    if frame.kind() != expected {
        return Err(RemoteError::UnexpectedFrame {
            expected,
            actual: frame.kind(),
        });
    }
    if !frame.stream_id().is_control() {
        return Err(RemoteError::NonControlFrame {
            kind: frame.kind(),
            stream_id: frame.stream_id().get(),
        });
    }
    if !frame.flags().is_empty() {
        return Err(RemoteError::ControlFlags {
            kind: frame.kind(),
            flags: frame.flags().bits(),
        });
    }
    Ok(())
}

fn process_capabilities() -> CapabilitySet {
    // Advertise only behavior owned by the negotiated v3 runtime. The central
    // frame router is mandatory after handshake, so multiplexing is a runtime
    // invariant rather than a reserved protocol possibility.
    endpoint_capabilities(&Capabilities::local())
        | CapabilitySet::BLAKE3
        | CapabilitySet::RAW_PATHS
        | CapabilitySet::MULTIPLEXING
}

fn endpoint_capabilities(capabilities: &Capabilities) -> CapabilitySet {
    let mut result = CapabilitySet::empty();
    set_capability(
        &mut result,
        CapabilitySet::ATOMIC_REPLACE,
        capabilities.atomic_rename,
    );
    set_capability(
        &mut result,
        CapabilitySet::STAGED_WRITE,
        capabilities.staged_write,
    );
    set_capability(
        &mut result,
        CapabilitySet::RANDOM_READ,
        capabilities.random_read,
    );
    set_capability(
        &mut result,
        CapabilitySet::RANDOM_WRITE,
        capabilities.random_write,
    );
    set_capability(&mut result, CapabilitySet::REFLINK, capabilities.reflink);
    set_capability(&mut result, CapabilitySet::SPARSE, capabilities.sparse);
    set_capability(
        &mut result,
        CapabilitySet::XATTR,
        capabilities.preserve_xattrs,
    );
    set_capability(&mut result, CapabilitySet::ACL, capabilities.preserve_acls);
    set_capability(
        &mut result,
        CapabilitySet::HARDLINK,
        capabilities.preserve_hardlinks,
    );
    set_capability(
        &mut result,
        CapabilitySet::BSD_FLAGS,
        capabilities.preserve_flags,
    );
    result
}

fn set_capability(set: &mut CapabilitySet, capability: CapabilitySet, enabled: bool) {
    if enabled {
        set.insert(capability);
    }
}

async fn prepare_root(operation: Operation, root: &Path) -> Result<()> {
    if root.as_os_str().is_empty() {
        return Err(RemoteError::InvalidRoot("root path is empty"));
    }

    match tokio::fs::try_exists(root).await? {
        true => Ok(()),
        false if operation == Operation::Push => {
            tokio::fs::create_dir_all(root).await?;
            Ok(())
        }
        false => Err(RemoteError::InvalidRoot("pull root does not exist")),
    }
}

#[cfg(unix)]
fn decode_native_root(root: WirePath) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let bytes = root.into_bytes();
    if bytes.is_empty() {
        return Err(RemoteError::InvalidRoot("root path is empty"));
    }
    if bytes.contains(&0) {
        return Err(RemoteError::InvalidRoot("root path contains NUL"));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn decode_native_root(root: WirePath) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    let bytes = root.into_bytes();
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return Err(RemoteError::InvalidRoot(
            "Windows root must contain complete UTF-16 code units",
        ));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(RemoteError::InvalidRoot("root path contains NUL"));
    }
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(not(any(unix, windows)))]
fn decode_native_root(_root: WirePath) -> Result<PathBuf> {
    Err(RemoteError::InvalidRoot(
        "native root decoding is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn encode_target_root(root: &Path, target: PlatformOs) -> Result<WirePath> {
    use std::os::unix::ffi::OsStrExt;

    match target {
        PlatformOs::Linux | PlatformOs::Macos => {
            Ok(WirePath::new(root.as_os_str().as_bytes().to_vec())?)
        }
        PlatformOs::Windows => {
            let value = root.to_str().ok_or(RemoteError::InvalidRoot(
                "non-Unicode Unix path cannot target Windows",
            ))?;
            let mut bytes = Vec::with_capacity(value.len() * 2);
            for unit in value.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            Ok(WirePath::new(bytes)?)
        }
        PlatformOs::Other(_) => Err(RemoteError::UnsupportedTargetPlatform(target)),
    }
}

#[cfg(windows)]
fn encode_target_root(root: &Path, target: PlatformOs) -> Result<WirePath> {
    use std::os::windows::ffi::OsStrExt;

    match target {
        PlatformOs::Windows => {
            let mut bytes = Vec::new();
            for unit in root.as_os_str().encode_wide() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            Ok(WirePath::new(bytes)?)
        }
        PlatformOs::Linux | PlatformOs::Macos => {
            let value = root.to_str().ok_or(RemoteError::InvalidRoot(
                "non-Unicode Windows path cannot target Unix",
            ))?;
            Ok(WirePath::new(value.as_bytes().to_vec())?)
        }
        PlatformOs::Other(_) => Err(RemoteError::UnsupportedTargetPlatform(target)),
    }
}

#[cfg(not(any(unix, windows)))]
fn encode_target_root(_root: &Path, target: PlatformOs) -> Result<WirePath> {
    Err(RemoteError::UnsupportedTargetPlatform(target))
}

#[cfg(unix)]
fn expand_tilde(path: PathBuf) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = path.as_os_str().as_bytes();
    let rest = if bytes == b"~" {
        Some(&b""[..])
    } else {
        bytes.strip_prefix(b"~/")
    };

    match (rest, dirs::home_dir()) {
        (Some([]), Some(home)) => home,
        (Some(rest), Some(home)) => home.join(OsString::from_vec(rest.to_vec())),
        _ => path,
    }
}

#[cfg(not(unix))]
fn expand_tilde(path: PathBuf) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        dirs::home_dir().unwrap_or(path)
    } else if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        dirs::home_dir().map_or(path, |home| home.join(rest))
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn control_plane_round_trip_opens_root_after_platform_negotiation() {
        let root = tempfile::TempDir::new().unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_io);

        let server =
            tokio::spawn(
                async move { server_handshake(&mut server_reader, &mut server_writer).await },
            );
        let client = client_handshake(
            &mut client_reader,
            &mut client_writer,
            Operation::Push,
            root.path(),
        )
        .await
        .unwrap();
        let opened = server.await.unwrap().unwrap();

        assert_eq!(client.server.version, PROTOCOL_V3);
        assert_eq!(opened.operation, Operation::Push);
        assert_eq!(opened.root, root.path());
        assert!(client.ready.capabilities.contains(CapabilitySet::BLAKE3));
        assert!(client.ready.capabilities.contains(CapabilitySet::RAW_PATHS));
        assert!(client
            .ready
            .capabilities
            .contains(CapabilitySet::MULTIPLEXING));
        assert!(!client
            .ready
            .capabilities
            .contains(CapabilitySet::ROLLING_SIGNATURES));
        assert!(!client.ready.capabilities.contains(CapabilitySet::REFLINK));
    }

    #[tokio::test]
    async fn push_session_creates_missing_root() {
        let parent = tempfile::TempDir::new().unwrap();
        let root = parent.path().join("missing").join("nested");
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_io);

        let server =
            tokio::spawn(
                async move { server_handshake(&mut server_reader, &mut server_writer).await },
            );
        client_handshake(
            &mut client_reader,
            &mut client_writer,
            Operation::Push,
            &root,
        )
        .await
        .unwrap();
        server.await.unwrap().unwrap();
        assert!(root.is_dir());
    }

    #[tokio::test]
    async fn rejects_non_control_hello() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (_client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_io);
        let hello = ClientHello::new(
            VersionRange::exact(PROTOCOL_V3),
            process_capabilities(),
            Platform::current(),
            BUILD_ID,
        )
        .unwrap();
        let frame = Frame::new(
            FrameKind::ClientHello,
            crate::protocol::FrameFlags::empty(),
            crate::protocol::StreamId::new(1),
            hello.encode().unwrap(),
        )
        .unwrap();
        write_frame(&mut client_writer, &frame).await.unwrap();
        client_writer.flush().await.unwrap();

        let error = server_handshake(&mut server_reader, &mut server_writer)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteError::NonControlFrame { .. }));
    }
}
