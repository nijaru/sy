pub mod acl;
pub mod bsdflags;
mod data;
pub mod fetch;
pub mod hash;
pub mod local_executor;
pub mod path;
pub mod pull;
pub mod pull_lower;
pub mod push;

pub mod router;
pub mod runtime;
pub mod scan;
pub mod serve;
pub mod signature;
#[cfg(feature = "ssh")]
pub mod ssh;
pub mod transfer;
pub mod xattr;

use crate::endpoint::{Capabilities as EndpointCapabilities, Endpoint};
use crate::protocol::{
    negotiate_version, read_frame, write_frame, CapabilitySet, ClientHello, Frame, FrameKind,
    Operation, Platform, PlatformOs, ProtocolError, ServerHello, SessionOpen, SessionReady,
    VersionRange, WirePath, SUPPORTED_VERSIONS,
};
use crate::rooted_fs::{RootedFs, RootedFsError};
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

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("root preparation worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),

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
    capabilities: EndpointCapabilities,
    namespace_semantics: Option<crate::engine::namespace::NamespaceSemantics>,
}

#[derive(Debug, Clone)]
enum SessionRoot {
    Present(RootedFs),
    AbsentPreview,
}

#[derive(Debug, Clone)]
struct OpenedServerSession {
    client: ClientHello,
    version: crate::protocol::ProtocolVersion,
    operation: Operation,
    root: PathBuf,
    rooted: SessionRoot,
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
    let versions = SUPPORTED_VERSIONS;
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
    let ready = SessionReady::decode(frame.payload(), server.version)?;
    let capabilities =
        EndpointCapabilities::from_negotiated_wire(ready.capabilities, ready.modtime_precision_ns);
    let namespace_semantics = ready.namespace_semantics.map(Into::into);

    Ok(ClientSession {
        server,
        capabilities,
        namespace_semantics,
    })
}

