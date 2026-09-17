use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WireEntryKind, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

/// Maximum bytes of ACL text carried in one request or result.
///
/// An exacl unified-entries text is small in practice (a POSIX ACL with
/// dozens of entries serializes to a few kilobytes; macOS NFSv4-style entries
/// are longer but still far below this cap). The protocol imposes its own
/// bound well below the frame payload limit and rejects larger sets loudly
/// instead of truncating them.
pub const MAX_ACL_TEXT_BYTES: usize = 64 * 1024;

/// One access-control list in exacl unified-entries text form.
///
/// The text stays the exacl serialization (`allow:flags:kind:name:perms`
/// lines, access + default entries merged for directories) because the local
/// executor drives `LocalEndpoint::{read,write}_acl`, which uses exactly that
/// format: push, pull, and local agree on one representation. An empty string
/// means "no ACL" and clears the destination (on Linux the write side falls
/// back to the destination mode, matching `LocalEndpoint::write_acl`).
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
}

/// Whether an ACL request reads the entry's current list or replaces it with
/// the carried text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AclMode {
    Read = 1,
    Write = 2,
}

impl TryFrom<u8> for AclMode {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Read),
            2 => Ok(Self::Write),
            _ => Err(ProtocolError::InvalidField {
                field: "acl_mode",
                reason: "unknown acl mode",
            }),
        }
    }
}

/// One bounded access-control request for an existing entry.
///
/// `Read` carries no text and the server answers with [`WireAclResult`];
/// `Write` carries the full desired text (possibly empty, which clears the
/// list) and the server answers with an empty acknowledgement frame.
/// Symlinks are rejected: their ACLs are not a portable mutable property and
/// the client never resolves link targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireAclRequest {
    path: RelativeWirePath,
    kind: WireEntryKind,
    mode: AclMode,
    acl: Option<WireAcl>,
}

impl WireAclRequest {
    pub fn read(path: RelativeWirePath, kind: WireEntryKind) -> Result<Self> {
        let request = Self {
            path,
            kind,
            mode: AclMode::Read,
            acl: None,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn write(path: RelativeWirePath, kind: WireEntryKind, acl: WireAcl) -> Result<Self> {
        let request = Self {
            path,
            kind,
            mode: AclMode::Write,
            acl: Some(acl),
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

    pub const fn mode(&self) -> AclMode {
        self.mode
    }

    pub const fn acl(&self) -> Option<&WireAcl> {
        self.acl.as_ref()
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let path_len = u32::try_from(self.path.as_encoded().len()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "acl_path",
                reason: "encoded relative path length exceeds u32",
            }
        })?;
        let text_len = self.acl.as_ref().map_or(0, |acl| acl.text.len());
        let mut capacity = 1_usize
            .checked_add(1)
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(self.path.as_encoded().len()))
            .ok_or(ProtocolError::InvalidMessage(
                "acl request payload length overflow",
            ))?;
        if self.mode == AclMode::Write {
            capacity = capacity
                .checked_add(4)
                .and_then(|value| value.checked_add(text_len))
                .ok_or(ProtocolError::InvalidMessage(
                    "acl request payload length overflow",
                ))?;
        }

        let mut out = BytesMut::with_capacity(capacity);
        out.put_u8(self.kind as u8);
        out.put_u8(self.mode as u8);
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        if let Some(acl) = &self.acl {
            out.put_u32(u32::try_from(acl.text.len()).map_err(|_| {
                ProtocolError::AclTextTooLarge {
                    len: acl.text.len(),
                    max: MAX_ACL_TEXT_BYTES,
                }
            })?);
            out.extend_from_slice(acl.text.as_bytes());
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let kind = WireEntryKind::try_from(reader.u8()?)?;
        let mode = AclMode::try_from(reader.u8()?)?;
        let path_len = reader.u32()? as usize;
        if path_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: path_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(reader.take(path_len)?))?;
        let acl = if mode == AclMode::Write {
            let text_len = reader.u32()? as usize;
            if text_len > MAX_ACL_TEXT_BYTES {
                return Err(ProtocolError::AclTextTooLarge {
                    len: text_len,
                    max: MAX_ACL_TEXT_BYTES,
                });
            }
            let text = std::str::from_utf8(reader.take(text_len)?).map_err(|_| {
                ProtocolError::InvalidField {
                    field: "acl_text",
                    reason: "acl text is not valid UTF-8",
                }
            })?;
            Some(WireAcl::new(text.to_string())?)
        } else {
            None
        };
        reader.finish()?;
        Self {
            path,
            kind,
            mode,
            acl,
        }
        .validated()
    }

    /// Structural checks that do not depend on the wire encoding.
    fn validate(&self) -> Result<()> {
        validate_kind(self.kind)?;
        match (self.mode, &self.acl) {
            (AclMode::Read, None) | (AclMode::Write, Some(_)) => Ok(()),
            (AclMode::Read, Some(_)) => Err(ProtocolError::InvalidField {
                field: "acl_text",
                reason: "read requests must not carry an acl",
            }),
            (AclMode::Write, None) => Err(ProtocolError::InvalidField {
                field: "acl_text",
                reason: "write requests must carry an acl",
            }),
        }
    }

    fn validated(self) -> Result<Self> {
        self.validate()?;
        Ok(self)
    }
}

/// One bounded access-control list returned for a read request.
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
        let mut out = BytesMut::with_capacity(4_usize.saturating_add(self.acl.text.len()));
        out.put_u32(u32::try_from(self.acl.text.len()).map_err(|_| {
            ProtocolError::AclTextTooLarge {
                len: self.acl.text.len(),
                max: MAX_ACL_TEXT_BYTES,
            }
        })?);
        out.extend_from_slice(self.acl.text.as_bytes());
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let text_len = reader.u32()? as usize;
        if text_len > MAX_ACL_TEXT_BYTES {
            return Err(ProtocolError::AclTextTooLarge {
                len: text_len,
                max: MAX_ACL_TEXT_BYTES,
            });
        }
        let text = std::str::from_utf8(reader.take(text_len)?).map_err(|_| {
            ProtocolError::InvalidField {
                field: "acl_text",
                reason: "acl text is not valid UTF-8",
            }
        })?;
        reader.finish()?;
        Ok(Self {
            acl: WireAcl::new(text.to_string())?,
        })
    }
}

