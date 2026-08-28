use super::codec::SliceReader;
use super::{
    ProtocolError, RelativeWirePath, Result, WirePath, MAX_WIRE_PATH_BYTES,
};
use bitflags::bitflags;
use bytes::{BufMut, Bytes, BytesMut};

const ENTRY_FIXED_BYTES: usize = 1 + 1 + 8 + 8 + 4 + 4;
const IDENTITY_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum WireEntryKind {
    File = 1,
    Directory = 2,
    Symlink = 3,
}

impl TryFrom<u8> for WireEntryKind {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::File),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Symlink),
            _ => Err(ProtocolError::InvalidField {
                field: "entry_kind",
                reason: "unknown entry kind",
            }),
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct EntryFields: u8 {
        const UNIX_MODE = 1 << 0;
        const IDENTITY = 1 << 1;
        const HARDLINK_GROUP = 1 << 2;
        const SYMLINK_TARGET = 1 << 3;
    }
}

/// Cheap ordered metadata record exchanged during reconciliation.
///
/// Native path components and symlink-target bytes remain opaque at this layer.
/// The endpoint adapter interprets them using the sender platform negotiated in
/// the hello exchange. Expensive metadata and signatures are separate protocol
/// requests and must not be added to this record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireEntry {
    pub path: RelativeWirePath,
    pub kind: WireEntryKind,
    pub size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: u32,
    pub unix_mode: Option<u32>,
    pub identity: Option<[u8; IDENTITY_BYTES]>,
    pub hardlink_group: Option<[u8; IDENTITY_BYTES]>,
    pub symlink_target: Option<WirePath>,
}

impl WireEntry {
    pub fn validate(&self) -> Result<()> {
        if self.modified_nanoseconds >= 1_000_000_000 {
            return Err(ProtocolError::InvalidField {
                field: "modified_nanoseconds",
                reason: "nanoseconds must be below 1,000,000,000",
            });
        }
        if self.kind != WireEntryKind::File && self.hardlink_group.is_some() {
            return Err(ProtocolError::InvalidField {
                field: "hardlink_group",
                reason: "hardlink group is only valid for regular files",
            });
        }
        match (self.kind, self.symlink_target.is_some()) {
            (WireEntryKind::Symlink, false) => {
                return Err(ProtocolError::InvalidField {
                    field: "symlink_target",
                    reason: "symlink entry is missing its target",
                })
            }
            (WireEntryKind::File | WireEntryKind::Directory, true) => {
                return Err(ProtocolError::InvalidField {
                    field: "symlink_target",
                    reason: "target is only valid for symlink entries",
                })
            }
            _ => {}
        }
        if self.kind == WireEntryKind::Directory && self.size != 0 {
            return Err(ProtocolError::InvalidField {
                field: "size",
                reason: "directory entry size must be zero",
            });
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let path_len = u32::try_from(self.path.as_encoded().len()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "path",
                reason: "encoded relative path length exceeds u32",
            }
        })?;
        let fields = self.fields();
        let target_len = match self.symlink_target.as_ref() {
            Some(target) => Some(u32::try_from(target.as_bytes().len()).map_err(|_| {
                ProtocolError::InvalidField {
                    field: "symlink_target",
                    reason: "symlink target length exceeds u32",
                }
            })?),
            None => None,
        };

        let mut capacity = ENTRY_FIXED_BYTES
            .checked_add(self.path.as_encoded().len())
            .ok_or(ProtocolError::InvalidMessage("entry payload length overflow"))?;
        if self.unix_mode.is_some() {
            capacity = capacity
                .checked_add(4)
                .ok_or(ProtocolError::InvalidMessage("entry payload length overflow"))?;
        }
        if self.identity.is_some() {
            capacity = capacity
                .checked_add(IDENTITY_BYTES)
                .ok_or(ProtocolError::InvalidMessage("entry payload length overflow"))?;
        }
        if self.hardlink_group.is_some() {
            capacity = capacity
                .checked_add(IDENTITY_BYTES)
                .ok_or(ProtocolError::InvalidMessage("entry payload length overflow"))?;
        }
        if let Some(target) = self.symlink_target.as_ref() {
            capacity = capacity
                .checked_add(4)
                .and_then(|value| value.checked_add(target.as_bytes().len()))
                .ok_or(ProtocolError::InvalidMessage("entry payload length overflow"))?;
        }