/// Perform the server half of the v3 control-plane handshake and pin the
/// requested local root. The data plane starts only after this returns.
async fn server_handshake<R, W>(reader: &mut R, writer: &mut W) -> Result<OpenedServerSession>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let frame = read_frame(reader).await?;
    expect_control(&frame, FrameKind::ClientHello)?;
    let client = ClientHello::decode(frame.payload())?;

    let version = negotiate_version(client.versions, SUPPORTED_VERSIONS)?;
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
    let rooted = prepare_root(open.operation, &root).await?;

    let endpoint = crate::endpoint::local::LocalEndpoint::new(root.clone());
    let capabilities = negotiated_capabilities(client.capabilities);
    let precision = endpoint.capabilities().modtime_precision.as_nanos();
    let modtime_precision_ns = u64::try_from(precision).unwrap_or(u64::MAX);
    // Root-scoped name semantics: the client's alias preflight must follow the
    // opened filesystem, not the peer OS name.
    let namespace_semantics = match &rooted {
        SessionRoot::Present(rooted) => rooted.namespace_semantics().await?,
        SessionRoot::AbsentPreview => crate::engine::namespace::NamespaceSemantics::UNSPECIFIED,
    }
    .into();
    let ready = SessionReady::new(capabilities, modtime_precision_ns, namespace_semantics);
    let frame = Frame::control(FrameKind::SessionReady, ready.encode(version))?;
    write_frame(writer, &frame).await?;
    writer.flush().await?;

    Ok(OpenedServerSession {
        client,
        version,
        operation: open.operation,
        root,
        rooted,
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
    // Advertise only behavior that the negotiated v3 runtime actually owns.
    // Endpoint-local filesystem capabilities become negotiable only when their
    // corresponding v3 operations exist.
    let mut capabilities =
        CapabilitySet::BLAKE3 | CapabilitySet::RAW_PATHS | CapabilitySet::MULTIPLEXING;
    let os = Platform::current().os;

    // Rolling-signature basis reads are advertised only where RootedFs can
    // enforce held-directory-FD confinement and the v3 path encoding has been
    // validated. Linux and macOS are the supported cross-OS family for 0.5.
    if supports_rolling_signatures(os) {
        capabilities.insert(CapabilitySet::ROLLING_SIGNATURES);
    }
    // File transfers now reconstruct into a same-directory no-follow staging
    // file and rename it through the held parent descriptor after verification.
    if supports_staged_files(os) {
        capabilities.insert(CapabilitySet::STAGED_WRITE | CapabilitySet::ATOMIC_REPLACE);
    }
    // Hardlink creation is owned end to end: the `Hardlink` mutation links
    // through held parent descriptors (no-follow, regular-file verified),
    // and the push/pull/local executors deduplicate scanned hardlink groups.
    if supports_hardlinks(os) {
        capabilities.insert(CapabilitySet::HARDLINK);
    }
    // Extended attributes are read and written through held descriptors with
    // the same no-follow leaf discipline, and every executor mirrors the
    // source set onto the destination after a content or metadata mutation.
    if supports_xattrs(os) {
        capabilities.insert(CapabilitySet::XATTR);
    }
    // Access-control lists are owned the same way: Linux reuses exacl on a
    // `/proc/self/fd` alias of the held no-follow leaf, macOS drives fd
    // syscalls directly, and every executor mirrors the source list after a
    // mutation. Advertised only where the implementation exists, i.e. with
    // the `acl` feature on the Unix data-plane family.
    if acl_advertised_for(os) {
        capabilities.insert(CapabilitySet::ACL);
    }
    // BSD file flags are a single confined word (fchflags through the held
    // no-follow leaf) mirrored by every executor after a mutation. Only
    // macOS implements them; elsewhere `-F` is refused up front.
    if supports_bsd_flags(os) {
        capabilities.insert(CapabilitySet::BSD_FLAGS);
    }
    // Compressed Data frames are decodable wherever zstd is linked, which is
    // everywhere this crate builds.
    capabilities.insert(CapabilitySet::ZSTD);
    capabilities
}

fn negotiated_capabilities(peer: CapabilitySet) -> CapabilitySet {
    process_capabilities() & peer
}

const fn supports_rolling_signatures(os: PlatformOs) -> bool {
    matches!(os, PlatformOs::Linux | PlatformOs::Macos)
}

const fn supports_staged_files(os: PlatformOs) -> bool {
    matches!(os, PlatformOs::Linux | PlatformOs::Macos)
}

const fn supports_hardlinks(os: PlatformOs) -> bool {
    matches!(os, PlatformOs::Linux | PlatformOs::Macos)
}

/// Extended-attribute access is descriptor-based and only compiled on Unix;
/// both supported 0.5 data-plane platforms advertise it.
const fn supports_xattrs(os: PlatformOs) -> bool {
    matches!(os, PlatformOs::Linux | PlatformOs::Macos)
}

/// Access-control confinement matches xattrs (held no-follow leaf on both
/// data-plane platforms), but the implementation additionally requires the
/// `acl` feature that links exacl/libacl. The capability is inserted under
/// that feature gate at the call site.
#[allow(dead_code)]
const fn supports_acls(os: PlatformOs) -> bool {
    matches!(os, PlatformOs::Linux | PlatformOs::Macos)
}

/// BSD-flags access is a single confined word available only on macOS
/// (`fchflags`/`st_flags`); no feature gate is involved.
const fn supports_bsd_flags(os: PlatformOs) -> bool {
    matches!(os, PlatformOs::Macos)
}

/// Whether this build advertises the ACL capability for `os`: the
/// implementation exists only with the `acl` feature on the Unix data-plane
/// family.
const fn acl_advertised_for(os: PlatformOs) -> bool {
    #[cfg(all(unix, feature = "acl"))]
    {
        supports_acls(os)
    }
    #[cfg(not(all(unix, feature = "acl")))]
    {
        let _ = os;
        false
    }
}

async fn prepare_root(operation: Operation, root: &Path) -> Result<SessionRoot> {
    if root.as_os_str().is_empty() {
        return Err(RemoteError::InvalidRoot("root path is empty"));
    }

    match tokio::fs::try_exists(root).await? {
        true => {}
        false if operation == Operation::PreviewPush => return Ok(SessionRoot::AbsentPreview),
        false if operation == Operation::Push => tokio::fs::create_dir_all(root).await?,
        false => return Err(RemoteError::InvalidRoot("pull root does not exist")),
    }
    // Never substitute an ancestor descriptor for an absent preview root.
    Ok(SessionRoot::Present(
        RootedFs::open(root.to_path_buf()).await?,
    ))
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
    let value = path.to_string_lossy().into_owned();
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

        assert_eq!(client.server.version, SUPPORTED_VERSIONS.max);
        // The client receives the probed root semantics, not an OS guess.
        assert_eq!(
            client.namespace_semantics,
            Some(
                crate::fs_util::namespace_semantics(root.path())
                    .await
                    .unwrap()
            )
        );
        assert_eq!(opened.operation, Operation::Push);
        assert_eq!(opened.root, root.path());
        let SessionRoot::Present(rooted) = opened.rooted else {
            panic!("missing root authority")
        };
        assert_eq!(rooted.root_path(), root.path());
        let capabilities = &client.capabilities;
        assert!(capabilities.blake3);
        assert!(capabilities.raw_paths);
        assert!(capabilities.multiplexing);
        assert_eq!(
            capabilities.rolling_signatures,
            supports_rolling_signatures(Platform::current().os)
        );
        assert_eq!(
            capabilities.atomic_rename,
            supports_staged_files(Platform::current().os)
        );
        assert_eq!(
            capabilities.staged_write,
            supports_staged_files(Platform::current().os)
        );
        assert_eq!(
            capabilities.staged_verify,
            supports_staged_files(Platform::current().os)
        );
        assert!(!capabilities.random_read);
        assert!(!capabilities.random_write);
        assert!(!capabilities.reflink);
        assert!(!capabilities.sparse);
        assert_eq!(
            capabilities.preserve_xattrs,
            supports_xattrs(Platform::current().os)
        );
        assert_eq!(
            capabilities.preserve_acls,
            acl_advertised_for(Platform::current().os),
        );
        assert_eq!(
            capabilities.preserve_hardlinks,
            supports_hardlinks(Platform::current().os)
        );
        assert_eq!(
            capabilities.preserve_flags,
            supports_bsd_flags(Platform::current().os),
        );
        assert!(capabilities.zstd);
    }

    #[test]
    fn process_capabilities_are_runtime_backed_and_peer_bounded() {
        let local = process_capabilities();
        assert!(local.contains(CapabilitySet::BLAKE3));
        assert!(local.contains(CapabilitySet::RAW_PATHS));
        assert!(local.contains(CapabilitySet::MULTIPLEXING));
        assert_eq!(
            local.contains(CapabilitySet::ATOMIC_REPLACE),
            supports_staged_files(Platform::current().os)
        );
        assert_eq!(
            local.contains(CapabilitySet::STAGED_WRITE),
            supports_staged_files(Platform::current().os)
        );
        assert_eq!(
            local.contains(CapabilitySet::HARDLINK),
            supports_hardlinks(Platform::current().os)
        );
        assert!(!local.intersects(
            CapabilitySet::RANDOM_READ
                | CapabilitySet::RANDOM_WRITE
                | CapabilitySet::REFLINK
                | CapabilitySet::SPARSE
        ));
        assert_eq!(
            local.contains(CapabilitySet::BSD_FLAGS),
            supports_bsd_flags(Platform::current().os),
        );
        assert_eq!(
            local.contains(CapabilitySet::ACL),
            acl_advertised_for(Platform::current().os)
        );

        let peer = CapabilitySet::BLAKE3 | CapabilitySet::MULTIPLEXING;
        assert_eq!(negotiated_capabilities(peer), peer);
    }

    #[test]
    fn rolling_signatures_are_scoped_to_tested_os_family() {
        assert!(supports_rolling_signatures(PlatformOs::Linux));
        assert!(supports_rolling_signatures(PlatformOs::Macos));
        assert!(!supports_rolling_signatures(PlatformOs::Windows));
        assert!(!supports_rolling_signatures(PlatformOs::Other(4)));
    }

    #[test]
    fn staged_files_are_scoped_to_tested_os_family() {
        assert!(supports_staged_files(PlatformOs::Linux));
        assert!(supports_staged_files(PlatformOs::Macos));
        assert!(!supports_staged_files(PlatformOs::Windows));
        assert!(!supports_staged_files(PlatformOs::Other(4)));
    }

    #[test]
    fn hardlinks_are_scoped_to_tested_os_family() {
        assert!(supports_hardlinks(PlatformOs::Linux));
        assert!(supports_hardlinks(PlatformOs::Macos));
        assert!(!supports_hardlinks(PlatformOs::Windows));
        assert!(!supports_hardlinks(PlatformOs::Other(4)));
    }

    #[test]
    fn xattrs_are_scoped_to_tested_os_family() {
        assert!(supports_xattrs(PlatformOs::Linux));
        assert!(supports_xattrs(PlatformOs::Macos));
        assert!(!supports_xattrs(PlatformOs::Windows));
        assert!(!supports_xattrs(PlatformOs::Other(4)));
    }

    #[test]
    fn bsd_flags_are_macos_only() {
        assert!(!supports_bsd_flags(PlatformOs::Linux));
        assert!(supports_bsd_flags(PlatformOs::Macos));
        assert!(!supports_bsd_flags(PlatformOs::Windows));
        assert!(!supports_bsd_flags(PlatformOs::Other(4)));
    }

    #[test]
    fn acls_are_scoped_to_tested_os_family() {
        assert!(supports_acls(PlatformOs::Linux));
        assert!(supports_acls(PlatformOs::Macos));
        assert!(!supports_acls(PlatformOs::Windows));
        assert!(!supports_acls(PlatformOs::Other(4)));
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
    async fn incompatible_client_is_rejected_before_root_creation() {
        let parent = tempfile::TempDir::new().unwrap();
        let root = parent.path().join("must-not-be-created");
        let old = VersionRange::new(crate::protocol::PROTOCOL_V3, crate::protocol::PROTOCOL_V3_6)
            .unwrap();
        let (mut client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut reader, mut writer) = tokio::io::split(server_io);
        let hello =
            ClientHello::new(old, process_capabilities(), Platform::current(), "old").unwrap();
        write_frame(
            &mut client_io,
            &Frame::control(FrameKind::ClientHello, hello.encode().unwrap()).unwrap(),
        )
        .await
        .unwrap();
        // Even a pipelined root-mutating request must not get past negotiation.
        let open = SessionOpen::new(
            Operation::Push,
            encode_target_root(&root, Platform::current().os).unwrap(),
        );
        write_frame(
            &mut client_io,
            &Frame::control(FrameKind::SessionOpen, open.encode().unwrap()).unwrap(),
        )
        .await
        .unwrap();
        let error = server_handshake(&mut reader, &mut writer)
            .await
            .unwrap_err();
        assert!(
            matches!(error, RemoteError::Protocol(ProtocolError::NoCompatibleVersion { client, server }) if client == old && server == SUPPORTED_VERSIONS)
        );
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn incompatible_server_is_rejected_before_session_open() {
        let root = tempfile::TempDir::new().unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut reader, mut writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let hello = read_frame(&mut reader).await.unwrap();
            assert_eq!(
                ClientHello::decode(hello.payload()).unwrap().versions,
                SUPPORTED_VERSIONS
            );
            let response = ServerHello::new(
                crate::protocol::PROTOCOL_V3_6,
                process_capabilities(),
                Platform::current(),
                "old",
            )
            .unwrap();
            write_frame(
                &mut writer,
                &Frame::control(FrameKind::ServerHello, response.encode().unwrap()).unwrap(),
            )
            .await
            .unwrap();
            writer.flush().await.unwrap();
            // EOF rather than SessionOpen: the old peer never receives a root.
            use tokio::io::AsyncReadExt;
            let mut byte = [0];
            reader.read(&mut byte).await.unwrap()
        });
        let result = client_handshake(
            &mut client_reader,
            &mut client_writer,
            Operation::Push,
            root.path(),
        )
        .await;
        drop(client_reader);
        drop(client_writer);
        let received = server.await.unwrap();
        assert!(matches!(
            result,
            Err(RemoteError::Protocol(
                ProtocolError::NoCompatibleVersion { .. }
            ))
        ));
        assert_eq!(received, 0);
    }

    #[tokio::test]
    async fn rejects_non_control_hello() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (_client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_io);
        let hello = ClientHello::new(
            SUPPORTED_VERSIONS,
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
