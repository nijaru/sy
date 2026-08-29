use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result};
use bytes::{BufMut, Bytes, BytesMut};

pub const TRANSFER_DIGEST_LEN: usize = 32;
pub const TRANSFER_BASIS_IDENTITY_LEN: usize = 32;
pub const MAX_TRANSFER_DATA_SIZE: usize = 256 * 1024;
pub const MAX_DELTA_COPY_SIZE: u32 = 1024 * 1024;

const WHOLE_FILE_MODE: u8 = 0;
const DELTA_FILE_MODE: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireFileBasis {
    file_size: u64,
    identity: [u8; TRANSFER_BASIS_IDENTITY_LEN],
}

impl WireFileBasis {
    pub const fn new(file_size: u64, identity: [u8; TRANSFER_BASIS_IDENTITY_LEN]) -> Self {
        Self {
            file_size,
            identity,
        }
    }

    pub const fn file_size(self) -> u64 {
        self.file_size
    }

    pub const fn identity(self) -> [u8; TRANSFER_BASIS_IDENTITY_LEN] {
        self.identity
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireFileBegin {
    pub path: RelativeWirePath,
    file_size: u64,
    basis: Option<WireFileBasis>,
}

impl WireFileBegin {
    pub const fn whole(path: RelativeWirePath, file_size: u64) -> Self {
        Self {
            path,
            file_size,
            basis: None,
        }
    }

    pub const fn delta(path: RelativeWirePath, file_size: u64, basis: WireFileBasis) -> Self {
        Self {
            path,
            file_size,
            basis: Some(basis),
        }
    }

    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    pub const fn basis(&self) -> Option<WireFileBasis> {
        self.basis
    }

    pub fn encode(&self) -> Bytes {
        let basis_len = self.basis.map_or(0, |_| 8 + TRANSFER_BASIS_IDENTITY_LEN);
        let mut out = BytesMut::with_capacity(1 + 8 + basis_len + self.path.as_encoded().len());
        out.put_u8(if self.basis.is_some() {
            DELTA_FILE_MODE
        } else {
            WHOLE_FILE_MODE
        });
        out.put_u64(self.file_size);
        if let Some(basis) = self.basis {
            out.put_u64(basis.file_size);
            out.extend_from_slice(&basis.identity);
        }
        out.extend_from_slice(self.path.as_encoded());
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let mode = reader.u8()?;
        let file_size = reader.u64()?;
        let basis = match mode {
            WHOLE_FILE_MODE => None,
            DELTA_FILE_MODE => Some(WireFileBasis::new(
                reader.u64()?,
                reader.array::<TRANSFER_BASIS_IDENTITY_LEN>()?,
            )),
            _ => {
                return Err(ProtocolError::InvalidField {
                    field: "file_begin_mode",
                    reason: "unknown transfer mode",
                });
            }
        };
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take_remaining()?))?;
        reader.finish()?;
        Ok(Self {
            path,
            file_size,
            basis,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireData(Bytes);

impl WireData {
    pub fn new(data: impl Into<Bytes>) -> Result<Self> {
        let data = data.into();
        validate_data_len(data.len())?;
        Ok(Self(data))
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        Self::new(Bytes::copy_from_slice(payload))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireDeltaCopy {
    basis_offset: u64,
    len: u32,
}

impl WireDeltaCopy {
    pub fn new(basis_offset: u64, len: u32) -> Result<Self> {
        if len == 0 {
            return Err(ProtocolError::InvalidField {
                field: "delta_copy_len",
                reason: "copy length must be non-zero",
            });
        }
        if len > MAX_DELTA_COPY_SIZE {
            return Err(ProtocolError::InvalidField {
                field: "delta_copy_len",
                reason: "copy length exceeds the protocol maximum",
            });
        }
        basis_offset
            .checked_add(u64::from(len))
            .ok_or(ProtocolError::InvalidField {
                field: "delta_copy_range",
                reason: "copy range overflows u64",
            })?;
        Ok(Self { basis_offset, len })
    }

    pub const fn basis_offset(self) -> u64 {
        self.basis_offset
    }

    pub const fn copy_len(self) -> u32 {
        self.len
    }

    pub fn end(self) -> Result<u64> {
        self.basis_offset
            .checked_add(u64::from(self.len))
            .ok_or(ProtocolError::InvalidField {
                field: "delta_copy_range",
                reason: "copy range overflows u64",
            })
    }

    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(12);
        out.put_u64(self.basis_offset);
        out.put_u32(self.len);
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let basis_offset = reader.u64()?;
        let len = reader.u32()?;
        reader.finish()?;
        Self::new(basis_offset, len)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireFileEnd {
    file_size: u64,
    digest: [u8; TRANSFER_DIGEST_LEN],
}

impl WireFileEnd {
    pub const fn new(file_size: u64, digest: [u8; TRANSFER_DIGEST_LEN]) -> Self {
        Self { file_size, digest }
    }

    pub const fn file_size(self) -> u64 {
        self.file_size
    }

    pub const fn digest(self) -> [u8; TRANSFER_DIGEST_LEN] {
        self.digest
    }

    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(8 + TRANSFER_DIGEST_LEN);
        out.put_u64(self.file_size);
        out.extend_from_slice(&self.digest);
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let file_size = reader.u64()?;
        let digest = reader.array::<TRANSFER_DIGEST_LEN>()?;
        reader.finish()?;
        Ok(Self::new(file_size, digest))
    }
}

fn validate_data_len(len: usize) -> Result<()> {
    if len == 0 {
        return Err(ProtocolError::InvalidField {
            field: "data",
            reason: "data payload must be non-empty",
        });
    }
    if len > MAX_TRANSFER_DATA_SIZE {
        return Err(ProtocolError::PayloadTooLarge {
            len,
            max: MAX_TRANSFER_DATA_SIZE,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"file.bin".as_slice()]).unwrap()
    }

    #[test]
    fn file_begin_round_trips_whole_and_delta_modes() {
        let whole = WireFileBegin::whole(path(), 123);
        assert_eq!(WireFileBegin::decode(&whole.encode()).unwrap(), whole);
        assert_eq!(whole.basis(), None);

        let basis = WireFileBasis::new(456, [0x5a; TRANSFER_BASIS_IDENTITY_LEN]);
        let delta = WireFileBegin::delta(path(), 123, basis);
        assert_eq!(WireFileBegin::decode(&delta.encode()).unwrap(), delta);
        assert_eq!(delta.basis(), Some(basis));
    }

    #[test]
    fn file_begin_rejects_unknown_mode_and_truncation() {
        let encoded = WireFileBegin::whole(path(), 123).encode();
        for len in 0..encoded.len() {
            assert!(WireFileBegin::decode(&encoded[..len]).is_err());
        }

        let mut invalid = encoded.to_vec();
        invalid[0] = u8::MAX;
        assert!(matches!(
            WireFileBegin::decode(&invalid),
            Err(ProtocolError::InvalidField {
                field: "file_begin_mode",
                ..
            })
        ));
    }

    #[test]
    fn data_chunk_enforces_transfer_bound() {
        assert!(WireData::new(Bytes::new()).is_err());
        assert!(WireData::new(vec![0_u8; MAX_TRANSFER_DATA_SIZE]).is_ok());
        assert!(matches!(
            WireData::new(vec![0_u8; MAX_TRANSFER_DATA_SIZE + 1]),
            Err(ProtocolError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn delta_copy_round_trip_and_range_validation() {
        let copy = WireDeltaCopy::new(4096, 8192).unwrap();
        assert_eq!(WireDeltaCopy::decode(&copy.encode()).unwrap(), copy);
        assert_eq!(copy.end().unwrap(), 12_288);
        assert!(WireDeltaCopy::new(0, 0).is_err());
        assert!(WireDeltaCopy::new(0, MAX_DELTA_COPY_SIZE + 1).is_err());
        assert!(WireDeltaCopy::new(u64::MAX, 1).is_err());

        let mut trailing = copy.encode().to_vec();
        trailing.push(0);
        assert!(WireDeltaCopy::decode(&trailing).is_err());
    }

    #[test]
    fn file_end_round_trip_and_exact_length() {
        let end = WireFileEnd::new(123, [0xa5; TRANSFER_DIGEST_LEN]);
        assert_eq!(WireFileEnd::decode(&end.encode()).unwrap(), end);
        let mut trailing = end.encode().to_vec();
        trailing.push(0);
        assert!(WireFileEnd::decode(&trailing).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_transfer_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireFileBegin::decode(&payload);
            let _ = WireData::decode(&payload);
            let _ = WireDeltaCopy::decode(&payload);
            let _ = WireFileEnd::decode(&payload);
        }
    }
}
