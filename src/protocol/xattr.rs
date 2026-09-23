use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WireEntryKind, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

/// Maximum total bytes of extended-attribute names plus values carried in one
/// request or result.
///
/// Xattr values are not intrinsically bounded by the filesystem (macOS keeps
/// large values in resource forks), so the protocol imposes its own cap well
/// below the frame payload limit and rejects larger sets loudly instead of
/// truncating them.
pub const MAX_XATTR_TOTAL_BYTES: usize = 512 * 1024;

/// Longest accepted attribute name. This covers Linux `XATTR_NAME_MAX` (255)
/// with headroom for other Unix naming rules; longer peer input is rejected
/// rather than truncated.
pub const MAX_XATTR_NAME_BYTES: usize = 255;

/// Upper bound on attributes per entry, independent of the byte cap. Keeps the
/// per-entry header overhead bounded even when every value is empty.
pub const MAX_XATTR_ENTRIES: usize = 4096;

/// One extended attribute: an opaque native name and its value.
///
/// Names are raw bytes rather than Rust strings because Unix attribute names
/// are byte sequences; the protocol must not silently lose non-UTF-8 names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireXattr {
    name: Bytes,
    value: Bytes,
}

impl WireXattr {
    pub fn new(name: impl Into<Bytes>, value: impl Into<Bytes>) -> Result<Self> {
        let name = name.into();
        let value = value.into();
        if name.is_empty() {
            return Err(ProtocolError::InvalidField {
                field: "xattr_name",
                reason: "attribute name is empty",
            });
        }
        if name.len() > MAX_XATTR_NAME_BYTES {
            return Err(ProtocolError::XattrNameTooLong {
                len: name.len(),
                max: MAX_XATTR_NAME_BYTES,
            });
        }
        if name.contains(&0) {
            return Err(ProtocolError::InvalidField {
                field: "xattr_name",
                reason: "attribute name contains a NUL byte",
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

/// Whether an xattr request reads the destination's current attributes or
/// replaces them with the carried set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum XattrMode {
    Read = 1,
    Write = 2,
}

impl TryFrom<u8> for XattrMode {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Read),
            2 => Ok(Self::Write),
            _ => Err(ProtocolError::InvalidField {
                field: "xattr_mode",
                reason: "unknown xattr mode",
            }),
        }
    }
}

/// One bounded extended-attribute request for an existing entry.
///
/// `Read` carries no attributes and the server answers with
/// [`WireXattrResult`]; `Write` carries the full desired set (possibly empty,
/// which clears every attribute) and the server answers with an empty
/// acknowledgement frame. Symlinks are rejected: Unix symlink attributes are
/// not a portable mutable property and the client never resolves link targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireXattrRequest {
    path: RelativeWirePath,
    kind: WireEntryKind,
    mode: XattrMode,
    entries: Vec<WireXattr>,
}

impl WireXattrRequest {
    pub fn read(path: RelativeWirePath, kind: WireEntryKind) -> Result<Self> {
        let request = Self {
            path,
            kind,
            mode: XattrMode::Read,
            entries: Vec::new(),
        };
        request.validate()?;
        Ok(request)
    }

    pub fn write(
        path: RelativeWirePath,
        kind: WireEntryKind,
        entries: Vec<WireXattr>,
    ) -> Result<Self> {
        let request = Self {
            path,
            kind,
            mode: XattrMode::Write,
            entries,
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

    pub const fn mode(&self) -> XattrMode {
        self.mode
    }

    pub fn entries(&self) -> &[WireXattr] {
        &self.entries
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let path_len = u32::try_from(self.path.as_encoded().len()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "xattr_path",
                reason: "encoded relative path length exceeds u32",
            }
        })?;
        let entries_len = encoded_entries_len(&self.entries)?;
        let mut capacity = 1_usize
            .checked_add(1)
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(self.path.as_encoded().len()))
            .ok_or(ProtocolError::InvalidMessage(
                "xattr request payload length overflow",
            ))?;
        if self.mode == XattrMode::Write {
            capacity = capacity
                .checked_add(2)
                .and_then(|value| value.checked_add(entries_len))
                .ok_or(ProtocolError::InvalidMessage(
                    "xattr request payload length overflow",
                ))?;
        }

