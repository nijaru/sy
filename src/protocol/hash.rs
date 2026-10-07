use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, MAX_WIRE_PATH_BYTES};
use crate::endpoint::existing::FingerprintOptions;
use bytes::{BufMut, Bytes, BytesMut};

pub const HASH_DIGEST_LEN: usize = 32;
pub const HASH_IDENTITY_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireHashRequest {
    pub path: RelativeWirePath,
    file_size: u64,
    identity: [u8; HASH_IDENTITY_LEN],
    preservation: FingerprintOptions,
}

impl WireHashRequest {
    pub const fn new(
        path: RelativeWirePath,
        file_size: u64,
        identity: [u8; HASH_IDENTITY_LEN],
        preservation: FingerprintOptions,
    ) -> Self {
        Self {
            path,
            file_size,
            identity,
            preservation,
        }
    }

    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    pub const fn identity(&self) -> &[u8; HASH_IDENTITY_LEN] {
        &self.identity
    }

    pub const fn preservation(&self) -> FingerprintOptions {
        self.preservation
    }

    pub fn encode(&self) -> Result<Bytes> {
        let path_len = u32::try_from(self.path.as_encoded().len()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "hash_path",
                reason: "encoded relative path length exceeds u32",
            }
        })?;
        let capacity = 4_usize
            .checked_add(self.path.as_encoded().len())
            .and_then(|value| value.checked_add(9 + HASH_IDENTITY_LEN))
            .ok_or(ProtocolError::InvalidMessage(
                "hash request payload length overflow",
            ))?;
        let mut out = BytesMut::with_capacity(capacity);
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        out.put_u64(self.file_size);
        out.extend_from_slice(&self.identity);
        out.put_u8(self.preservation.bits());
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let path_len = reader.u32()? as usize;
        if path_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: path_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(path_len)?))?;
        let file_size = reader.u64()?;
        let identity = reader.array::<HASH_IDENTITY_LEN>()?;
        let preservation = FingerprintOptions::from_bits(reader.u8()?).map_err(|_| {
            ProtocolError::InvalidField {
                field: "hash_preservation",
                reason: "unknown preservation selection bits",
            }
        })?;
        reader.finish()?;
        Ok(Self::new(path, file_size, identity, preservation))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireHashResult {
    file_size: u64,
    identity: [u8; HASH_IDENTITY_LEN],
    digest: [u8; HASH_DIGEST_LEN],
    preservation: [u8; HASH_DIGEST_LEN],
    binding: [u8; HASH_DIGEST_LEN],
}

impl WireHashResult {
    pub const fn new(
        file_size: u64,
        identity: [u8; HASH_IDENTITY_LEN],
        digest: [u8; HASH_DIGEST_LEN],
        preservation: [u8; HASH_DIGEST_LEN],
        binding: [u8; HASH_DIGEST_LEN],
    ) -> Self {
        Self {
            file_size,
            identity,
            digest,
            preservation,
            binding,
        }
    }

    pub const fn file_size(self) -> u64 {
        self.file_size
    }

    pub const fn identity(self) -> [u8; HASH_IDENTITY_LEN] {
        self.identity
    }

    pub const fn digest(self) -> [u8; HASH_DIGEST_LEN] {
        self.digest
    }

    pub const fn preservation(self) -> [u8; HASH_DIGEST_LEN] {
        self.preservation
    }

    pub const fn binding(self) -> [u8; HASH_DIGEST_LEN] {
        self.binding
    }

    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(8 + HASH_IDENTITY_LEN + 3 * HASH_DIGEST_LEN);
        out.put_u64(self.file_size);
        out.extend_from_slice(&self.identity);
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&self.preservation);
        out.extend_from_slice(&self.binding);
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let file_size = reader.u64()?;
        let identity = reader.array::<HASH_IDENTITY_LEN>()?;
        let digest = reader.array::<HASH_DIGEST_LEN>()?;
        let preservation = reader.array::<HASH_DIGEST_LEN>()?;
        let binding = reader.array::<HASH_DIGEST_LEN>()?;
        reader.finish()?;
        Ok(Self::new(
            file_size,
            identity,
            digest,
            preservation,
            binding,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"file".as_slice()]).unwrap()
    }

    #[test]
    fn hash_messages_round_trip() {
        for preservation in [
            FingerprintOptions::default(),
            FingerprintOptions {
                permissions: true,
                times: true,
                xattrs: true,
                acls: true,
                bsd_flags: true,
            },
        ] {
            let request = WireHashRequest::new(path(), 42, [7; HASH_IDENTITY_LEN], preservation);
            assert_eq!(
                WireHashRequest::decode(&request.encode().unwrap()).unwrap(),
                request
            );
        }

        let result = WireHashResult::new(
            42,
            [7; HASH_IDENTITY_LEN],
            [9; HASH_DIGEST_LEN],
            [8; HASH_DIGEST_LEN],
            [6; HASH_DIGEST_LEN],
        );
        assert_eq!(WireHashResult::decode(&result.encode()).unwrap(), result);
    }

    #[test]
    fn hash_decoders_reject_truncation_and_trailing_data() {
        let request = WireHashRequest::new(
            path(),
            42,
            [7; HASH_IDENTITY_LEN],
            FingerprintOptions::default(),
        )
        .encode()
        .unwrap();
        for len in 0..request.len() {
            assert!(WireHashRequest::decode(&request[..len]).is_err());
        }
        let mut trailing = request.to_vec();
        trailing.push(0);
        assert!(WireHashRequest::decode(&trailing).is_err());
        let mut unknown_selection = request.to_vec();
        *unknown_selection.last_mut().unwrap() = 32;
        assert!(matches!(
            WireHashRequest::decode(&unknown_selection),
            Err(ProtocolError::InvalidField {
                field: "hash_preservation",
                ..
            })
        ));

        let result = WireHashResult::new(
            42,
            [7; HASH_IDENTITY_LEN],
            [9; HASH_DIGEST_LEN],
            [8; HASH_DIGEST_LEN],
            [6; HASH_DIGEST_LEN],
        )
        .encode();
        for len in 0..result.len() {
            assert!(WireHashResult::decode(&result[..len]).is_err());
        }
        let mut trailing = result.to_vec();
        trailing.push(0);
        assert!(WireHashResult::decode(&trailing).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_hash_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireHashRequest::decode(&payload);
            let _ = WireHashResult::decode(&payload);
        }
    }
}
