//! Bounded declarations and commands for one retained hardlink-group stream.
use super::codec::SliceReader;
use super::{
    ProtocolError, RelativeWirePath, Result, WireEntryKind, WireFetchCompression, WireFileBegin,
    MAX_WIRE_PATH_BYTES,
};
use bytes::{BufMut, Bytes, BytesMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WireGroupRole {
    Destination = 0,
    Source = 1,
}

/// `file` carries the first physical address, byte size, optional delta basis,
/// and the group's single intended mode/time/flags. Source leases use its basis
/// as the original scanned source observation, not as a reconstruction basis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireGroupOpen {
    pub role: WireGroupRole,
    pub source_bytes: bool,
    pub retain_metadata: bool,
    pub xattrs: bool,
    pub acls: bool,
    pub source_flags: bool,
    /// Request destination-byte parity for retained/no-op removal candidates.
    /// Source data streams always carry their own mandatory BLAKE3 footer.
    pub verify_bytes: bool,
    pub compression: WireFetchCompression,
    pub file: WireFileBegin,
}

impl WireGroupOpen {
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.put_u8(self.role as u8);
        out.put_u8(
            u8::from(self.source_bytes)
                | (u8::from(self.retain_metadata) << 1)
                | (u8::from(self.xattrs) << 2)
                | (u8::from(self.acls) << 3)
                | (u8::from(self.source_flags) << 4)
                | (u8::from(self.verify_bytes) << 5),
        );
        out.put_u8(match self.compression {
            WireFetchCompression::None => 0,
            WireFetchCompression::Auto => 1,
            WireFetchCompression::Always => 2,
        });
        out.extend_from_slice(&self.file.encode());
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let role = match reader.u8()? {
            0 => WireGroupRole::Destination,
            1 => WireGroupRole::Source,
            _ => return Err(invalid("unknown group role")),
        };
        let fields = reader.u8()?;
        if fields & !63 != 0 {
            return Err(invalid("unknown group fields"));
        }
        if role == WireGroupRole::Source && fields & 32 != 0 {
            return Err(invalid(
                "destination-byte verification requires a destination group",
            ));
        }
        let compression = match reader.u8()? {
            0 => WireFetchCompression::None,
            1 => WireFetchCompression::Auto,
            2 => WireFetchCompression::Always,
            _ => return Err(invalid("unknown group compression")),
        };
        let file = WireFileBegin::decode(reader.take_remaining()?)?;
        if role == WireGroupRole::Source && file.basis().is_none() {
            return Err(invalid("source lease requires an original observation"));
        }
        Ok(Self {
            role,
            source_bytes: fields & 1 != 0,
            retain_metadata: fields & 2 != 0,
            xattrs: fields & 4 != 0,
            acls: fields & 8 != 0,
            source_flags: fields & 16 != 0,
            verify_bytes: fields & 32 != 0,
            compression,
            file,
        })
    }
}

/// One physical member per frame. An absent expectation is valid only for a
/// destination; source declarations always carry a regular-file observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireGroupMember {
    pub ordinal: u64,
    pub path: RelativeWirePath,
    pub observation: Option<(WireEntryKind, [u8; 32])>,
}

impl WireGroupMember {
    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.put_u64(self.ordinal);
        match self.observation {
            None => out.put_u8(0),
            Some((kind, identity)) => {
                out.put_u8(kind as u8);
                out.extend_from_slice(&identity);
            }
        }
        out.extend_from_slice(self.path.as_encoded());
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let ordinal = reader.u64()?;
        let kind = reader.u8()?;
        let observation = if kind == 0 {
            None
        } else {
            Some((WireEntryKind::try_from(kind)?, reader.array::<32>()?))
        };
        let remaining = reader.take_remaining()?;
        if remaining.len() > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: remaining.len(),
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let path = RelativeWirePath::decode(Bytes::copy_from_slice(remaining))?;
        Ok(Self {
            ordinal,
            path,
            observation,
        })
    }
}

/// Exact ordered declaration transcript. This is a live-stream commitment,
/// separate from the controller's original sealed plan catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireGroupSeal {
    pub count: u64,
    pub digest: [u8; 32],
}