        let mut out = BytesMut::with_capacity(capacity);
        out.put_u8(self.kind as u8);
        out.put_u8(self.mode as u8);
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        if self.mode == XattrMode::Write {
            out.put_u16(u16::try_from(self.entries.len()).map_err(|_| {
                ProtocolError::TooManyXattrs {
                    count: self.entries.len(),
                    max: MAX_XATTR_ENTRIES,
                }
            })?);
            for entry in &self.entries {
                put_entry(&mut out, entry);
            }
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let kind = WireEntryKind::try_from(reader.u8()?)?;
        let mode = XattrMode::try_from(reader.u8()?)?;
        let path_len = reader.u32()? as usize;
        if path_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: path_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(path_len)?))?;
        let entries = if mode == XattrMode::Write {
            read_entries(&mut reader)?
        } else {
            Vec::new()
        };
        reader.finish()?;
        Self {
            path,
            kind,
            mode,
            entries,
        }
        .validated()
    }

    /// Structural checks that do not depend on the wire encoding.
    fn validate(&self) -> Result<()> {
        validate_kind(self.kind)?;
        if self.mode == XattrMode::Read && !self.entries.is_empty() {
            return Err(ProtocolError::InvalidField {
                field: "xattr_entries",
                reason: "read requests must not carry attributes",
            });
        }
        validate_entries(&self.entries)
    }

    fn validated(self) -> Result<Self> {
        self.validate()?;
        Ok(self)
    }
}

/// One bounded extended-attribute set returned for a read request.
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
        let entries_len = encoded_entries_len(&self.entries)?;
        let mut out = BytesMut::with_capacity(2_usize.saturating_add(entries_len));
        out.put_u16(u16::try_from(self.entries.len()).map_err(|_| {
            ProtocolError::TooManyXattrs {
                count: self.entries.len(),
                max: MAX_XATTR_ENTRIES,
            }
        })?);
        for entry in &self.entries {
            put_entry(&mut out, entry);
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let entries = read_entries(&mut reader)?;
        reader.finish()?;
        Self::new(entries)
    }
}

fn put_entry(out: &mut BytesMut, entry: &WireXattr) {
    // `WireXattr` construction already bounded the name length to a u16, so
    // the conversions below cannot truncate.
    out.put_u16(u16::try_from(entry.name.len()).unwrap_or(u16::MAX));
    out.extend_from_slice(&entry.name);
    out.put_u32(u32::try_from(entry.value.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(&entry.value);
}

fn read_entries(reader: &mut SliceReader<'_>) -> Result<Vec<WireXattr>> {
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
        if name_len == 0 {
            return Err(ProtocolError::InvalidField {
                field: "xattr_name",
                reason: "attribute name is empty",
            });
        }
        if name_len > MAX_XATTR_NAME_BYTES {
            return Err(ProtocolError::XattrNameTooLong {
                len: name_len,
                max: MAX_XATTR_NAME_BYTES,
            });
        }
        let name = Bytes::copy_from_slice(reader.take(name_len)?);
        let value_len = reader.u32()? as usize;
        total = total
            .checked_add(name.len())
            .and_then(|value| value.checked_add(value_len))
            .ok_or(ProtocolError::InvalidMessage(
                "xattr payload length overflow",
            ))?;
        if total > MAX_XATTR_TOTAL_BYTES {
            return Err(ProtocolError::XattrPayloadTooLarge {
                len: total,
                max: MAX_XATTR_TOTAL_BYTES,
            });
        }
        // Validate the announced value length before taking the bytes so an
        // oversized value never drives a large allocation attempt.
        let value = Bytes::copy_from_slice(reader.take(value_len)?);
        entries.push(WireXattr::new(name, value)?);
    }
    Ok(entries)
}

fn encoded_entries_len(entries: &[WireXattr]) -> Result<usize> {
    let mut total = 0_usize;
    for entry in entries {
        total = total
            .checked_add(2)
            .and_then(|value| value.checked_add(entry.name.len()))
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(entry.value.len()))
            .ok_or(ProtocolError::InvalidMessage(
                "xattr payload length overflow",
            ))?;
    }
    Ok(total)
}

