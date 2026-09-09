use super::codec::SliceReader;
use super::transfer::TRANSFER_BASIS_IDENTITY_LEN;
use super::{ProtocolError, RelativeWirePath, Result};
use bytes::{BufMut, Bytes, BytesMut};

/// Identity length shared with the transfer path basis (`FileBegin`).
pub const FETCH_IDENTITY_LEN: usize = TRANSFER_BASIS_IDENTITY_LEN;

/// Client request for whole-file source data in a pull session.
///
/// The server must validate the entry identity (size plus the scan-reported
/// identity) before streaming: a source replaced or modified after the scan
/// must fail the fetch instead of delivering mixed bytes. The response is
/// `Data` frames (optionally zstd-compressed per frame) terminated by
/// `FileEnd` carrying the BLAKE3 digest of the uncompressed source bytes;
/// the client acknowledges only after its staged copy is verified and
/// committed, mirroring the push direction's ack-after-commit ordering.
/// Request flag: the client asked for per-chunk zstd (`-z`/`--compress`).
/// The per-frame COMPRESSED flag alone describes each payload, but the
/// server must not compress when this bit is absent — otherwise `-z` off
/// would be a silent no-op.
pub const FETCH_FLAG_COMPRESSED: u8 = 1 << 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireFileFetchRequest {
    pub path: RelativeWirePath,
    file_size: u64,
    identity: [u8; FETCH_IDENTITY_LEN],
    flags: u8,
}

impl WireFileFetchRequest {
    pub fn new(
        path: RelativeWirePath,
        file_size: u64,
        identity: [u8; FETCH_IDENTITY_LEN],
        compressed: bool,
    ) -> Self {
        Self {
            path,
            file_size,
            identity,
            flags: u8::from(compressed) * FETCH_FLAG_COMPRESSED,
        }
    }

    pub const fn compressed(&self) -> bool {
        self.flags & FETCH_FLAG_COMPRESSED != 0
    }

    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    pub const fn identity(&self) -> [u8; FETCH_IDENTITY_LEN] {
        self.identity
    }

    pub fn encode(&self) -> Bytes {
        let mut out =
            BytesMut::with_capacity(9 + FETCH_IDENTITY_LEN + self.path.as_encoded().len());
        out.put_u64(self.file_size);
        out.extend_from_slice(&self.identity);
        out.put_u8(self.flags);
        out.extend_from_slice(self.path.as_encoded());
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let file_size = reader.u64()?;
        let identity = reader.array::<FETCH_IDENTITY_LEN>()?;
        let flags = reader.u8()?;
        if flags & !FETCH_FLAG_COMPRESSED != 0 {
            return Err(ProtocolError::InvalidField {
                field: "file_fetch_flags",
                reason: "unknown fetch request flag bits",
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take_remaining()?))?;
        Ok(Self {
            path,
            file_size,
            identity,
            flags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> WireFileFetchRequest {
        let path =
            RelativeWirePath::from_components([b"dir".as_slice(), b"file.txt".as_slice()]).unwrap();
        WireFileFetchRequest::new(path, 4096, [7_u8; FETCH_IDENTITY_LEN], true)
    }

    #[test]
    fn file_fetch_request_round_trips() {
        let original = request();
        assert_eq!(
            WireFileFetchRequest::decode(&original.encode()).unwrap(),
            original
        );
    }

    #[test]
    fn file_fetch_request_rejects_truncation() {
        let encoded = request().encode();
        for len in 0..encoded.len() {
            assert!(WireFileFetchRequest::decode(&encoded[..len]).is_err());
        }
    }

    #[test]
    fn file_fetch_request_rejects_unknown_flag_bits() {
        let mut encoded = request().encode().to_vec();
        // flags byte sits after file_size (8) + identity (32)
        encoded[8 + FETCH_IDENTITY_LEN] |= 0x80;
        assert!(WireFileFetchRequest::decode(&encoded).is_err());
    }

    #[test]
    fn file_fetch_request_round_trips_compression_bit() {
        let path =
            RelativeWirePath::from_components([b"dir".as_slice(), b"file.txt".as_slice()]).unwrap();
        let off = WireFileFetchRequest::new(path, 4096, [7_u8; FETCH_IDENTITY_LEN], false);
        assert!(!off.compressed());
        assert!(!WireFileFetchRequest::decode(&off.encode())
            .unwrap()
            .compressed());
        assert!(WireFileFetchRequest::decode(&request().encode())
            .unwrap()
            .compressed());
    }

    #[test]
    fn file_fetch_request_rejects_trailing_data() {
        let mut encoded = request().encode().to_vec();
        encoded.push(0);
        assert!(WireFileFetchRequest::decode(&encoded).is_err());
    }
}
