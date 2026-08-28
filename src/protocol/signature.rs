use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result};
use bytes::{BufMut, Bytes, BytesMut};

pub const MIN_SIGNATURE_BLOCK_SIZE: u32 = 4 * 1024;
pub const MAX_SIGNATURE_BLOCK_SIZE: u32 = 1024 * 1024;
pub const STRONG_SIGNATURE_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureBlockSize(u32);

impl SignatureBlockSize {
    pub fn new(bytes: u32) -> Result<Self> {
        if bytes < MIN_SIGNATURE_BLOCK_SIZE {
            return Err(ProtocolError::InvalidField {
                field: "signature_block_size",
                reason: "block size is below the protocol minimum",
            });
        }
        if bytes > MAX_SIGNATURE_BLOCK_SIZE {
            return Err(ProtocolError::InvalidField {
                field: "signature_block_size",
                reason: "block size exceeds the protocol maximum",
            });
        }
        if !bytes.is_power_of_two() {
            return Err(ProtocolError::InvalidField {
                field: "signature_block_size",
                reason: "block size must be a power of two",
            });
        }
        Ok(Self(bytes))
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<usize> for SignatureBlockSize {
    type Error = ProtocolError;

    fn try_from(bytes: usize) -> Result<Self> {
        let bytes = u32::try_from(bytes).map_err(|_| ProtocolError::InvalidField {
            field: "signature_block_size",
            reason: "block size exceeds u32",
        })?;
        Self::new(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireSignatureRequest {
    pub path: RelativeWirePath,
    pub block_size: SignatureBlockSize,
}

impl WireSignatureRequest {
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(4 + self.path.as_encoded().len());
        out.put_u32(self.block_size.get());
        out.extend_from_slice(self.path.as_encoded());
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let block_size = SignatureBlockSize::new(reader.u32()?)?;
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take_remaining()?))?;
        reader.finish()?;
        Ok(Self { path, block_size })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireSignature {
    index: u64,
    size: u32,
    weak: u32,
    strong: [u8; STRONG_SIGNATURE_LEN],
}

impl WireSignature {
    pub fn new(
        index: u64,
        size: u32,
        weak: u32,
        strong: [u8; STRONG_SIGNATURE_LEN],
    ) -> Result<Self> {
        if size == 0 || size > MAX_SIGNATURE_BLOCK_SIZE {
            return Err(ProtocolError::InvalidField {
                field: "signature_size",
                reason: "signature block size must be within protocol bounds",
            });
        }
        Ok(Self {
            index,
            size,
            weak,
            strong,
        })
    }

    pub const fn index(self) -> u64 {
        self.index
    }

    pub const fn size(self) -> u32 {
        self.size
    }

    pub const fn weak(self) -> u32 {
        self.weak
    }

    pub const fn strong(self) -> [u8; STRONG_SIGNATURE_LEN] {
        self.strong
    }

    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(32);
        out.put_u64(self.index);
        out.put_u32(self.size);
        out.put_u32(self.weak);
        out.extend_from_slice(&self.strong);
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let index = reader.u64()?;
        let size = reader.u32()?;
        let weak = reader.u32()?;
        let strong = reader.array::<STRONG_SIGNATURE_LEN>()?;
        reader.finish()?;
        Self::new(index, size, weak, strong)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireSignatureEnd {
    file_size: u64,
    block_count: u64,
}

impl WireSignatureEnd {
    pub fn new(file_size: u64, block_count: u64) -> Result<Self> {
        if (file_size == 0) != (block_count == 0) {
            return Err(ProtocolError::InvalidField {
                field: "signature_end",
                reason: "empty file size and block count must agree",
            });
        }
        Ok(Self {
            file_size,
            block_count,
        })
    }

    pub const fn file_size(self) -> u64 {
        self.file_size
    }

    pub const fn block_count(self) -> u64 {
        self.block_count
    }

    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(16);
        out.put_u64(self.file_size);
        out.put_u64(self.block_count);
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let file_size = reader.u64()?;
        let block_count = reader.u64()?;
        reader.finish()?;
        Self::new(file_size, block_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"file.bin".as_slice()]).unwrap()
    }

    #[test]
    fn signature_request_round_trip() {
        let request = WireSignatureRequest {
            path: path(),
            block_size: SignatureBlockSize::new(64 * 1024).unwrap(),
        };
        assert_eq!(
            WireSignatureRequest::decode(&request.encode()).unwrap(),
            request
        );
    }

    #[test]
    fn signature_request_rejects_invalid_block_sizes() {
        let encoded_path = path().into_encoded();
        for block_size in [0_u32, 1024, 6000, 2 * 1024 * 1024] {
            let mut payload = BytesMut::with_capacity(4 + encoded_path.len());
            payload.put_u32(block_size);
            payload.extend_from_slice(&encoded_path);
            assert!(WireSignatureRequest::decode(&payload).is_err());
        }
    }

    #[test]
    fn signature_round_trip_and_exact_length() {
        let signature = WireSignature::new(7, 4096, 0x1234_5678, [0x5a; 16]).unwrap();
        assert_eq!(
            WireSignature::decode(&signature.encode()).unwrap(),
            signature
        );

        let mut trailing = signature.encode().to_vec();
        trailing.push(0);
        assert!(WireSignature::decode(&trailing).is_err());
    }

    #[test]
    fn signature_end_round_trip_and_empty_consistency() {
        let end = WireSignatureEnd::new(10_000, 3).unwrap();
        assert_eq!(WireSignatureEnd::decode(&end.encode()).unwrap(), end);
        assert!(WireSignatureEnd::new(0, 1).is_err());
        assert!(WireSignatureEnd::new(1, 0).is_err());
        assert!(WireSignatureEnd::new(0, 0).is_ok());
    }
}
