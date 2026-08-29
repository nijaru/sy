use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WireEntryKind, MAX_WIRE_PATH_BYTES};
use bitflags::bitflags;
use bytes::{BufMut, Bytes, BytesMut};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MetadataFields: u8 {
        const UNIX_MODE = 1 << 0;
        const MODIFIED = 1 << 1;
    }
}

/// One bounded metadata-only update for an existing destination entry.
///
/// Presence bits express policy decisions made by the engine. The protocol
/// carries only the fields that should be applied; it does not decide which
/// metadata is preserved. Symlink permissions are intentionally unsupported
/// because Unix symlink mode bits are not a portable mutable property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMetadata {
    pub path: RelativeWirePath,
    kind: WireEntryKind,
    unix_mode: Option<u32>,
    modified: Option<(i64, u32)>,
}

impl WireMetadata {
    pub fn new(
        path: RelativeWirePath,
        kind: WireEntryKind,
        unix_mode: Option<u32>,
        modified: Option<(i64, u32)>,
    ) -> Result<Self> {
        let metadata = Self {
            path,
            kind,
            unix_mode,
            modified,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    pub const fn kind(&self) -> WireEntryKind {
        self.kind
    }

    pub const fn unix_mode(&self) -> Option<u32> {
        self.unix_mode
    }

    pub const fn modified(&self) -> Option<(i64, u32)> {
        self.modified
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let path_len = u32::try_from(self.path.as_encoded().len()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "metadata_path",
                reason: "encoded relative path length exceeds u32",
            }
        })?;
        let fields = self.fields();
        let mut capacity = 1_usize
            .checked_add(1)
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(self.path.as_encoded().len()))
            .ok_or(ProtocolError::InvalidMessage(
                "metadata payload length overflow",
            ))?;
        if self.unix_mode.is_some() {
            capacity = capacity
                .checked_add(4)
                .ok_or(ProtocolError::InvalidMessage(
                    "metadata payload length overflow",
                ))?;
        }
        if self.modified.is_some() {
            capacity = capacity
                .checked_add(12)
                .ok_or(ProtocolError::InvalidMessage(
                    "metadata payload length overflow",
                ))?;
        }

        let mut out = BytesMut::with_capacity(capacity);
        out.put_u8(self.kind as u8);
        out.put_u8(fields.bits());
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        if let Some(mode) = self.unix_mode {
            out.put_u32(mode);
        }
        if let Some((seconds, nanoseconds)) = self.modified {
            out.put_i64(seconds);
            out.put_u32(nanoseconds);
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let kind = WireEntryKind::try_from(reader.u8()?)?;
        let raw_fields = reader.u8()?;
        let fields = MetadataFields::from_bits(raw_fields).ok_or(ProtocolError::InvalidField {
            field: "metadata_fields",
            reason: "unknown metadata field bits",
        })?;
        let path_len = reader.u32()? as usize;
        if path_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: path_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(path_len)?))?;
        let unix_mode = fields
            .contains(MetadataFields::UNIX_MODE)
            .then(|| reader.u32())
            .transpose()?;
        let modified = if fields.contains(MetadataFields::MODIFIED) {
            Some((reader.i64()?, reader.u32()?))
        } else {
            None
        };
        reader.finish()?;
        Self::new(path, kind, unix_mode, modified)
    }

    fn fields(&self) -> MetadataFields {
        let mut fields = MetadataFields::empty();
        fields.set(MetadataFields::UNIX_MODE, self.unix_mode.is_some());
        fields.set(MetadataFields::MODIFIED, self.modified.is_some());
        fields
    }

    fn validate(&self) -> Result<()> {
        if self.unix_mode.is_none() && self.modified.is_none() {
            return Err(ProtocolError::InvalidField {
                field: "metadata_fields",
                reason: "metadata request must contain at least one field",
            });
        }
        if self.kind == WireEntryKind::Symlink && self.unix_mode.is_some() {
            return Err(ProtocolError::InvalidField {
                field: "unix_mode",
                reason: "symlink mode changes are unsupported",
            });
        }
        if self
            .modified
            .is_some_and(|(_, nanoseconds)| nanoseconds >= 1_000_000_000)
        {
            return Err(ProtocolError::InvalidField {
                field: "modified_nanoseconds",
                reason: "nanoseconds must be below 1,000,000,000",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"entry".as_slice()]).unwrap()
    }

    #[test]
    fn metadata_round_trip_preserves_requested_fields() {
        let metadata = WireMetadata::new(
            path(),
            WireEntryKind::File,
            Some(0o100640),
            Some((-1, 999_999_999)),
        )
        .unwrap();
        assert_eq!(WireMetadata::decode(&metadata.encode().unwrap()).unwrap(), metadata);
    }

    #[test]
    fn metadata_rejects_empty_symlink_mode_and_invalid_time() {
        assert!(WireMetadata::new(path(), WireEntryKind::File, None, None).is_err());
        assert!(WireMetadata::new(path(), WireEntryKind::Symlink, Some(0o777), None).is_err());
        assert!(WireMetadata::new(
            path(),
            WireEntryKind::Directory,
            None,
            Some((0, 1_000_000_000))
        )
        .is_err());
    }

    #[test]
    fn decoder_rejects_truncation_and_trailing_data() {
        let encoded = WireMetadata::new(
            path(),
            WireEntryKind::Directory,
            Some(0o755),
            Some((1, 2)),
        )
        .unwrap()
        .encode()
        .unwrap();
        for len in 0..encoded.len() {
            assert!(WireMetadata::decode(&encoded[..len]).is_err());
        }
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireMetadata::decode(&trailing).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_metadata_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireMetadata::decode(&payload);
        }
    }
}