impl WireGroupSeal {
    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(40);
        out.put_u64(self.count);
        out.extend_from_slice(&self.digest);
        out.freeze()
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let count = reader.u64()?;
        if count == 0 {
            return Err(invalid("empty group seal"));
        }
        let digest = reader.array::<32>()?;
        reader.finish()?;
        Ok(Self { count, digest })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireGroupStep {
    Prepare,
    Validate(u64),
    Publish(u64),
    Finish(WireGroupSeal),
    /// Quiescent control phases acknowledge cancellation. During population,
    /// cancellation is terminal: the session drains owned work without an
    /// Aborted receipt or any promise to roll back published members.
    Abort,
}

impl WireGroupStep {
    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::new();
        match self {
            Self::Prepare => out.put_u8(0),
            Self::Validate(ordinal) => {
                out.put_u8(1);
                out.put_u64(ordinal);
            }
            Self::Publish(ordinal) => {
                out.put_u8(2);
                out.put_u64(ordinal);
            }
            Self::Finish(seal) => {
                out.put_u8(3);
                out.extend_from_slice(&seal.encode());
            }
            Self::Abort => out.put_u8(4),
        }
        out.freeze()
    }
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let command = match reader.u8()? {
            0 => Self::Prepare,
            1 => Self::Validate(reader.u64()?),
            2 => Self::Publish(reader.u64()?),
            3 => Self::Finish(WireGroupSeal::decode(reader.take_remaining()?)?),
            4 => Self::Abort,
            _ => return Err(invalid("unknown group command")),
        };
        reader.finish()?;
        Ok(command)
    }
}

/// Replies distinguish observational member progress from final all-member
/// certification. An early publication identity must never be a removal proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireGroupResult {
    Declarations(WireGroupSeal),
    SourceReady {
        flags: Option<u32>,
    },
    DestinationReady {
        publish: bool,
        size: u64,
        digest: Option<[u8; 32]>,
        literal_bytes: u64,
        reused_bytes: u64,
    },
    Validated(u64),
    Published(u64),
    SourceComplete(WireGroupSeal),
    DestinationComplete {
        seal: WireGroupSeal,
        identity: [u8; 32],
    },
    Aborted,
}

impl WireGroupResult {
    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::new();
        match self {
            Self::Declarations(seal) => {
                out.put_u8(0);
                out.extend_from_slice(&seal.encode());
            }
            Self::SourceReady { flags } => {
                out.put_u8(1);
                out.put_u8(u8::from(flags.is_some()));
                if let Some(flags) = flags {
                    out.put_u32(flags);
                }
            }
            Self::DestinationReady {
                publish,
                size,
                digest,
                literal_bytes,
                reused_bytes,
            } => {
                out.put_u8(2);
                out.put_u8(u8::from(publish));
                out.put_u64(size);
                out.put_u8(u8::from(digest.is_some()));
                if let Some(digest) = digest {
                    out.extend_from_slice(&digest);
                }
                out.put_u64(literal_bytes);
                out.put_u64(reused_bytes);
            }
            Self::Validated(ordinal) => {
                out.put_u8(3);
                out.put_u64(ordinal);
            }
            Self::Published(ordinal) => {
                out.put_u8(4);
                out.put_u64(ordinal);
            }
            Self::SourceComplete(seal) => {
                out.put_u8(5);
                out.extend_from_slice(&seal.encode());
            }
            Self::DestinationComplete { seal, identity } => {
                out.put_u8(6);
                out.extend_from_slice(&seal.encode());
                out.extend_from_slice(&identity);
            }
            Self::Aborted => out.put_u8(7),
        }
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let reply = match reader.u8()? {
            0 => Self::Declarations(read_seal(&mut reader)?),
            1 => Self::SourceReady {
                flags: read_bool(&mut reader)?.then(|| reader.u32()).transpose()?,
            },
            2 => {
                let publish = read_bool(&mut reader)?;
                let size = reader.u64()?;
                let digest = read_bool(&mut reader)?
                    .then(|| reader.array::<32>())
                    .transpose()?;
                let literal_bytes = reader.u64()?;
                let reused_bytes = reader.u64()?;
                Self::DestinationReady {
                    publish,
                    size,
                    digest,
                    literal_bytes,
                    reused_bytes,
                }
            }
            3 => Self::Validated(reader.u64()?),
            4 => Self::Published(reader.u64()?),
            5 => Self::SourceComplete(read_seal(&mut reader)?),
            6 => Self::DestinationComplete {
                seal: read_seal(&mut reader)?,
                identity: reader.array::<32>()?,
            },
            7 => Self::Aborted,
            _ => return Err(invalid("unknown group reply")),
        };
        reader.finish()?;
        Ok(reply)
    }
}

fn read_bool(reader: &mut SliceReader<'_>) -> Result<bool> {
    match reader.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("invalid group boolean")),
    }
}

fn read_seal(reader: &mut SliceReader<'_>) -> Result<WireGroupSeal> {
    let count = reader.u64()?;
    if count == 0 {
        return Err(invalid("empty group seal"));
    }
    Ok(WireGroupSeal {
        count,
        digest: reader.array::<32>()?,
    })
}