fn validate_kind(kind: WireEntryKind) -> Result<()> {
    if kind == WireEntryKind::Symlink {
        return Err(ProtocolError::InvalidField {
            field: "acl_kind",
            reason: "access control lists are unsupported for symlinks",
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
    fn read_request_round_trips_without_text() {
        let request = WireAclRequest::read(path(), WireEntryKind::File).unwrap();
        let decoded = WireAclRequest::decode(&request.encode().unwrap()).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.mode(), AclMode::Read);
        assert!(decoded.acl().is_none());
    }

    #[test]
    fn write_request_round_trips_text_including_empty_clear() {
        for text in ["", "allow::user:nick:read\n"] {
            let request = WireAclRequest::write(
                path(),
                WireEntryKind::Directory,
                WireAcl::new(text.to_string()).unwrap(),
            )
            .unwrap();
            let decoded = WireAclRequest::decode(&request.encode().unwrap()).unwrap();
            assert_eq!(decoded, request);
            assert_eq!(decoded.mode(), AclMode::Write);
            assert_eq!(decoded.acl().unwrap().text(), text);
        }
    }

    #[test]
    fn result_round_trips_and_reports_text() {
        let result =
            WireAclResult::new(WireAcl::new("allow::user:nick:read\n".to_string()).unwrap());
        let decoded = WireAclResult::decode(&result.encode().unwrap()).unwrap();
        assert_eq!(decoded.acl().text(), "allow::user:nick:read\n");
    }

    #[test]
    fn symlink_kind_is_rejected_before_encoding() {
        assert!(WireAclRequest::read(path(), WireEntryKind::Symlink).is_err());
    }

    #[test]
    fn oversized_text_fails_loudly() {
        let big = "x".repeat(MAX_ACL_TEXT_BYTES + 1);
        assert!(matches!(
            WireAcl::new(big),
            Err(ProtocolError::AclTextTooLarge { .. })
        ));
    }

    #[test]
    fn decoder_rejects_truncation_unknown_mode_and_trailing_data() {
        let encoded = WireAclRequest::write(
            path(),
            WireEntryKind::File,
            WireAcl::new("allow::user:nick:read\n".to_string()).unwrap(),
        )
        .unwrap()
        .encode()
        .unwrap();
        for len in 0..encoded.len() {
            assert!(WireAclRequest::decode(&encoded[..len]).is_err());
        }
        let mut unknown = encoded.to_vec();
        unknown[1] = u8::MAX;
        assert!(matches!(
            WireAclRequest::decode(&unknown),
            Err(ProtocolError::InvalidField {
                field: "acl_mode",
                ..
            })
        ));
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(WireAclRequest::decode(&trailing).is_err());
    }

    #[test]
    fn decoder_rejects_oversized_text_length_before_allocating_it() {
        let path = path();
        let mut out = BytesMut::new();
        out.put_u8(WireEntryKind::File as u8);
        out.put_u8(AclMode::Write as u8);
        out.put_u32(u32::try_from(path.as_encoded().len()).unwrap());
        out.extend_from_slice(path.as_encoded());
        out.put_u32(u32::try_from(MAX_ACL_TEXT_BYTES + 1).unwrap());
        let payload = out.freeze();
        assert!(matches!(
            WireAclRequest::decode(&payload),
            Err(ProtocolError::AclTextTooLarge { .. })
        ));
    }

    #[test]
    fn decoder_rejects_non_utf8_text() {
        let path = path();
        let mut out = BytesMut::new();
        out.put_u8(WireEntryKind::File as u8);
        out.put_u8(AclMode::Write as u8);
        out.put_u32(u32::try_from(path.as_encoded().len()).unwrap());
        out.extend_from_slice(path.as_encoded());
        out.put_u32(2);
        out.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            WireAclRequest::decode(&out.freeze()),
            Err(ProtocolError::InvalidField {
                field: "acl_text",
                ..
            })
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_acl_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..8192)) {
            let _ = WireAclRequest::decode(&payload);
            let _ = WireAclResult::decode(&payload);
        }
    }
}
