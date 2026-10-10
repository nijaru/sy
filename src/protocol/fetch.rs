use super::codec::SliceReader;
use super::transfer::TRANSFER_BASIS_IDENTITY_LEN;
use super::{ProtocolError, RelativeWirePath, Result};
use bytes::{BufMut, Bytes, BytesMut};

/// Identity length shared with the transfer path basis (`FileBegin`).
pub const FETCH_IDENTITY_LEN: usize = TRANSFER_BASIS_IDENTITY_LEN;

/// Per-chunk compression intent, not a statement about the encoded response.
/// Only a smaller actual zstd payload earns a `Data` frame's COMPRESSED flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFetchCompression {
    /// Send raw data without attempting compression.
    None,
    /// Sample first, then follow the elapsed-time model's transfer decision.
    Auto,
    /// Attempt every chunk, but retain raw data when it is not smaller as zstd.
    Always,
}

impl WireFetchCompression {
    const fn encode(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Auto => 1,
            Self::Always => 2,
        }
    }

    fn decode(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Auto),
            2 => Ok(Self::Always),
            _ => Err(ProtocolError::InvalidField {
                field: "file_fetch_compression",
                reason: "unknown fetch compression policy",
            }),
        }
    }
}

const FETCH_FLAG_XATTRS: u8 = 1 << 1;
const FETCH_FLAG_ACLS: u8 = 1 << 2;

/// Client request for whole-file source data in a pull session.
///
/// The server validates the original scanned identity before and after reading.
/// The response is bounded `Data` frames followed by `FileEnd` with the BLAKE3
/// digest of uncompressed bytes. The client acknowledges only after verification
/// and destination commit. Requested FileXattrs/FileAcls precede file bytes;
/// an empty value explicitly clears destination metadata.
///
/// Protocol 3.8 layout: size:u64, identity:[u8;32], preservation_flags:u8,
/// compression:u8 (0=None, 1=Auto, 2=Always), relative wire path. The old boolean
/// compression flag (bit 0) is invalid; preservation retains bits 1 and 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireFileFetchRequest {
    pub path: RelativeWirePath,
    file_size: u64,
    identity: [u8; FETCH_IDENTITY_LEN],
    flags: u8,
    compression: WireFetchCompression,
}

impl WireFileFetchRequest {
    pub fn new(
        path: RelativeWirePath,
        file_size: u64,
        identity: [u8; FETCH_IDENTITY_LEN],
        compression: WireFetchCompression,
    ) -> Self {
        Self {
            path,
            file_size,
            identity,
            flags: 0,
            compression,
        }
    }

    pub const fn compression(&self) -> WireFetchCompression {
        self.compression
    }

    pub const fn preserve_xattrs(&self) -> bool {
        self.flags & FETCH_FLAG_XATTRS != 0
    }

    pub const fn preserve_acls(&self) -> bool {
        self.flags & FETCH_FLAG_ACLS != 0
    }

    pub fn with_preservation(mut self, xattrs: bool, acls: bool) -> Self {
        self.flags |= u8::from(xattrs) * FETCH_FLAG_XATTRS;
        self.flags |= u8::from(acls) * FETCH_FLAG_ACLS;
        self
    }

    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    pub const fn identity(&self) -> [u8; FETCH_IDENTITY_LEN] {
        self.identity
    }

    pub fn encode(&self) -> Bytes {
        let mut out =
            BytesMut::with_capacity(10 + FETCH_IDENTITY_LEN + self.path.as_encoded().len());
        out.put_u64(self.file_size);
        out.extend_from_slice(&self.identity);
        out.put_u8(self.flags);
        out.put_u8(self.compression.encode());
        out.extend_from_slice(self.path.as_encoded());
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let file_size = reader.u64()?;
        let identity = reader.array::<FETCH_IDENTITY_LEN>()?;
        let flags = reader.u8()?;
        if flags & !(FETCH_FLAG_XATTRS | FETCH_FLAG_ACLS) != 0 {
            return Err(ProtocolError::InvalidField {
                field: "file_fetch_flags",
                reason: "unknown fetch request flag bits",
            });
        }
        let compression = WireFetchCompression::decode(reader.u8()?)?;
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take_remaining()?))?;
        Ok(Self {
            path,
            file_size,
            identity,
            flags,
            compression,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(compression: WireFetchCompression) -> WireFileFetchRequest {
        let path =
            RelativeWirePath::from_components([b"dir".as_slice(), b"file.txt".as_slice()]).unwrap();
        WireFileFetchRequest::new(path, 4096, [7_u8; FETCH_IDENTITY_LEN], compression)
    }

    #[test]
    fn file_fetch_request_preserves_each_policy_and_preservation_flags() {
        for (policy, value) in [
            (WireFetchCompression::None, 0),
            (WireFetchCompression::Auto, 1),
            (WireFetchCompression::Always, 2),
        ] {
            for (xattrs, acls, flags) in [
                (false, false, 0),
                (true, false, 2),
                (false, true, 4),
                (true, true, 6),
            ] {
                let original = request(policy).with_preservation(xattrs, acls);
                let encoded = original.encode();
                assert_eq!(&encoded[..8], &4096_u64.to_be_bytes());
                assert_eq!(
                    &encoded[8..8 + FETCH_IDENTITY_LEN],
                    &[7; FETCH_IDENTITY_LEN]
                );
                assert_eq!(
                    &encoded[8 + FETCH_IDENTITY_LEN..10 + FETCH_IDENTITY_LEN],
                    &[flags, value]
                );
                let decoded = WireFileFetchRequest::decode(&encoded).unwrap();
                assert_eq!(decoded, original);
                assert_eq!(decoded.compression(), policy);
                assert_eq!(decoded.preserve_xattrs(), xattrs);
                assert_eq!(decoded.preserve_acls(), acls);
            }
        }
    }

    #[test]
    fn file_fetch_request_rejects_truncation_and_trailing_data() {
        let encoded = request(WireFetchCompression::Always).encode();
        for len in 0..encoded.len() {
            assert!(WireFileFetchRequest::decode(&encoded[..len]).is_err());
        }
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireFileFetchRequest::decode(&trailing).is_err());
    }

    #[test]
    fn file_fetch_request_rejects_every_unknown_policy() {
        let mut encoded = request(WireFetchCompression::Always).encode().to_vec();
        for value in 3..=u8::MAX {
            encoded[9 + FETCH_IDENTITY_LEN] = value;
            assert!(matches!(
                WireFileFetchRequest::decode(&encoded),
                Err(ProtocolError::InvalidField {
                    field: "file_fetch_compression",
                    ..
                })
            ));
        }
    }

    #[test]
    fn file_fetch_request_rejects_unknown_and_obsolete_flag_bits() {
        let mut encoded = request(WireFetchCompression::Auto).encode().to_vec();
        for bit in [0, 3, 4, 5, 6, 7] {
            encoded[8 + FETCH_IDENTITY_LEN] = 1 << bit;
            assert!(matches!(
                WireFileFetchRequest::decode(&encoded),
                Err(ProtocolError::InvalidField {
                    field: "file_fetch_flags",
                    ..
                })
            ));
        }
    }
}
