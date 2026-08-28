use super::codec::SliceReader;
use super::{ProtocolError, Result};
use bitflags::bitflags;
use bytes::{BufMut, Bytes, BytesMut};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ScanFlags: u8 {
        const RESPECT_GITIGNORE = 1 << 0;
        const INCLUDE_GIT_DIR = 1 << 1;
        const HAS_MAX_DEPTH = 1 << 2;
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MetadataFlags: u8 {
        const UNIX_MODE = 1 << 0;
        const SYMLINK_TARGET = 1 << 1;
        const IDENTITY = 1 << 2;
        const HARDLINK_GROUP = 1 << 3;
    }
}

/// Request for one ordered metadata enumeration stream.
///
/// This remains separate from `SessionOpen`: opening an endpoint and enumerating
/// it are distinct operations, and future sessions may issue more than one scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireScanRequest {
    pub respect_gitignore: bool,
    pub include_git_dir: bool,
    pub max_depth: Option<u32>,
    pub unix_mode: bool,
    pub symlink_target: bool,
    pub identity: bool,
    pub hardlink_group: bool,
}

impl WireScanRequest {
    pub fn encode(self) -> Bytes {
        let mut scan = ScanFlags::empty();
        scan.set(ScanFlags::RESPECT_GITIGNORE, self.respect_gitignore);
        scan.set(ScanFlags::INCLUDE_GIT_DIR, self.include_git_dir);
        scan.set(ScanFlags::HAS_MAX_DEPTH, self.max_depth.is_some());

        let mut metadata = MetadataFlags::empty();
        metadata.set(MetadataFlags::UNIX_MODE, self.unix_mode);
        metadata.set(MetadataFlags::SYMLINK_TARGET, self.symlink_target);
        metadata.set(MetadataFlags::IDENTITY, self.identity);
        metadata.set(MetadataFlags::HARDLINK_GROUP, self.hardlink_group);

        let mut out = BytesMut::with_capacity(6);
        out.put_u8(scan.bits());
        out.put_u8(metadata.bits());
        out.put_u32(self.max_depth.unwrap_or(0));
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let raw_scan = reader.u8()?;
        let scan = ScanFlags::from_bits(raw_scan).ok_or(ProtocolError::InvalidField {
            field: "scan_flags",
            reason: "unknown scan flag bits",
        })?;
        let raw_metadata = reader.u8()?;
        let metadata =
            MetadataFlags::from_bits(raw_metadata).ok_or(ProtocolError::InvalidField {
                field: "scan_metadata_flags",
                reason: "unknown scan metadata flag bits",
            })?;
        let raw_depth = reader.u32()?;
        reader.finish()?;

        if !scan.contains(ScanFlags::HAS_MAX_DEPTH) && raw_depth != 0 {
            return Err(ProtocolError::InvalidField {
                field: "max_depth",
                reason: "depth must be zero when HAS_MAX_DEPTH is clear",
            });
        }

        Ok(Self {
            respect_gitignore: scan.contains(ScanFlags::RESPECT_GITIGNORE),
            include_git_dir: scan.contains(ScanFlags::INCLUDE_GIT_DIR),
            max_depth: scan.contains(ScanFlags::HAS_MAX_DEPTH).then_some(raw_depth),
            unix_mode: metadata.contains(MetadataFlags::UNIX_MODE),
            symlink_target: metadata.contains(MetadataFlags::SYMLINK_TARGET),
            identity: metadata.contains(MetadataFlags::IDENTITY),
            hardlink_group: metadata.contains(MetadataFlags::HARDLINK_GROUP),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_request_round_trip_preserves_zero_depth() {
        let request = WireScanRequest {
            respect_gitignore: true,
            include_git_dir: false,
            max_depth: Some(0),
            unix_mode: true,
            symlink_target: true,
            identity: true,
            hardlink_group: false,
        };
        assert_eq!(WireScanRequest::decode(&request.encode()).unwrap(), request);
    }

    #[test]
    fn scan_request_rejects_unknown_bits_and_trailing_data() {
        assert!(WireScanRequest::decode(&[0x80, 0, 0, 0, 0, 0]).is_err());
        assert!(WireScanRequest::decode(&[0, 0x80, 0, 0, 0, 0]).is_err());

        let request = WireScanRequest {
            respect_gitignore: false,
            include_git_dir: false,
            max_depth: None,
            unix_mode: false,
            symlink_target: false,
            identity: false,
            hardlink_group: false,
        };
        let mut encoded = request.encode().to_vec();
        encoded.push(0);
        assert!(WireScanRequest::decode(&encoded).is_err());
    }

    #[test]
    fn scan_request_rejects_depth_without_flag() {
        assert!(WireScanRequest::decode(&[0, 0, 0, 0, 0, 1]).is_err());
    }
}
