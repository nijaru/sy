//! Bounded observation-bound directory preservation on Metadata streams.
use super::codec::SliceReader;
use super::{
    ProtocolError, RelativeWirePath, Result, WireAcl, WireXattrResult, MAX_ACL_TEXT_BYTES,
    MAX_WIRE_PATH_BYTES,
};
use bytes::{BufMut, Bytes, BytesMut};

pub const DIRECTORY_READ: u8 = 4;
pub const DIRECTORY_FINALIZE: u8 = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireDirectoryPreservation {
    pub xattrs: Option<WireXattrResult>,
    pub acl: Option<WireAcl>,
    pub flags: Option<u32>,
}

impl WireDirectoryPreservation {
    pub fn encode(&self) -> Result<Bytes> {
        let mut out = BytesMut::new();
        out.put_u8(
            u8::from(self.xattrs.is_some())
                | (u8::from(self.acl.is_some()) << 1)
                | (u8::from(self.flags.is_some()) << 2),
        );
        if let Some(xattrs) = &self.xattrs {
            let bytes = xattrs.encode()?;
            out.put_u32(
                u32::try_from(bytes.len())
                    .map_err(|_| ProtocolError::InvalidMessage("xattr length"))?,
            );
            out.extend_from_slice(&bytes);
        }
        if let Some(acl) = &self.acl {
            let bytes = acl.encode();
            out.put_u32(
                u32::try_from(bytes.len())
                    .map_err(|_| ProtocolError::InvalidMessage("ACL length"))?,
            );
            out.extend_from_slice(&bytes);
        }
        if let Some(flags) = self.flags {
            out.put_u32(flags);
        }
        Ok(out.freeze())
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = SliceReader::new(payload);
        let fields = r.u8()?;
        if fields & !7 != 0 {
            return Err(ProtocolError::InvalidMessage(
                "directory preservation fields",
            ));
        }
        let xattrs = if fields & 1 != 0 {
            let len = r.u32()? as usize;
            Some(WireXattrResult::decode(r.take(len)?)?)
        } else {
            None
        };
        let acl = if fields & 2 != 0 {
            let len = r.u32()? as usize;
            if len > MAX_ACL_TEXT_BYTES + 4 {
                return Err(ProtocolError::AclTextTooLarge {
                    len: len - 4,
                    max: MAX_ACL_TEXT_BYTES,
                });
            }
            Some(WireAcl::decode(r.take(len)?)?)
        } else {
            None
        };
        let flags = if fields & 4 != 0 {
            Some(r.u32()?)
        } else {
            None
        };
        r.finish()?;
        Ok(Self { xattrs, acl, flags })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireDirectoryAction {
    Read {
        xattrs: bool,
        acl: bool,
        flags: bool,
    },
    Finalize {
        mode: Option<u32>,
        modified: Option<(i64, u32)>,
        preservation: WireDirectoryPreservation,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireDirectoryMetadata {
    pub path: RelativeWirePath,
    pub identity: [u8; 32],
    pub action: WireDirectoryAction,
}

impl WireDirectoryMetadata {
    pub fn encode(&self) -> Result<Bytes> {
        let mut out = BytesMut::new();
        out.put_u8(match self.action {
            WireDirectoryAction::Read { .. } => DIRECTORY_READ,
            WireDirectoryAction::Finalize { .. } => DIRECTORY_FINALIZE,
        });
        out.put_u32(
            u32::try_from(self.path.as_encoded().len())
                .map_err(|_| ProtocolError::InvalidMessage("directory path length"))?,
        );
        out.extend_from_slice(self.path.as_encoded());
        out.extend_from_slice(&self.identity);
        match &self.action {
            WireDirectoryAction::Read { xattrs, acl, flags } => {
                out.put_u8(u8::from(*xattrs) | (u8::from(*acl) << 1) | (u8::from(*flags) << 2))
            }
            WireDirectoryAction::Finalize {
                mode,
                modified,
                preservation,
            } => {
                out.put_u8(u8::from(mode.is_some()) | (u8::from(modified.is_some()) << 1));
                if let Some(mode) = mode {
                    out.put_u32(*mode);
                }
                if let Some((seconds, nanos)) = modified {
                    if *nanos >= 1_000_000_000 {
                        return Err(ProtocolError::InvalidMessage("directory timestamp"));
                    }
                    out.put_i64(*seconds);
                    out.put_u32(*nanos);
                }
                out.extend_from_slice(&preservation.encode()?);
            }
        }
        Ok(out.freeze())
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = SliceReader::new(payload);
        let tag = r.u8()?;
        let len = r.u32()? as usize;
        if len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(r.take(len)?))?;
        let identity = r
            .take(32)?
            .try_into()
            .map_err(|_| ProtocolError::InvalidMessage("directory identity"))?;
        let fields = r.u8()?;
        let action = match tag {
            DIRECTORY_READ if fields & !7 == 0 => WireDirectoryAction::Read {
                xattrs: fields & 1 != 0,
                acl: fields & 2 != 0,
                flags: fields & 4 != 0,
            },
            DIRECTORY_FINALIZE if fields & !3 == 0 => {
                let mode = if fields & 1 != 0 {
                    Some(r.u32()?)
                } else {
                    None
                };
                let modified = if fields & 2 != 0 {
                    let seconds = r.i64()?;
                    let nanos = r.u32()?;
                    if nanos >= 1_000_000_000 {
                        return Err(ProtocolError::InvalidMessage("directory timestamp"));
                    }
                    Some((seconds, nanos))
                } else {
                    None
                };
                let preservation = WireDirectoryPreservation::decode(r.take_remaining()?)?;
                WireDirectoryAction::Finalize {
                    mode,
                    modified,
                    preservation,
                }
            }
            _ => return Err(ProtocolError::InvalidMessage("directory action or fields")),
        };
        r.finish()?;
        Ok(Self {
            path,
            identity,
            action,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn unobserved_directory_mutations_are_rejected() {
        use crate::protocol::{
            WireAclRequest, WireBsdFlagsRequest, WireEntryKind, WireMetadata, WireMetadataTarget,
            WireXattrRequest,
        };
        let path = RelativeWirePath::from_components([b"dir".as_slice()]).unwrap();
        assert!(WireXattrRequest::write(path.clone(), WireEntryKind::Directory, vec![]).is_err());
        assert!(WireAclRequest::write(
            path.clone(),
            WireEntryKind::Directory,
            WireAcl::new(String::new()).unwrap()
        )
        .is_err());
        assert!(WireBsdFlagsRequest::write(path.clone(), WireEntryKind::Directory, 0).is_err());
        let mut former = WireMetadata::new(
            path,
            WireMetadataTarget::Observed {
                kind: WireEntryKind::Directory,
                identity: [0; 32],
            },
            Some(0o755),
            None,
        )
        .unwrap()
        .encode()
        .unwrap()
        .to_vec();
        former[0] = 0;
        assert!(WireMetadata::decode(&former).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_directory_messages_never_panic(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = WireDirectoryMetadata::decode(&payload);
            let _ = WireDirectoryPreservation::decode(&payload);
        }
    }

    #[test]
    fn directory_authority_and_bounds() {
        for action in [
            WireDirectoryAction::Read {
                xattrs: true,
                acl: true,
                flags: true,
            },
            WireDirectoryAction::Finalize {
                mode: Some(0o750),
                modified: Some((1, 2)),
                preservation: WireDirectoryPreservation {
                    xattrs: Some(WireXattrResult::new(vec![]).unwrap()),
                    acl: Some(WireAcl::new(String::new()).unwrap()),
                    flags: Some(0),
                },
            },
        ] {
            let value = WireDirectoryMetadata {
                path: RelativeWirePath::from_components([b"dir".as_slice()]).unwrap(),
                identity: [42; 32],
                action,
            };
            let bytes = value.encode().unwrap();
            assert_eq!(WireDirectoryMetadata::decode(&bytes).unwrap(), value);
            for len in 0..bytes.len() {
                assert!(WireDirectoryMetadata::decode(&bytes[..len]).is_err());
            }
            let mut extra = bytes.to_vec();
            extra.push(0);
            assert!(WireDirectoryMetadata::decode(&extra).is_err());
        }
    }
}
