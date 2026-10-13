//! Parent-scoped destination namespace qualification in exact protocol 3.13.
use super::codec::SliceReader;
use super::{ProtocolError, RelativeWirePath, Result, WireNamespaceSemantics, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

/// Root is explicit: the relative path codec deliberately excludes empty paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireNamespaceParent {
    Root,
    Directory(RelativeWirePath),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireNamespaceRequest {
    Begin,
    Qualify(WireNamespaceParent),
    /// Destination-only parent observation for an explicit root-confined backup.
    QualifyBackup(WireNamespaceParent),
    ValidateBackupAddress(RelativeWirePath),
    Seal,
}

impl WireNamespaceRequest {
    pub fn encode(&self) -> Result<Bytes> {
        let mut out = BytesMut::new();
        match self {
            Self::Begin => out.put_u8(0),
            Self::Qualify(parent) | Self::QualifyBackup(parent) => {
                out.put_u8(if matches!(self, Self::QualifyBackup(_)) {
                    3
                } else {
                    1
                });
                match parent {
                    WireNamespaceParent::Root => out.put_u8(0),
                    WireNamespaceParent::Directory(path) => {
                        out.put_u8(1);
                        let bytes = path.as_encoded();
                        out.put_u32(u32::try_from(bytes.len()).map_err(|_| {
                            ProtocolError::InvalidMessage("namespace path length exceeds u32")
                        })?);
                        out.extend_from_slice(bytes);
                    }
                }
            }
            Self::ValidateBackupAddress(path) => {
                out.put_u8(4);
                let bytes = path.as_encoded();
                out.put_u32(u32::try_from(bytes.len()).map_err(|_| {
                    ProtocolError::InvalidMessage("namespace path length exceeds u32")
                })?);
                out.extend_from_slice(bytes);
            }
            Self::Seal => out.put_u8(2),
        }
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let tag = reader.u8()?;
        let value = match tag {
            0 => Self::Begin,
            1 | 3 => {
                let parent = match reader.u8()? {
                    0 => WireNamespaceParent::Root,
                    1 => {
                        let len = reader.u32()? as usize;
                        if len > MAX_WIRE_PATH_BYTES {
                            return Err(ProtocolError::PathTooLong {
                                len,
                                max: MAX_WIRE_PATH_BYTES,
                            });
                        }
                        WireNamespaceParent::Directory(RelativeWirePath::decode(
                            Bytes::copy_from_slice(reader.take(len)?),
                        )?)
                    }
                    _ => {
                        return Err(ProtocolError::InvalidMessage(
                            "invalid namespace parent tag",
                        ))
                    }
                };
                if tag == 3 {
                    Self::QualifyBackup(parent)
                } else {
                    Self::Qualify(parent)
                }
            }
            4 => {
                let len = reader.u32()? as usize;
                if len > MAX_WIRE_PATH_BYTES {
                    return Err(ProtocolError::PathTooLong {
                        len,
                        max: MAX_WIRE_PATH_BYTES,
                    });
                }
                Self::ValidateBackupAddress(RelativeWirePath::decode(Bytes::copy_from_slice(
                    reader.take(len)?,
                ))?)
            }
            2 => Self::Seal,
            _ => {
                return Err(ProtocolError::InvalidMessage(
                    "invalid namespace request tag",
                ))
            }
        };
        reader.finish()?;
        Ok(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireNamespaceResult {
    Begun,
    Qualified(WireNamespaceSemantics),
    Sealed,
    BackupAddressValidated,
}

impl WireNamespaceResult {
    pub fn encode(self) -> Bytes {
        match self {
            Self::Begun => Bytes::from_static(&[0]),
            Self::Qualified(profile) => Bytes::copy_from_slice(&[1, profile.encode()]),
            Self::Sealed => Bytes::from_static(&[2]),
            Self::BackupAddressValidated => Bytes::from_static(&[3]),
        }
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let value = match reader.u8()? {
            0 => Self::Begun,
            1 => Self::Qualified(WireNamespaceSemantics::decode(reader.u8()?)?),
            2 => Self::Sealed,
            3 => Self::BackupAddressValidated,
            _ => {
                return Err(ProtocolError::InvalidMessage(
                    "invalid namespace result tag",
                ))
            }
        };
        reader.finish()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_preserve_native_parent_and_require_complete_canonical_payloads() {
        let parent = RelativeWirePath::from_components([b"a".as_slice(), b"\xff"]).unwrap();
        for request in [
            WireNamespaceRequest::Begin,
            WireNamespaceRequest::Qualify(WireNamespaceParent::Root),
            WireNamespaceRequest::Qualify(WireNamespaceParent::Directory(parent.clone())),
            WireNamespaceRequest::QualifyBackup(WireNamespaceParent::Root),
            WireNamespaceRequest::QualifyBackup(WireNamespaceParent::Directory(parent.clone())),
            WireNamespaceRequest::ValidateBackupAddress(parent),
            WireNamespaceRequest::Seal,
        ] {
            let encoded = request.encode().unwrap();
            assert_eq!(WireNamespaceRequest::decode(&encoded).unwrap(), request);
            for end in 0..encoded.len() {
                assert!(WireNamespaceRequest::decode(&encoded[..end]).is_err());
            }
            let mut trailing = encoded.to_vec();
            trailing.push(0);
            assert!(WireNamespaceRequest::decode(&trailing).is_err());
        }
        for payload in [&[4][..], &[3], &[1, 2], &[1, 1, 0, 0, 0, 2, 0, 0]] {
            assert!(WireNamespaceRequest::decode(payload).is_err());
        }
        let mut oversized = vec![1, 1];
        oversized.extend_from_slice(&(MAX_WIRE_PATH_BYTES as u32 + 1).to_be_bytes());
        assert!(matches!(
            WireNamespaceRequest::decode(&oversized),
            Err(ProtocolError::PathTooLong { .. })
        ));
    }

    #[test]
    fn replies_validate_phase_and_profile_tags() {
        use super::super::NameFolding;
        for reply in [
            WireNamespaceResult::Begun,
            WireNamespaceResult::Qualified(WireNamespaceSemantics {
                case: NameFolding::Exact,
                normalization: NameFolding::Unspecified,
            }),
            WireNamespaceResult::Sealed,
            WireNamespaceResult::BackupAddressValidated,
        ] {
            let encoded = reply.encode();
            assert_eq!(WireNamespaceResult::decode(&encoded).unwrap(), reply);
            for end in 0..encoded.len() {
                assert!(WireNamespaceResult::decode(&encoded[..end]).is_err());
            }
        }
        for payload in [&[4][..], &[1, 0xff], &[0, 0], &[2, 0], &[3, 0]] {
            assert!(WireNamespaceResult::decode(payload).is_err());
        }
    }
}
