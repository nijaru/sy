//! One observation-bound demand request shared by source preservation RPCs.
use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WireEntryKind, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

/// Source-only authority. Destination preservation belongs to `WireMetadata`
/// or directory finalization; there is no standalone path-based write request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireSourceMetadataRead {
    path: RelativeWirePath,
    kind: WireEntryKind,
    identity: [u8; 32],
}

impl WireSourceMetadataRead {
    pub fn new(path: RelativeWirePath, kind: WireEntryKind, identity: [u8; 32]) -> Result<Self> {
        if kind == WireEntryKind::Symlink {
            return Err(ProtocolError::InvalidMessage(
                "symlink preservation reads are unsupported",
            ));
        }
        Ok(Self {
            path,
            kind,
            identity,
        })
    }
    pub const fn path(&self) -> &RelativeWirePath {
        &self.path
    }
    pub const fn kind(&self) -> WireEntryKind {
        self.kind
    }
    pub const fn identity(&self) -> [u8; 32] {
        self.identity
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut out = BytesMut::with_capacity(6 + self.path.as_encoded().len() + 32);
        out.put_u8(self.kind as u8);
        out.put_u8(1); // Source read; former standalone write tag 2 is invalid.
        out.put_u32(
            u32::try_from(self.path.as_encoded().len())
                .map_err(|_| ProtocolError::InvalidMessage("source metadata path length"))?,
        );
        out.extend_from_slice(self.path.as_encoded());
        out.extend_from_slice(&self.identity);
        Ok(out.freeze())
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let kind = WireEntryKind::try_from(reader.u8()?)?;
        if reader.u8()? != 1 {
            return Err(ProtocolError::InvalidMessage(
                "source metadata requests are read-only",
            ));
        }
        let len = reader.u32()? as usize;
        if len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(len)?))?;
        let identity = super::entry::read_identity(&mut reader)?;
        reader.finish()?;
        Self::new(path, kind, identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn reads_require_complete_observation_and_reject_standalone_writes() {
        let path = RelativeWirePath::from_components([b"file".as_slice()]).unwrap();
        let request =
            WireSourceMetadataRead::new(path.clone(), WireEntryKind::File, [7; 32]).unwrap();
        let encoded = request.encode().unwrap();
        assert_eq!(WireSourceMetadataRead::decode(&encoded).unwrap(), request);
        for len in 0..encoded.len() {
            assert!(WireSourceMetadataRead::decode(&encoded[..len]).is_err());
        }
        let mut write = encoded.to_vec();
        write[1] = 2;
        assert!(WireSourceMetadataRead::decode(&write).is_err());
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireSourceMetadataRead::decode(&trailing).is_err());
        assert!(WireSourceMetadataRead::new(path, WireEntryKind::Symlink, [7; 32]).is_err());
    }
    proptest! {
        #[test]
        fn arbitrary_source_metadata_requests_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireSourceMetadataRead::decode(&payload);
        }
    }
}