fn invalid(reason: &'static str) -> ProtocolError {
    ProtocolError::InvalidField {
        field: "hardlink_group",
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn open_binds_role_compression_and_original_observation() {
        let path = RelativeWirePath::from_components([b"file".as_slice()]).unwrap();
        for role in [WireGroupRole::Source, WireGroupRole::Destination] {
            for compression in [
                WireFetchCompression::None,
                WireFetchCompression::Auto,
                WireFetchCompression::Always,
            ] {
                let open = WireGroupOpen {
                    role,
                    source_bytes: true,
                    retain_metadata: false,
                    xattrs: true,
                    acls: true,
                    source_flags: true,
                    verify_bytes: role == WireGroupRole::Destination,
                    compression,
                    file: WireFileBegin::delta(
                        path.clone(),
                        9,
                        crate::protocol::WireFileBasis::new(8, [7; 32]),
                    )
                    .with_metadata(Some(0o640), Some((-1, 999_999_999)))
                    .unwrap()
                    .with_bsd_flags(Some(0)),
                };
                let encoded = open.encode();
                assert_eq!(WireGroupOpen::decode(&encoded).unwrap(), open);
                for end in 0..encoded.len() {
                    assert!(WireGroupOpen::decode(&encoded[..end]).is_err());
                }
                if role == WireGroupRole::Source {
                    let mut invalid = encoded.to_vec();
                    invalid[1] |= 32;
                    assert!(WireGroupOpen::decode(&invalid).is_err());
                }
                for (index, value) in [(0, 2), (1, 0x80), (2, 3)] {
                    let mut invalid = encoded.to_vec();
                    invalid[index] = value;
                    assert!(WireGroupOpen::decode(&invalid).is_err());
                }
            }
        }
        let missing_observation = WireGroupOpen {
            role: WireGroupRole::Source,
            source_bytes: false,
            retain_metadata: false,
            xattrs: false,
            acls: false,
            source_flags: false,
            verify_bytes: false,
            compression: WireFetchCompression::None,
            file: WireFileBegin::whole(path, 9),
        };
        assert!(WireGroupOpen::decode(&missing_observation.encode()).is_err());
    }

    #[test]
    fn member_and_commands_require_complete_exact_payloads() {
        let path = RelativeWirePath::from_components([b"member".as_slice()]).unwrap();
        for observation in [
            None,
            Some((WireEntryKind::File, [7; 32])),
            Some((WireEntryKind::Directory, [8; 32])),
        ] {
            let member = WireGroupMember {
                ordinal: 12,
                path: path.clone(),
                observation,
            };
            let encoded = member.encode();
            assert_eq!(WireGroupMember::decode(&encoded).unwrap(), member);
            for end in 0..encoded.len() {
                assert!(WireGroupMember::decode(&encoded[..end]).is_err());
            }
        }
        for command in [
            WireGroupStep::Prepare,
            WireGroupStep::Validate(2),
            WireGroupStep::Publish(3),
            WireGroupStep::Finish(WireGroupSeal {
                count: 4,
                digest: [9; 32],
            }),
            WireGroupStep::Abort,
        ] {
            let encoded = command.encode();
            assert_eq!(WireGroupStep::decode(&encoded).unwrap(), command);
            for end in 0..encoded.len() {
                assert!(WireGroupStep::decode(&encoded[..end]).is_err());
            }
            let mut trailing = encoded.to_vec();
            trailing.push(0);
            assert!(WireGroupStep::decode(&trailing).is_err());
        }
    }

    #[test]
    fn replies_distinguish_progress_from_final_certification() {
        let seal = WireGroupSeal {
            count: 2,
            digest: [4; 32],
        };
        for reply in [
            WireGroupResult::Declarations(seal),
            WireGroupResult::SourceReady { flags: None },
            WireGroupResult::SourceReady { flags: Some(7) },
            WireGroupResult::DestinationReady {
                publish: false,
                size: 9,
                digest: None,
                literal_bytes: 0,
                reused_bytes: 0,
            },
            WireGroupResult::DestinationReady {
                publish: true,
                size: 9,
                digest: Some([3; 32]),
                literal_bytes: 5,
                reused_bytes: 4,
            },
            WireGroupResult::Validated(0),
            WireGroupResult::Published(1),
            WireGroupResult::SourceComplete(seal),
            WireGroupResult::DestinationComplete {
                seal,
                identity: [8; 32],
            },
            WireGroupResult::Aborted,
        ] {
            let encoded = reply.encode();
            assert_eq!(WireGroupResult::decode(&encoded).unwrap(), reply);
            for end in 0..encoded.len() {
                assert!(WireGroupResult::decode(&encoded[..end]).is_err());
            }
            let mut trailing = encoded.to_vec();
            trailing.push(0);
            assert!(WireGroupResult::decode(&trailing).is_err());
        }
    }

    proptest! {
        #[test]
        fn untrusted_group_messages_never_panic(payload in prop::collection::vec(any::<u8>(), 0..8192)) {
            let _ = WireGroupOpen::decode(&payload);
            let _ = WireGroupMember::decode(&payload);
            let _ = WireGroupSeal::decode(&payload);
            let _ = WireGroupStep::decode(&payload);
            let _ = WireGroupResult::decode(&payload);
        }
    }
}
