use super::codec::SliceReader;
use super::{ProtocolError, Result};
use bitflags::bitflags;
use bytes::{BufMut, Bytes, BytesMut};

const MAX_BUILD_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

pub const PROTOCOL_V3: ProtocolVersion = ProtocolVersion { major: 3, minor: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    pub min: ProtocolVersion,
    pub max: ProtocolVersion,
}

impl VersionRange {
    pub fn new(min: ProtocolVersion, max: ProtocolVersion) -> Result<Self> {
        if min > max {
            return Err(ProtocolError::InvalidField {
                field: "version_range",
                reason: "minimum version is greater than maximum version",
            });
        }
        Ok(Self { min, max })
    }

    pub const fn exact(version: ProtocolVersion) -> Self {
        Self {
            min: version,
            max: version,
        }
    }
}

pub fn negotiate_version(client: VersionRange, server: VersionRange) -> Result<ProtocolVersion> {
    let lower = client.min.max(server.min);
    let upper = client.max.min(server.max);
    if lower > upper {
        return Err(ProtocolError::NoCompatibleVersion { client, server });
    }
    Ok(upper)
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CapabilitySet: u64 {
        const ATOMIC_REPLACE = 1 << 0;
        const STAGED_WRITE = 1 << 1;
        const RANDOM_READ = 1 << 2;
        const RANDOM_WRITE = 1 << 3;
        const REFLINK = 1 << 4;
        const SPARSE = 1 << 5;
        const XATTR = 1 << 6;
        const ACL = 1 << 7;
        const HARDLINK = 1 << 8;
        const BSD_FLAGS = 1 << 9;
        const BLAKE3 = 1 << 10;
        const ROLLING_SIGNATURES = 1 << 11;
        const MULTIPLEXING = 1 << 12;
        const RAW_PATHS = 1 << 13;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformOs {
    Linux,
    Macos,
    Windows,
    Other(u8),
}

impl PlatformOs {
    pub(super) const fn encode(self) -> u8 {
        match self {
            Self::Linux => 1,
            Self::Macos => 2,
            Self::Windows => 3,
            Self::Other(value) => value,
        }
    }

    pub(super) const fn decode(value: u8) -> Self {
        match value {
            1 => Self::Linux,
            2 => Self::Macos,
            3 => Self::Windows,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformArch {
    X86_64,
    Aarch64,
    X86,
    Other(u8),
}

impl PlatformArch {
    pub(super) const fn encode(self) -> u8 {
        match self {
            Self::X86_64 => 1,
            Self::Aarch64 => 2,
            Self::X86 => 3,
            Self::Other(value) => value,
        }
    }

    pub(super) const fn decode(value: u8) -> Self {
        match value {
            1 => Self::X86_64,
            2 => Self::Aarch64,
            3 => Self::X86,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: PlatformOs,
    pub arch: PlatformArch,
}

impl Platform {
    pub const fn current() -> Self {
        Self {
            os: current_os(),
            arch: current_arch(),
        }
    }

    fn encode_into(self, out: &mut BytesMut) {
        out.put_u8(self.os.encode());
        out.put_u8(self.arch.encode());
    }

    fn decode_from(reader: &mut SliceReader<'_>) -> Result<Self> {
        Ok(Self {
            os: PlatformOs::decode(reader.u8()?),
            arch: PlatformArch::decode(reader.u8()?),
        })
    }
}

const fn current_os() -> PlatformOs {
    #[cfg(target_os = "linux")]
    {
        return PlatformOs::Linux;
    }
    #[cfg(target_os = "macos")]
    {
        return PlatformOs::Macos;
    }
    #[cfg(target_os = "windows")]
    {
        return PlatformOs::Windows;
    }
    #[allow(unreachable_code)]
    PlatformOs::Other(0)
}

const fn current_arch() -> PlatformArch {
    #[cfg(target_arch = "x86_64")]
    {
        return PlatformArch::X86_64;
    }
    #[cfg(target_arch = "aarch64")]
    {
        return PlatformArch::Aarch64;
    }
    #[cfg(target_arch = "x86")]
    {
        return PlatformArch::X86;
    }
    #[allow(unreachable_code)]
    PlatformArch::Other(0)
}

/// First client control message. It negotiates protocol mechanics only.
///
/// Remote roots and operation direction deliberately do not appear here. The
/// peer platform must be known before a target-native root path is interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub versions: VersionRange,
    pub capabilities: CapabilitySet,
    pub platform: Platform,
    pub build_id: String,
}

impl ClientHello {
    pub fn new(
        versions: VersionRange,
        capabilities: CapabilitySet,
        platform: Platform,
        build_id: impl Into<String>,
    ) -> Result<Self> {
        let build_id = build_id.into();
        validate_build_id(&build_id)?;
        Ok(Self {
            versions,
            capabilities,
            platform,
            build_id,
        })
    }

    pub fn encode(&self) -> Result<Bytes> {
        validate_build_id(&self.build_id)?;
        let build = self.build_id.as_bytes();
        let build_len = u16::try_from(build.len()).map_err(|_| ProtocolError::InvalidField {
            field: "build_id",
            reason: "build identifier length exceeds u16",
        })?;

        let mut out = BytesMut::with_capacity(20 + build.len());
        put_version(&mut out, self.versions.min);
        put_version(&mut out, self.versions.max);
        out.put_u64(self.capabilities.bits());
        self.platform.encode_into(&mut out);
        out.put_u16(build_len);
        out.extend_from_slice(build);
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let min = read_version(&mut reader)?;
        let max = read_version(&mut reader)?;
        let versions = VersionRange::new(min, max)?;
        let capabilities = CapabilitySet::from_bits_retain(reader.u64()?);
        let platform = Platform::decode_from(&mut reader)?;
        let build_id = read_build_id(&mut reader)?;
        reader.finish()?;

        Self::new(versions, capabilities, platform, build_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub version: ProtocolVersion,
    pub capabilities: CapabilitySet,
    pub platform: Platform,
    pub build_id: String,
}

impl ServerHello {
    pub fn new(
        version: ProtocolVersion,
        capabilities: CapabilitySet,
        platform: Platform,
        build_id: impl Into<String>,
    ) -> Result<Self> {
        let build_id = build_id.into();
        validate_build_id(&build_id)?;
        Ok(Self {
            version,
            capabilities,
            platform,
            build_id,
        })
    }

    pub fn encode(&self) -> Result<Bytes> {
        validate_build_id(&self.build_id)?;
        let build = self.build_id.as_bytes();
        let build_len = u16::try_from(build.len()).map_err(|_| ProtocolError::InvalidField {
            field: "build_id",
            reason: "build identifier length exceeds u16",
        })?;

        let mut out = BytesMut::with_capacity(16 + build.len());
        put_version(&mut out, self.version);
        out.put_u64(self.capabilities.bits());
        self.platform.encode_into(&mut out);
        out.put_u16(build_len);
        out.extend_from_slice(build);
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let version = read_version(&mut reader)?;
        let capabilities = CapabilitySet::from_bits_retain(reader.u64()?);
        let platform = Platform::decode_from(&mut reader)?;
        let build_id = read_build_id(&mut reader)?;
        reader.finish()?;

        Self::new(version, capabilities, platform, build_id)
    }
}

fn validate_build_id(build_id: &str) -> Result<()> {
    if build_id.is_empty() {
        return Err(ProtocolError::InvalidField {
            field: "build_id",
            reason: "build identifier is empty",
        });
    }
    if build_id.len() > MAX_BUILD_ID_BYTES {
        return Err(ProtocolError::InvalidField {
            field: "build_id",
            reason: "build identifier exceeds maximum length",
        });
    }
    Ok(())
}

fn read_build_id(reader: &mut SliceReader<'_>) -> Result<String> {
    let build_len = reader.u16()? as usize;
    if build_len > MAX_BUILD_ID_BYTES {
        return Err(ProtocolError::InvalidField {
            field: "build_id",
            reason: "build identifier exceeds maximum length",
        });
    }
    let build_id = std::str::from_utf8(reader.take(build_len)?)
        .map_err(|_| ProtocolError::InvalidField {
            field: "build_id",
            reason: "build identifier is not UTF-8",
        })?
        .to_owned();
    validate_build_id(&build_id)?;
    Ok(build_id)
}

fn put_version(out: &mut BytesMut, version: ProtocolVersion) {
    out.put_u16(version.major);
    out.put_u16(version.minor);
}

fn read_version(reader: &mut SliceReader<'_>) -> Result<ProtocolVersion> {
    Ok(ProtocolVersion {
        major: reader.u16()?,
        minor: reader.u16()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> CapabilitySet {
        CapabilitySet::ATOMIC_REPLACE
            | CapabilitySet::STAGED_WRITE
            | CapabilitySet::BLAKE3
            | CapabilitySet::MULTIPLEXING
            | CapabilitySet::RAW_PATHS
    }

    #[test]
    fn version_negotiation_selects_highest_overlap() {
        let client = VersionRange::new(
            ProtocolVersion { major: 3, minor: 0 },
            ProtocolVersion { major: 3, minor: 4 },
        )
        .unwrap();
        let server = VersionRange::new(
            ProtocolVersion { major: 3, minor: 1 },
            ProtocolVersion { major: 3, minor: 2 },
        )
        .unwrap();
        assert_eq!(
            negotiate_version(client, server).unwrap(),
            ProtocolVersion { major: 3, minor: 2 }
        );
    }

    #[test]
    fn version_negotiation_rejects_disjoint_ranges() {
        let client = VersionRange::exact(PROTOCOL_V3);
        let server = VersionRange::exact(ProtocolVersion { major: 4, minor: 0 });
        assert!(matches!(
            negotiate_version(client, server),
            Err(ProtocolError::NoCompatibleVersion { .. })
        ));
    }

    #[test]
    fn client_hello_round_trip_includes_client_platform() {
        let hello = ClientHello::new(
            VersionRange::exact(PROTOCOL_V3),
            capabilities(),
            Platform {
                os: PlatformOs::Macos,
                arch: PlatformArch::Aarch64,
            },
            "0.5.0-test",
        )
        .unwrap();
        let decoded = ClientHello::decode(&hello.encode().unwrap()).unwrap();
        assert_eq!(decoded, hello);
    }

    #[test]
    fn server_hello_round_trip() {
        let hello = ServerHello::new(
            PROTOCOL_V3,
            capabilities(),
            Platform {
                os: PlatformOs::Linux,
                arch: PlatformArch::Aarch64,
            },
            "0.5.0-test",
        )
        .unwrap();
        let decoded = ServerHello::decode(&hello.encode().unwrap()).unwrap();
        assert_eq!(decoded, hello);
    }

    #[test]
    fn decode_rejects_truncation_and_trailing_data() {
        let hello = ClientHello::new(
            VersionRange::exact(PROTOCOL_V3),
            capabilities(),
            Platform::current(),
            "build",
        )
        .unwrap();
        let encoded = hello.encode().unwrap();

        for len in 0..encoded.len() {
            assert!(ClientHello::decode(&encoded[..len]).is_err());
        }

        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(ClientHello::decode(&trailing).is_err());
    }

    #[test]
    fn unknown_capability_bits_are_forward_compatible() {
        let capabilities = CapabilitySet::from_bits_retain(1_u64 << 63);
        let hello =
            ServerHello::new(PROTOCOL_V3, capabilities, Platform::current(), "future").unwrap();
        let decoded = ServerHello::decode(&hello.encode().unwrap()).unwrap();
        assert_eq!(decoded.capabilities.bits(), 1_u64 << 63);
    }
}
