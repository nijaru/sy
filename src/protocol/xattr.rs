use super::codec::SliceReader;
use super::{ProtocolError, Result};
use bytes::{BufMut, Bytes, BytesMut};

/// Separate caps bound values, names, and per-entry allocation overhead.
pub const MAX_XATTR_TOTAL_BYTES: usize = 512 * 1024;
pub const MAX_XATTR_NAME_BYTES: usize = 255;
pub const MAX_XATTR_ENTRIES: usize = 4096;

/// Opaque native name and value; Unix names never pass through UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireXattr {
    name: Bytes,
    value: Bytes,
}
impl WireXattr {
    pub fn new(name: impl Into<Bytes>, value: impl Into<Bytes>) -> Result<Self> {
        let name = name.into();
        let value = value.into();
        if name.is_empty() || name.contains(&0) {
            return Err(ProtocolError::InvalidField {
                field: "xattr_name",
                reason: "attribute name is empty or contains NUL",
            });
        }
        if name.len() > MAX_XATTR_NAME_BYTES {
            return Err(ProtocolError::XattrNameTooLong {
                len: name.len(),
                max: MAX_XATTR_NAME_BYTES,
            });
        }
        Ok(Self { name, value })
    }
    pub fn name(&self) -> &[u8] {
        &self.name
    }
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// Complete bounded xattr set, used for source reads and observed mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireXattrResult {
    entries: Vec<WireXattr>,
}
impl WireXattrResult {
    pub fn new(entries: Vec<WireXattr>) -> Result<Self> {
        validate_entries(&entries)?;
        Ok(Self { entries })
    }
    pub fn entries(&self) -> &[WireXattr] {
        &self.entries
    }
    pub fn encode(&self) -> Result<Bytes> {
        validate_entries(&self.entries)?;
        let capacity = 2 + self
            .entries
            .iter()
            .map(|entry| 6 + entry.name.len() + entry.value.len())
            .sum::<usize>();
        let mut out = BytesMut::with_capacity(capacity);
        // Validated count, name and aggregate value bounds fit these widths.
        out.put_u16(self.entries.len() as u16);
        for entry in &self.entries {
            out.put_u16(entry.name.len() as u16);
            out.extend_from_slice(&entry.name);
            out.put_u32(entry.value.len() as u32);
            out.extend_from_slice(&entry.value);
        }
        Ok(out.freeze())
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let count = reader.u16()? as usize;
        if count > MAX_XATTR_ENTRIES {
            return Err(ProtocolError::TooManyXattrs {
                count,
                max: MAX_XATTR_ENTRIES,
            });
        }
        let mut entries = Vec::with_capacity(count.min(64));
        let mut total = 0_usize;
        for _ in 0..count {
            let name_len = reader.u16()? as usize;
            if name_len > MAX_XATTR_NAME_BYTES {
                return Err(ProtocolError::XattrNameTooLong {
                    len: name_len,
                    max: MAX_XATTR_NAME_BYTES,
                });
            }
            let name = reader.take(name_len)?;
            let value_len = reader.u32()? as usize;
            total = total
                .checked_add(name_len)
                .and_then(|n| n.checked_add(value_len))
                .ok_or(ProtocolError::InvalidMessage(
                    "xattr payload length overflow",
                ))?;
            if total > MAX_XATTR_TOTAL_BYTES {
                return Err(ProtocolError::XattrPayloadTooLarge {
                    len: total,
                    max: MAX_XATTR_TOTAL_BYTES,
                });
            }
            // Check announced lengths and availability before allocating either field.
            let value = reader.take(value_len)?;
            entries.push(WireXattr::new(
                Bytes::copy_from_slice(name),
                Bytes::copy_from_slice(value),
            )?);
        }
        reader.finish()?;
        Self::new(entries)
    }
}

fn validate_entries(entries: &[WireXattr]) -> Result<()> {
    if entries.len() > MAX_XATTR_ENTRIES {
        return Err(ProtocolError::TooManyXattrs {
            count: entries.len(),
            max: MAX_XATTR_ENTRIES,
        });
    }
    let mut total = 0_usize;
    for entry in entries {
        total = total
            .checked_add(entry.name.len())
            .and_then(|n| n.checked_add(entry.value.len()))
            .ok_or(ProtocolError::InvalidMessage(
                "xattr payload length overflow",
            ))?;
        if total > MAX_XATTR_TOTAL_BYTES {
            return Err(ProtocolError::XattrPayloadTooLarge {
                len: total,
                max: MAX_XATTR_TOTAL_BYTES,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn xattr_sets_preserve_empty_binary_and_non_utf8_fields() {
        let entries = vec![
            WireXattr::new(Bytes::from_static(b"user.empty"), Bytes::new()).unwrap(),
            WireXattr::new(
                Bytes::from_static(b"user.\xff"),
                Bytes::from_static(&[0, 1, 255]),
            )
            .unwrap(),
        ];
        for entries in [vec![], entries] {
            let result = WireXattrResult::new(entries).unwrap();
            let encoded = result.encode().unwrap();
            assert_eq!(WireXattrResult::decode(&encoded).unwrap(), result);
            for len in 0..encoded.len() {
                assert!(WireXattrResult::decode(&encoded[..len]).is_err());
            }
            let mut trailing = encoded.to_vec();
            trailing.push(0);
            assert!(WireXattrResult::decode(&trailing).is_err());
        }
    }
    #[test]
    fn names_and_aggregate_sets_fail_loudly_at_bounds() {
        assert!(WireXattr::new(Bytes::new(), Bytes::new()).is_err());
        assert!(WireXattr::new(Bytes::from_static(b"user\0bad"), Bytes::new()).is_err());
        assert!(matches!(
            WireXattr::new(vec![b'a'; MAX_XATTR_NAME_BYTES + 1], Bytes::new()),
            Err(ProtocolError::XattrNameTooLong { .. })
        ));
        assert!(matches!(
            WireXattrResult::new(vec![WireXattr::new(
                Bytes::from_static(b"user.big"),
                vec![0; MAX_XATTR_TOTAL_BYTES]
            )
            .unwrap()]),
            Err(ProtocolError::XattrPayloadTooLarge { .. })
        ));
    }
    #[test]
    fn decoder_checks_announced_count_and_value_before_allocation() {
        let count = ((MAX_XATTR_ENTRIES + 1) as u16).to_be_bytes();
        assert!(matches!(
            WireXattrResult::decode(&count),
            Err(ProtocolError::TooManyXattrs { .. })
        ));
        let mut payload = BytesMut::new();
        payload.put_u16(1);
        payload.put_u16(8);
        payload.extend_from_slice(b"user.big");
        payload.put_u32((MAX_XATTR_TOTAL_BYTES + 1) as u32);
        assert!(matches!(
            WireXattrResult::decode(&payload),
            Err(ProtocolError::XattrPayloadTooLarge { .. })
        ));
    }
    proptest! {
        #[test]
        fn arbitrary_xattr_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..8192)) {
            let _ = WireXattrResult::decode(&payload);
        }
    }
}
