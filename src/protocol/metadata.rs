use super::codec::SliceReader;
use super::{
    ProtocolError, RelativeWirePath, Result, WireAcl, WireEntryKind, WireXattrResult,
    MAX_ACL_TEXT_BYTES, MAX_FRAME_PAYLOAD, MAX_WIRE_PATH_BYTES,
};
use bitflags::bitflags;
use bytes::{BufMut, Bytes, BytesMut};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MetadataFields: u8 {
        const UNIX_MODE = 1 << 0;
        const MODIFIED = 1 << 1;
        const XATTRS = 1 << 2;
        const ACL = 1 << 3;
        const BSD_FLAGS = 1 << 4;
    }
}

/// Authority for a metadata request. Observed mutations always carry an
/// endpoint-issued identity; there is no unobserved mutation authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireMetadataTarget {
    Observed {
        kind: WireEntryKind,
        identity: [u8; 32],
    },
}

impl WireMetadataTarget {
    fn kind(self) -> WireEntryKind {
        match self {
            Self::Observed { kind, .. } => kind,
        }
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
    target: WireMetadataTarget,
    unix_mode: Option<u32>,
    modified: Option<(i64, u32)>,
    xattrs: Option<WireXattrResult>,
    acl: Option<WireAcl>,
    bsd_flags: Option<u32>,
}

impl WireMetadata {
    pub fn new(
        path: RelativeWirePath,
        target: WireMetadataTarget,
        unix_mode: Option<u32>,
        modified: Option<(i64, u32)>,
        xattrs: Option<WireXattrResult>,
        acl: Option<WireAcl>,
        bsd_flags: Option<u32>,
    ) -> Result<Self> {
        let metadata = Self {
            path,
            target,
            unix_mode,
            modified,
            xattrs,
            acl,
            bsd_flags,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    pub const fn target(&self) -> WireMetadataTarget {
        self.target
    }

    pub const fn unix_mode(&self) -> Option<u32> {
        self.unix_mode
    }

    pub const fn modified(&self) -> Option<(i64, u32)> {
        self.modified
    }

    pub fn xattrs(&self) -> Option<&WireXattrResult> {
        self.xattrs.as_ref()
    }
    pub fn acl(&self) -> Option<&WireAcl> {
        self.acl.as_ref()
    }
    pub const fn bsd_flags(&self) -> Option<u32> {
        self.bsd_flags
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let xattrs = self
            .xattrs
            .as_ref()
            .map(WireXattrResult::encode)
            .transpose()?;
        let acl = self.acl.as_ref().map(WireAcl::encode);
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
            .and_then(|value| {
                value.checked_add(match self.target {
                    WireMetadataTarget::Observed { .. } => 32,
                })
            })
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

        for bytes in [xattrs.as_ref(), acl.as_ref()].into_iter().flatten() {
            capacity = capacity
                .checked_add(4)
                .and_then(|n| n.checked_add(bytes.len()))
                .ok_or(ProtocolError::InvalidMessage(
                    "metadata payload length overflow",
                ))?;
        }
        if self.bsd_flags.is_some() {
            capacity += 4;
        }
        require_payload_bound(capacity)?;
        let mut out = BytesMut::with_capacity(capacity);
        out.put_u8(match self.target {
            WireMetadataTarget::Observed { kind, .. } => kind as u8,
        });
        out.put_u8(fields.bits());
        out.put_u32(path_len);
        out.extend_from_slice(self.path.as_encoded());
        let WireMetadataTarget::Observed { identity, .. } = self.target;
        {
            out.extend_from_slice(&identity);
        }
        if let Some(mode) = self.unix_mode {
            out.put_u32(mode);
        }
        if let Some((seconds, nanoseconds)) = self.modified {
            out.put_i64(seconds);
            out.put_u32(nanoseconds);
        }
        for bytes in [xattrs, acl].into_iter().flatten() {
            out.put_u32(
                u32::try_from(bytes.len())
                    .map_err(|_| ProtocolError::InvalidMessage("metadata field length"))?,
            );
            out.extend_from_slice(&bytes);
        }
        if let Some(flags) = self.bsd_flags {
            out.put_u32(flags);
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        require_payload_bound(payload.len())?;
        let mut reader = SliceReader::new(payload);
        let target_tag = reader.u8()?;
        let kind = WireEntryKind::try_from(target_tag)?;
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
        let target = WireMetadataTarget::Observed {
            kind,
            identity: reader
                .take(32)?
                .try_into()
                .map_err(|_| ProtocolError::InvalidMessage("invalid metadata identity length"))?,
        };
        let unix_mode = fields
            .contains(MetadataFields::UNIX_MODE)
            .then(|| reader.u32())
            .transpose()?;
        let modified = if fields.contains(MetadataFields::MODIFIED) {
            Some((reader.i64()?, reader.u32()?))
        } else {
            None
        };
        let xattrs = if fields.contains(MetadataFields::XATTRS) {
            let len = reader.u32()? as usize;
            Some(WireXattrResult::decode(reader.take(len)?)?)
        } else {
            None
        };
        let acl = if fields.contains(MetadataFields::ACL) {
            let len = reader.u32()? as usize;
            if len > MAX_ACL_TEXT_BYTES + 4 {
                return Err(ProtocolError::AclTextTooLarge {
                    len: len - 4,
                    max: MAX_ACL_TEXT_BYTES,
                });
            }
            Some(WireAcl::decode(reader.take(len)?)?)
        } else {
            None
        };
        let bsd_flags = fields
            .contains(MetadataFields::BSD_FLAGS)
            .then(|| reader.u32())
            .transpose()?;
        reader.finish()?;
        Self::new(path, target, unix_mode, modified, xattrs, acl, bsd_flags)
    }

    fn fields(&self) -> MetadataFields {
        let mut fields = MetadataFields::empty();
        fields.set(MetadataFields::UNIX_MODE, self.unix_mode.is_some());
        fields.set(MetadataFields::MODIFIED, self.modified.is_some());
        fields.set(MetadataFields::XATTRS, self.xattrs.is_some());
        fields.set(MetadataFields::ACL, self.acl.is_some());
        fields.set(MetadataFields::BSD_FLAGS, self.bsd_flags.is_some());
        fields
    }

    fn validate(&self) -> Result<()> {
        if self.fields().is_empty() {
            return Err(ProtocolError::InvalidField {
                field: "metadata_fields",
                reason: "metadata request must contain at least one field",
            });
        }
        if self.target.kind() == WireEntryKind::Symlink && self.fields() != MetadataFields::MODIFIED
        {
            return Err(ProtocolError::InvalidField {
                field: "metadata_fields",
                reason: "symlinks support only no-follow modification time",
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

fn require_payload_bound(len: usize) -> Result<()> {
    if len > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge {
            len,
            max: MAX_FRAME_PAYLOAD,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn observed(kind: WireEntryKind) -> WireMetadataTarget {
        WireMetadataTarget::Observed {
            kind,
            identity: [42; 32],
        }
    }

    fn path() -> RelativeWirePath {
        RelativeWirePath::from_components([b"dir".as_slice(), b"entry".as_slice()]).unwrap()
    }

    #[test]
    fn metadata_round_trip_preserves_requested_fields() {
        let metadata = WireMetadata::new(
            path(),
            observed(WireEntryKind::File),
            Some(0o100640),
            Some((-1, 999_999_999)),
            Some(
                WireXattrResult::new(vec![super::super::WireXattr::new(
                    bytes::Bytes::from_static(b"user.test"),
                    bytes::Bytes::from_static(b"value"),
                )
                .unwrap()])
                .unwrap(),
            ),
            Some(WireAcl::new("allow::user:42:read".to_owned()).unwrap()),
            Some(1),
        )
        .unwrap();
        assert_eq!(
            WireMetadata::decode(&metadata.encode().unwrap()).unwrap(),
            metadata
        );
    }

    #[test]
    fn metadata_rejects_empty_symlink_mode_and_invalid_time() {
        assert!(WireMetadata::new(
            path(),
            observed(WireEntryKind::File),
            None,
            None,
            None,
            None,
            None
        )
        .is_err());
        assert!(WireMetadata::new(
            path(),
            observed(WireEntryKind::Symlink),
            Some(0o777),
            None,
            None,
            None,
            None
        )
        .is_err());
        assert!(WireMetadata::new(
            path(),
            observed(WireEntryKind::Directory),
            None,
            Some((0, 1_000_000_000)),
            None,
            None,
            None,
        )
        .is_err());
    }

    #[test]
    fn decoder_rejects_truncation_and_trailing_data() {
        let encoded = WireMetadata::new(
            path(),
            observed(WireEntryKind::Directory),
            Some(0o755),
            Some((1, 2)),
            Some(WireXattrResult::new(vec![]).unwrap()),
            Some(WireAcl::new(String::new()).unwrap()),
            Some(0),
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

    #[test]
    fn malformed_preservation_tail_cannot_accept_a_valid_mode_prefix_or_allocate_unbounded_fields()
    {
        let prefix = WireMetadata::new(
            path(),
            observed(WireEntryKind::File),
            Some(0o600),
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .encode()
        .unwrap();
        let mut attrs = prefix.to_vec();
        attrs[1] |= MetadataFields::XATTRS.bits();
        attrs.extend_from_slice(&2_u32.to_be_bytes());
        attrs.extend_from_slice(&((crate::protocol::MAX_XATTR_ENTRIES + 1) as u16).to_be_bytes());
        assert!(matches!(
            WireMetadata::decode(&attrs),
            Err(ProtocolError::TooManyXattrs { .. })
        ));
        let mut acl = prefix.to_vec();
        acl[1] |= MetadataFields::ACL.bits();
        acl.extend_from_slice(&((MAX_ACL_TEXT_BYTES + 5) as u32).to_be_bytes());
        assert!(matches!(
            WireMetadata::decode(&acl),
            Err(ProtocolError::AclTextTooLarge { .. })
        ));
        assert!(matches!(
            WireMetadata::decode(&vec![0; MAX_FRAME_PAYLOAD + 1]),
            Err(ProtocolError::PayloadTooLarge { .. })
        ));
    }

    proptest! {
        #[test]
        fn observed_metadata_round_trips_dynamic_demands_without_losing_presence_or_identity(
            identity in any::<[u8; 32]>(),
            mode in prop::option::of(any::<u32>()),
            modified in prop::option::of((any::<i64>(), 0_u32..1_000_000_000)),
            value in prop::option::of(prop::collection::vec(any::<u8>(), 0..256)),
            acl in prop::option::of("[a-z: \\n]{0,128}"),
            flags in prop::option::of(any::<u32>()),
        ) {
            prop_assume!(mode.is_some() || modified.is_some() || value.is_some() || acl.is_some() || flags.is_some());
            let xattrs = value.map(|value| WireXattrResult::new(vec![super::super::WireXattr::new(Bytes::from_static(b"user.demand"), value).unwrap()]).unwrap());
            let metadata = WireMetadata::new(
                path(), WireMetadataTarget::Observed { kind: WireEntryKind::File, identity },
                mode, modified, xattrs, acl.map(|acl| WireAcl::new(acl).unwrap()), flags,
            ).unwrap();
            prop_assert_eq!(WireMetadata::decode(&metadata.encode().unwrap()).unwrap(), metadata);
        }

        #[test]
        fn arbitrary_metadata_payloads_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireMetadata::decode(&payload);
        }
    }
}