fn validate_kind(kind: WireEntryKind) -> Result<()> {
    if kind == WireEntryKind::Symlink {
        return Err(ProtocolError::InvalidField {
            field: "xattr_kind",
            reason: "extended attributes are unsupported for symlinks",
        });
    }
    Ok(())
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
            .and_then(|value| value.checked_add(entry.value.len()))
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

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"entry".as_slice()]).unwrap()
    }

    fn xattr(name: &[u8], value: &[u8]) -> WireXattr {
        WireXattr::new(Bytes::copy_from_slice(name), Bytes::copy_from_slice(value)).unwrap()
    }

    #[test]
    fn read_request_round_trips_without_entries() {
        let request = WireXattrRequest::read(path(), WireEntryKind::File).unwrap();
        let decoded = WireXattrRequest::decode(&request.encode().unwrap()).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.mode(), XattrMode::Read);
        assert!(decoded.entries().is_empty());
    }

    #[test]
    fn write_request_round_trips_attributes_including_empty_values() {
        let request = WireXattrRequest::write(
            path(),
            WireEntryKind::Directory,
            vec![
                xattr(b"user.empty", b""),
                xattr(b"user.binary", &[0, 1, 2, 255]),
            ],
        )
        .unwrap();
        let decoded = WireXattrRequest::decode(&request.encode().unwrap()).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.mode(), XattrMode::Write);
        assert_eq!(decoded.entries().len(), 2);
    }

    #[test]
    fn write_request_allows_empty_set_that_clears_attributes() {
        let request = WireXattrRequest::write(path(), WireEntryKind::File, Vec::new()).unwrap();
        let decoded = WireXattrRequest::decode(&request.encode().unwrap()).unwrap();
        assert_eq!(decoded, request);
        assert!(decoded.entries().is_empty());
    }

    #[test]
    fn result_round_trips_and_reports_entries() {
        let result =
            WireXattrResult::new(vec![xattr(b"user.a", b"1"), xattr(b"user.b", b"2")]).unwrap();
        let decoded = WireXattrResult::decode(&result.encode().unwrap()).unwrap();
        assert_eq!(decoded.entries().len(), 2);
        assert_eq!(decoded.entries()[0].name(), b"user.a");
        assert_eq!(decoded.entries()[1].value(), b"2");
    }

    #[test]
    fn symlink_kind_is_rejected_before_encoding() {
        assert!(WireXattrRequest::read(path(), WireEntryKind::Symlink).is_err());
    }

    #[test]
    fn name_and_total_bounds_fail_loudly() {
        let long_name = vec![b'a'; MAX_XATTR_NAME_BYTES + 1];
        assert!(matches!(
            WireXattr::new(Bytes::from(long_name), Bytes::new()),
            Err(ProtocolError::XattrNameTooLong { .. })
        ));
        assert!(WireXattr::new(Bytes::new(), Bytes::new()).is_err());
        assert!(WireXattr::new(Bytes::from_static(b"user\0bad"), Bytes::new()).is_err());

        let oversized = vec![xattr(b"user.big", &vec![0_u8; MAX_XATTR_TOTAL_BYTES])];
        assert!(matches!(
            WireXattrRequest::write(path(), WireEntryKind::File, oversized),
            Err(ProtocolError::XattrPayloadTooLarge { .. })
        ));
    }

    #[test]
    fn decoder_rejects_truncation_unknown_mode_and_trailing_data() {
        let encoded = WireXattrRequest::write(
            path(),
            WireEntryKind::File,
            vec![xattr(b"user.a", b"value")],
        )
        .unwrap()
        .encode()
        .unwrap();
        for len in 0..encoded.len() {
            assert!(WireXattrRequest::decode(&encoded[..len]).is_err());
        }
        let mut unknown = encoded.to_vec();
        unknown[1] = u8::MAX;
        assert!(matches!(
            WireXattrRequest::decode(&unknown),
            Err(ProtocolError::InvalidField {
                field: "xattr_mode",
                ..
            })
        ));
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireXattrRequest::decode(&trailing).is_err());
    }

    #[test]
    fn decoder_rejects_oversized_value_length_before_allocating_it() {
        // Hand-built payload: file kind, write mode, one path component, then
        // a single entry announcing a value length beyond the total cap.
        let path = path();
        let mut out = BytesMut::new();
        out.put_u8(WireEntryKind::File as u8);
        out.put_u8(XattrMode::Write as u8);
        out.put_u32(u32::try_from(path.as_encoded().len()).unwrap());
        out.extend_from_slice(path.as_encoded());
        out.put_u16(1);
        out.put_u16(8);
        out.extend_from_slice(b"user.big");
        out.put_u32(u32::try_from(MAX_XATTR_TOTAL_BYTES + 1).unwrap());
        let payload = out.freeze();
        assert!(matches!(
            WireXattrRequest::decode(&payload),
            Err(ProtocolError::XattrPayloadTooLarge { .. })
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_xattr_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..8192)) {
            let _ = WireXattrRequest::decode(&payload);
            let _ = WireXattrResult::decode(&payload);
        }
    }
}
