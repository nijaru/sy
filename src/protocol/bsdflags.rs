use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WireEntryKind, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

/// Whether a BSD-flags request reads the entry's current flags or replaces
/// them with the carried value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BsdFlagsMode {
    Read = 1,
    Write = 2,
}

impl TryFrom<u8> for BsdFlagsMode {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Read),
            2 => Ok(Self::Write),
            _ => Err(ProtocolError::InvalidField {
                field: "bsd_flags_mode",
                reason: "unknown bsd flags mode",
            }),
        }
    }
}

/// One bounded BSD-flags request for an existing entry.
///
/// `Read` carries no value and the server answers with [`WireBsdFlagsResult`];
/// `Write` carries the full desired value (0 clears every flag) and the
/// server answers with an empty acknowledgement frame. Symlinks are
/// refused: like xattrs and ACLs, link-target resolution has no place in a
/// preservation RPC. The value is a fixed 4 bytes, so no size bound or
/// dedicated error variant is needed beyond the mode/kind checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireBsdFlagsRequest {
    path: RelativeWirePath,
    kind: WireEntryKind,
    mode: BsdFlagsMode,
    flags: Option<u32>,
}

impl WireBsdFlagsRequest {
    pub fn read(path: RelativeWirePath, kind: WireEntryKind) -> Result<Self> {
        let request = Self {
            path,
            kind,
            mode: BsdFlagsMode::Read,
            flags: None,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn write(path: RelativeWirePath, kind: WireEntryKind, flags: u32) -> Result<Self> {
        let request = Self {
            path,
            kind,
            mode: BsdFlagsMode::Write,
            flags: Some(flags),
        };
        request.validate()?;
        Ok(request)
    }

    pub const fn path(&self) -> &RelativeWirePath {
        &self.path
    }

    pub const fn kind(&self) -> WireEntryKind {
        self.kind
    }

    pub const fn mode(&self) -> BsdFlagsMode {
        self.mode
    }

    pub const fn flags(&self) -> Option<u32> {
        self.flags
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let path_len = u32::try_from(self.path.as_encoded().len()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "bsd_flags_path",
                reason: "encoded relative path length exceeds u32",
            }
        })?;
        let mut capacity = 1_usize
            .checked_add(1)
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(self.path.as_encoded().len()))
            .ok_or(ProtocolError::InvalidMessage(
                "bsd flags request payload length overflow",
            ))?;
        if self.mode == BsdFlagsMode::Write {
            capacity = capacity
                .checked_add(4)
                .ok_or(ProtocolError::InvalidMessage(
                    "bsd flags request payload length overflow",
                ))?;
        }

        let mut out = BytesMut::with_capacity(capacity);
        out.put_u8(self.kind as u8);
        out.put_u8(self.mode as u8);
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        if let Some(flags) = self.flags {
            out.put_u32(flags);
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let kind = WireEntryKind::try_from(reader.u8()?)?;
        let mode = BsdFlagsMode::try_from(reader.u8()?)?;
        let path_len = reader.u32()? as usize;
        if path_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: path_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(path_len)?))?;
        let flags = if mode == BsdFlagsMode::Write {
            Some(reader.u32()?)
        } else {
            None
        };
        reader.finish()?;
        Self {
            path,
            kind,
            mode,
            flags,
        }
        .validated()
    }

    /// Structural checks that do not depend on the wire encoding.
    fn validate(&self) -> Result<()> {
        validate_kind(self.kind)?;
        match (self.mode, &self.flags) {
            (BsdFlagsMode::Read, None) | (BsdFlagsMode::Write, Some(_)) => Ok(()),
            (BsdFlagsMode::Read, Some(_)) => Err(ProtocolError::InvalidField {
                field: "bsd_flags_value",
                reason: "read requests must not carry flags",
            }),
            (BsdFlagsMode::Write, None) => Err(ProtocolError::InvalidField {
                field: "bsd_flags_value",
                reason: "write requests must carry flags",
            }),
        }
    }

    fn validated(self) -> Result<Self> {
        self.validate()?;
        Ok(self)
    }
}

/// The BSD-flags value returned for a read request.
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

fn validate_kind(kind: WireEntryKind) -> Result<()> {
    if kind == WireEntryKind::Symlink {
        return Err(ProtocolError::InvalidField {
            field: "bsd_flags_kind",
            reason: "bsd flags are unsupported for symlinks",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"entry".as_slice()]).unwrap()
    }

    #[test]
    fn read_request_round_trips_without_value() {
        let request = WireBsdFlagsRequest::read(path(), WireEntryKind::File).unwrap();
        let decoded = WireBsdFlagsRequest::decode(&request.encode().unwrap()).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.mode(), BsdFlagsMode::Read);
        assert!(decoded.flags().is_none());
    }

    #[test]
    fn write_request_round_trips_value_including_zero_clear() {
        for flags in [0, 0x8000] {
            let request =
                WireBsdFlagsRequest::write(path(), WireEntryKind::Directory, flags).unwrap();
            let decoded = WireBsdFlagsRequest::decode(&request.encode().unwrap()).unwrap();
            assert_eq!(decoded, request);
            assert_eq!(decoded.mode(), BsdFlagsMode::Write);
            assert_eq!(decoded.flags(), Some(flags));
        }
    }

    #[test]
    fn result_round_trips_and_reports_value() {
        let result = WireBsdFlagsResult::new(0x8000);
        let decoded = WireBsdFlagsResult::decode(&result.encode().unwrap()).unwrap();
        assert_eq!(decoded.flags(), 0x8000);
    }

    #[test]
    fn symlink_kind_is_rejected_before_encoding() {
        assert!(WireBsdFlagsRequest::read(path(), WireEntryKind::Symlink).is_err());
    }

    #[test]
    fn decoder_rejects_truncation_unknown_mode_and_trailing_data() {
        let encoded = WireBsdFlagsRequest::write(path(), WireEntryKind::File, 1)
            .unwrap()
            .encode()
            .unwrap();
        for len in 0..encoded.len() {
            assert!(WireBsdFlagsRequest::decode(&encoded[..len]).is_err());
        }
        let mut unknown = encoded.to_vec();
        unknown[1] = u8::MAX;
        assert!(matches!(
            WireBsdFlagsRequest::decode(&unknown),
            Err(ProtocolError::InvalidField {
                field: "bsd_flags_mode",
                ..
            })
        ));
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireBsdFlagsRequest::decode(&trailing).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_bsd_flags_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..8192)) {
            let _ = WireBsdFlagsRequest::decode(&payload);
            let _ = WireBsdFlagsResult::decode(&payload);
        }
    }
}
