//! Cheap read-only validation of a previously observed rooted entry.
use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WireEntryKind, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

/// Metadata-stream discriminator; distinct from entry kinds and directory actions.
pub const OBSERVATION_READ: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireObservationRead {
    pub path: RelativeWirePath,
    pub kind: WireEntryKind,
    pub identity: [u8; 32],
}

impl WireObservationRead {
    pub fn encode(&self) -> Result<Bytes> {
        let mut out = BytesMut::with_capacity(38 + self.path.as_encoded().len());
        out.put_u8(OBSERVATION_READ);
        out.put_u8(self.kind as u8);
        out.put_u32(
            u32::try_from(self.path.as_encoded().len())
                .map_err(|_| ProtocolError::InvalidMessage("observation path length"))?,
        );
        out.extend_from_slice(self.path.as_encoded());
        out.extend_from_slice(&self.identity);
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        if reader.u8()? != OBSERVATION_READ {
            return Err(ProtocolError::InvalidMessage(
                "observation read discriminator",
            ));
        }
        let kind = WireEntryKind::try_from(reader.u8()?)?;
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
        Ok(Self {
            path,
            kind,
            identity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn reads_require_a_complete_bounded_observation() {
        for kind in [
            WireEntryKind::File,
            WireEntryKind::Directory,
            WireEntryKind::Symlink,
        ] {
            let read = WireObservationRead {
                path: RelativeWirePath::from_components([b"entry".as_slice()]).unwrap(),
                kind,
                identity: [7; 32],
            };
            let encoded = read.encode().unwrap();
            assert_eq!(WireObservationRead::decode(&encoded).unwrap(), read);
            for len in 0..encoded.len() {
                assert!(WireObservationRead::decode(&encoded[..len]).is_err());
            }
            let mut invalid = encoded.to_vec();
            invalid[1] = 255;
            assert!(WireObservationRead::decode(&invalid).is_err());
            let mut oversized = encoded.to_vec();
            oversized[2..6].copy_from_slice(&u32::MAX.to_be_bytes());
            assert!(matches!(
                WireObservationRead::decode(&oversized),
                Err(ProtocolError::PathTooLong { .. })
            ));
            let mut trailing = encoded.to_vec();
            trailing.push(0);
            assert!(WireObservationRead::decode(&trailing).is_err());
        }
    }

    proptest! {
        #[test]
        fn arbitrary_observation_reads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireObservationRead::decode(&payload);
        }
    }
}
