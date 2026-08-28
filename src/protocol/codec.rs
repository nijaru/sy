use super::{ProtocolError, Result};

/// Checked cursor over a single bounded frame payload.
///
/// Frame decoding enforces the global payload bound before this type is created.
/// Message codecs use it to avoid unchecked indexing, integer-wrap bugs, and
/// duplicated truncation handling.
pub(super) struct SliceReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> SliceReader<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(super) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(ProtocolError::InvalidMessage("message length overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ProtocolError::InvalidMessage("truncated message payload"))?;
        self.offset = end;
        Ok(value)
    }

    pub(super) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(super) fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub(super) fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub(super) fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub(super) fn i64(&mut self) -> Result<i64> {
        let bytes = self.take(8)?;
        Ok(i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub(super) fn finish(self) -> Result<()> {
        if self.offset != self.bytes.len() {
            return Err(ProtocolError::InvalidMessage(
                "trailing bytes after message payload",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_cursor_rejects_truncation_and_trailing_bytes() {
        let mut reader = SliceReader::new(&[0, 1, 2]);
        assert_eq!(reader.u16().unwrap(), 1);
        assert!(reader.u16().is_err());

        let reader = SliceReader::new(&[1]);
        assert!(reader.finish().is_err());
        let reader = SliceReader::new(&[]);
        assert!(reader.finish().is_ok());
    }
}
