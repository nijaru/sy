//! Acquisition is tied to a planned Create address, never an arbitrary root.
use super::{ProtocolError, RelativeWirePath, Result, MAX_WIRE_PATH_BYTES};
use bytes::Bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRoot {
    pub create: RelativeWirePath,
}

impl AcquireRoot {
    pub fn encode(self) -> Bytes {
        self.create.into_encoded()
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        if payload.len() > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: payload.len(),
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        // Bound allocation first; RelativeWirePath validates component structure.
        Ok(Self {
            create: RelativeWirePath::decode(Bytes::copy_from_slice(payload))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn acquisition_target_is_exact_bounded_and_not_a_root_path() {
        let request = AcquireRoot {
            create: RelativeWirePath::from_components([b"native\xff".as_slice()]).unwrap(),
        };
        let encoded = request.clone().encode();
        assert_eq!(AcquireRoot::decode(&encoded).unwrap(), request);
        for len in 0..encoded.len() {
            assert!(AcquireRoot::decode(&encoded[..len]).is_err());
        }
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(AcquireRoot::decode(&trailing).is_err());
        assert!(matches!(
            AcquireRoot::decode(&vec![0; MAX_WIRE_PATH_BYTES + 1]),
            Err(ProtocolError::PathTooLong { .. })
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_acquisition_payload_never_panics(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = AcquireRoot::decode(&payload);
        }
    }
}
