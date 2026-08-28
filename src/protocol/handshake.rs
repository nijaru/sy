use super::{ProtocolError, Result, WirePath};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Operation {
    Push = 1,
    Pull = 2,
}

impl TryFrom<u8> for Operation {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Push),
            2 => Ok(Self::Pull),
            _ => Err(ProtocolError::InvalidField {
                field: "operation",
                reason: "unknown operation value",
            }),
        }
    }
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
    fn encode(self) -> u8 {
        match self {
            Self::Linux => 1,
            Self::Macos => 2,
            Self::Windows => 3,
            Self::Other(value) => value,
        }
    }

    fn decode(value: u8) -> Self {
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
    fn encode(self) -> u8 {
        match self {
            Self::X86_64 => 1,
            Self::Aarch64 => 2,
            Self::X86 => 3,
            Self::Other(value) => value,
        }
    }

    fn decode(value: u8) -> Self {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub versions: VersionRange,
    pub operation: Operation,
    pub capabilities: CapabilitySet,
    pub build_id: String,
    pub root: WirePath,
}

impl ClientHello {
    pub fn new(
        versions: VersionRange,
        operation: Operation,
        capabilities: CapabilitySet,
        build_id: impl Into<String>,
        root: WirePath,
    ) -> Result<Self> {
        let build_id = build_id.into();
        validate_build_id(&build_id)?;
        Ok(Self {
            versions,
            operation,
            capabilities,
            build_id,
            root,
        })
    }

    pub fn encode(&self) -> Result<Bytes> {
        validate_build_id(&self.build_id)?;
        let build = self.build_id.as_bytes();
        let build_len = u16::try_from(build.len()).map_err(|_| ProtocolError::InvalidField {
            field: "build_id",
            reason: "build identifier length exceeds u16",
        })?;
        let root_len =
            u32::try_from(self.root.as_bytes().len()).map_err(|_| ProtocolError::InvalidField {
                field: "root",
                reason: "root path length exceeds u32",
            })?;

        let mut out = BytesMut::with_capacity(25 + build.len() + self.root.as_bytes().len());
        put_version(&mut out, self.versions.min);
        put_version(&mut out, self.versions.max);
        out.put_u8(self.operation as u8);
        out.put_u64(self.capabilities.bits());
        out.put_u16(build_len);
        out.put_u32(root_len);
        out.extend_from_slice(build);
        out.extend_from_slice(self.root.as_bytes());
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let min = reader.version()?;
        let max = reader.version()?;
        let versions = VersionRange::new(min, max)?;
        let operation = Operation::try_from(reader.u8()?)?;
        let capabilities = CapabilitySet::from_bits_retain(reader.u64()?);
        let build_len = reader.u16()? as usize;
        if build_len > MAX_BUILD_ID_BYTES {
            return Err(ProtocolError::InvalidField {
                field: "build_id",
                reason: "build identifier exceeds maximum length",
            });
        }
        let root_len = reader.u32()? as usize;
        let build = reader.bytes(build_len)?;
        let build_id = std::str::from_utf8(build)
            .map_err(|_| ProtocolError::InvalidField {
                field: "build_id",
                reason: "build identifier is not UTF-8",
            })?
            .to_owned();
        validate_build_id(&build_id)?;
        let root = WirePath::new(Bytes::copy_from_slice(reader.bytes(root_len)?))?;
        reader.finish()?;

        Self::new(versions, operation, capabilities, build_id, root)
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
        out.put_u8(self.platform.os.encode());
        out.put_u8(self.platform.arch.encode());
        out.put_u16(build_len);
        out.extend_from_slice(build);
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let version = reader.version()?;
        let capabilities = CapabilitySet::from_bits_retain(reader.u64()?);
        let platform = Platform {
            os: PlatformOs::decode(reader.u8()?),
            arch: PlatformArch::decode(reader.u8()?),
        };
        let build_len = reader.u16()? as usize;
        if build_len > MAX_BUILD_ID_BYTES {
            return Err(ProtocolError::InvalidField {
                field: "build_id",
                reason: "build identifier exceeds maximum length",
            });
        }
        let build_id = std::str::from_utf8(reader.bytes(build_len)?)
            .map_err(|_| ProtocolError::InvalidField {
                field: "build_id",
                reason: "build identifier is not UTF-8",
            })?
            .to_owned();
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

fn put_version(out: &mut BytesMut, version: ProtocolVersion) {
    out.put_u16(version.major);
    out.put_u16(version.minor);
}

struct SliceReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> SliceReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(ProtocolError::InvalidMessage("message length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ProtocolError::InvalidMessage("truncated handshake payload"))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn version(&mut self) -> Result<ProtocolVersion> {
        Ok(ProtocolVersion {
            major: self.u16()?,
            minor: self.u16()?,
        })
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        self.take(len)
    }

    fn finish(self) -> Result<()> {
        if self.offset != self.bytes.len() {
            return Err(ProtocolError::InvalidMessage(
                "trailing bytes after handshake payload",
            ));
        }
        Ok(())
    }
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
    fn client_hello_round_trip_preserves_raw_root() {
        let hello = ClientHello::new(
            VersionRange::exact(PROTOCOL_V3),
            Operation::Push,
            capabilities(),
            "0.5.0-test",
            WirePath::new(Bytes::from_static(b"/tmp/\xffroot")).unwrap(),
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
            Operation::Pull,
            capabilities(),
            "build",
            WirePath::new(Bytes::from_static(b"/root")).unwrap(),
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