        let mut out = BytesMut::with_capacity(capacity);
        out.put_u8(self.kind as u8);
        out.put_u8(fields.bits());
        out.put_u64(self.size);
        out.put_i64(self.modified_seconds);
        out.put_u32(self.modified_nanoseconds);
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        if let Some(mode) = self.unix_mode {
            out.put_u32(mode);
        }
        if let Some(identity) = self.identity {
            out.extend_from_slice(&identity);
        }
        if let Some(group) = self.hardlink_group {
            out.extend_from_slice(&group);
        }
        if let (Some(target), Some(target_len)) = (self.symlink_target.as_ref(), target_len) {
            out.put_u32(target_len);
            out.extend_from_slice(target.as_bytes());
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let kind = WireEntryKind::try_from(reader.u8()?)?;
        let raw_fields = reader.u8()?;
        let fields = EntryFields::from_bits(raw_fields)
            .ok_or(ProtocolError::InvalidField {
                field: "entry_fields",
                reason: "unknown entry field bits",
            })?;
        let size = reader.u64()?;
        let modified_seconds = reader.i64()?;
        let modified_nanoseconds = reader.u32()?;
        if modified_nanoseconds >= 1_000_000_000 {
            return Err(ProtocolError::InvalidField {
                field: "modified_nanoseconds",
                reason: "nanoseconds must be below 1,000,000,000",
            });
        }

        let path_len = reader.u32()? as usize;
        if path_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: path_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(path_len)?))?;

        let unix_mode = fields
            .contains(EntryFields::UNIX_MODE)
            .then(|| reader.u32())
            .transpose()?;
        let identity = if fields.contains(EntryFields::IDENTITY) {
            Some(read_identity(&mut reader)?)
        } else {
            None
        };
        let hardlink_group = if fields.contains(EntryFields::HARDLINK_GROUP) {
            Some(read_identity(&mut reader)?)
        } else {
            None
        };
        let symlink_target = if fields.contains(EntryFields::SYMLINK_TARGET) {
            let len = reader.u32()? as usize;
            if len > MAX_WIRE_PATH_BYTES {
                return Err(ProtocolError::PathTooLong {
                    len,
                    max: MAX_WIRE_PATH_BYTES,
                });
            }
            Some(WirePath::new(Bytes::copy_from_slice(reader.take(len)?))?)
        } else {
            None
        };
        reader.finish()?;

        let entry = Self {
            path,
            kind,
            size,
            modified_seconds,
            modified_nanoseconds,
            unix_mode,
            identity,
            hardlink_group,
            symlink_target,
        };
        entry.validate()?;
        Ok(entry)
    }

    fn fields(&self) -> EntryFields {
        let mut fields = EntryFields::empty();
        fields.set(EntryFields::UNIX_MODE, self.unix_mode.is_some());
        fields.set(EntryFields::IDENTITY, self.identity.is_some());
        fields.set(
            EntryFields::HARDLINK_GROUP,
            self.hardlink_group.is_some(),
        );
        fields.set(
            EntryFields::SYMLINK_TARGET,
            self.symlink_target.is_some(),
        );
        fields
    }
}

fn read_identity(reader: &mut SliceReader<'_>) -> Result<[u8; IDENTITY_BYTES]> {
    let bytes = reader.take(IDENTITY_BYTES)?;
    let mut identity = [0_u8; IDENTITY_BYTES];
    identity.copy_from_slice(bytes);
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"\xffname".as_slice()]).unwrap()
    }

    #[test]
    fn regular_entry_round_trip() {
        let entry = WireEntry {
            path: path(),
            kind: WireEntryKind::File,
            size: 42,
            modified_seconds: -1,
            modified_nanoseconds: 999_999_999,
            unix_mode: Some(0o644),
            identity: Some([1; IDENTITY_BYTES]),
            hardlink_group: Some([2; IDENTITY_BYTES]),
            symlink_target: None,
        };
        assert_eq!(WireEntry::decode(&entry.encode().unwrap()).unwrap(), entry);
    }

    #[test]
    fn symlink_entry_round_trip_preserves_native_target() {
        let entry = WireEntry {
            path: path(),
            kind: WireEntryKind::Symlink,
            size: 0,
            modified_seconds: 1,
            modified_nanoseconds: 2,
            unix_mode: None,
            identity: Some([3; IDENTITY_BYTES]),
            hardlink_group: None,
            symlink_target: Some(
                WirePath::new(Bytes::from_static(&[b'.', 0, b'.', 0, b'\\', 0])).unwrap(),
            ),
        };
        assert_eq!(WireEntry::decode(&entry.encode().unwrap()).unwrap(), entry);
    }

    #[test]
    fn rejects_semantically_invalid_option_combinations() {
        let mut directory = WireEntry {
            path: path(),
            kind: WireEntryKind::Directory,
            size: 0,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            unix_mode: None,
            identity: None,
            hardlink_group: None,
            symlink_target: None,
        };
        directory.hardlink_group = Some([0; IDENTITY_BYTES]);
        assert!(directory.encode().is_err());

        directory.hardlink_group = None;
        directory.size = 1;
        assert!(directory.encode().is_err());
    }

    #[test]
    fn decoder_rejects_truncation_and_trailing_data() {
        let entry = WireEntry {
            path: path(),
            kind: WireEntryKind::File,
            size: 42,
            modified_seconds: 3,
            modified_nanoseconds: 4,
            unix_mode: None,
            identity: None,
            hardlink_group: None,
            symlink_target: None,
        };
        let encoded = entry.encode().unwrap();
        for len in 0..encoded.len() {
            assert!(WireEntry::decode(&encoded[..len]).is_err());
        }
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireEntry::decode(&trailing).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_entry_payload_never_panics(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireEntry::decode(&payload);
        }
    }
}
