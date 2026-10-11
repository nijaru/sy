use super::codec::SliceReader;
use super::{ProtocolError, Result};
use bytes::{BufMut, Bytes, BytesMut};

/// Hard cap on exacl unified-entry text, shared by source reads and mutations.
pub const MAX_ACL_TEXT_BYTES: usize = 64 * 1024;

/// Unified exacl text. An empty string requests clearing the ACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireAcl {
    text: String,
}

impl WireAcl {
    pub fn new(text: String) -> Result<Self> {
        if text.len() > MAX_ACL_TEXT_BYTES {
            return Err(ProtocolError::AclTextTooLarge {
                len: text.len(),
                max: MAX_ACL_TEXT_BYTES,
            });
        }
        Ok(Self { text })
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(4 + self.text.len());
        // Construction caps text far below u32::MAX.
        out.put_u32(self.text.len() as u32);
        out.extend_from_slice(self.text.as_bytes());
        out.freeze()
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let len = reader.u32()? as usize;
        // Refuse the announced length before any text allocation.
        if len > MAX_ACL_TEXT_BYTES {
            return Err(ProtocolError::AclTextTooLarge {
                len,
                max: MAX_ACL_TEXT_BYTES,
            });
        }
        let text =
            std::str::from_utf8(reader.take(len)?).map_err(|_| ProtocolError::InvalidField {
                field: "acl_text",
                reason: "ACL text is not valid UTF-8",
            })?;
        reader.finish()?;
        Self::new(text.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireAclResult {
    acl: WireAcl,
}
impl WireAclResult {
    pub fn new(acl: WireAcl) -> Self {
        Self { acl }
    }
    pub const fn acl(&self) -> &WireAcl {
        &self.acl
    }
    pub fn encode(&self) -> Result<Bytes> {
        Ok(self.acl.encode())
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        Ok(Self::new(WireAcl::decode(payload)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn acl_text_round_trips_including_explicit_clear() {
        for text in ["", "allow::user:nick:read\n"] {
            let result = WireAclResult::new(WireAcl::new(text.to_owned()).unwrap());
            let encoded = result.encode().unwrap();
            assert_eq!(WireAclResult::decode(&encoded).unwrap(), result);
            for len in 0..encoded.len() {
                assert!(WireAclResult::decode(&encoded[..len]).is_err());
            }
            let mut trailing = encoded.to_vec();
            trailing.push(0);
            assert!(WireAclResult::decode(&trailing).is_err());
        }
    }
    #[test]
    fn acl_decoder_refuses_oversized_announcements_and_non_utf8_before_allocation() {
        let oversized = ((MAX_ACL_TEXT_BYTES + 1) as u32).to_be_bytes();
        assert!(matches!(
            WireAcl::decode(&oversized),
            Err(ProtocolError::AclTextTooLarge { .. })
        ));
        assert!(WireAcl::new("x".repeat(MAX_ACL_TEXT_BYTES + 1)).is_err());
        assert!(matches!(
            WireAcl::decode(&[0, 0, 0, 2, 0xff, 0xfe]),
            Err(ProtocolError::InvalidField {
                field: "acl_text",
                ..
            })
        ));
    }
    proptest! {
        #[test]
        fn arbitrary_acl_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..8192)) {
            let _ = WireAcl::decode(&payload);
        }
    }
}
