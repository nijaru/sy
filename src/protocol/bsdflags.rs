use super::codec::SliceReader;
use super::Result;
use bytes::{BufMut, Bytes, BytesMut};

/// Fixed-size BSD flags returned by an observed source read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireBsdFlagsResult {
    flags: u32,
}
impl WireBsdFlagsResult {
    pub const fn new(flags: u32) -> Self {
        Self { flags }
    }
    pub const fn flags(&self) -> u32 {
        self.flags
    }
    pub fn encode(&self) -> Result<Bytes> {
        let mut out = BytesMut::with_capacity(4);
        out.put_u32(self.flags);
        Ok(out.freeze())
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let flags = reader.u32()?;
        reader.finish()?;
        Ok(Self { flags })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn flags_results_round_trip_without_accepting_incomplete_or_trailing_values(flags in any::<u32>()) {
            let result = WireBsdFlagsResult::new(flags);
            let encoded = result.encode().unwrap();
            prop_assert_eq!(WireBsdFlagsResult::decode(&encoded).unwrap(), result);
            for len in 0..4 { prop_assert!(WireBsdFlagsResult::decode(&encoded[..len]).is_err()); }
            let mut extra = encoded.to_vec(); extra.push(0);
            prop_assert!(WireBsdFlagsResult::decode(&extra).is_err());
        }
    }
}
