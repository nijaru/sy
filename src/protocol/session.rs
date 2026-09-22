use super::codec::SliceReader;
use super::handshake::{ProtocolVersion, PROTOCOL_V3_1};
use super::{CapabilitySet, ProtocolError, Result, WirePath, MAX_WIRE_PATH_BYTES};
use bytes::{BufMut, Bytes, BytesMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Operation {
    Push = 1,
    Pull = 2,
}

impl TryFrom<u8> for Operation {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Push),
            2 => Ok(Self::Pull),
            _ => Err(ProtocolError::InvalidField {
                field: "operation",
                reason: "unknown operation value",
            }),
        }
    }
}

/// Opens the remote endpoint after protocol/platform negotiation.
///
/// `root` is encoded for the server platform announced in `ServerHello`. Keeping
/// it out of `ClientHello` avoids interpreting target-native bytes before the
/// target platform is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOpen {
    pub operation: Operation,
    pub root: WirePath,
}

impl SessionOpen {
    pub const fn new(operation: Operation, root: WirePath) -> Self {
        Self { operation, root }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let root_len =
            u32::try_from(self.root.as_bytes().len()).map_err(|_| ProtocolError::InvalidField {
                field: "root",
                reason: "root path length exceeds u32",
            })?;
        let mut out = BytesMut::with_capacity(5 + self.root.as_bytes().len());
        out.put_u8(self.operation as u8);
        out.put_u32(root_len);
        out.extend_from_slice(self.root.as_bytes());
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let operation = Operation::try_from(reader.u8()?)?;
        let root_len = reader.u32()? as usize;
        if root_len > MAX_WIRE_PATH_BYTES {
            return Err(ProtocolError::PathTooLong {
                len: root_len,
                max: MAX_WIRE_PATH_BYTES,
            });
        }
        let root = WirePath::new(Bytes::copy_from_slice(reader.take(root_len)?))?;
        reader.finish()?;
        Ok(Self { operation, root })
    }
}

/// Wire form of one name-comparison axis reported for the opened root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NameFolding {
    /// Names alias only when byte-identical.
    Exact = 0,
    /// The axis folds distinct byte names together.
    Folded = 1,
    /// The server could not determine this axis.
    Unspecified = 2,
}

impl NameFolding {
    fn decode(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Exact),
            1 => Ok(Self::Folded),
            2 => Ok(Self::Unspecified),
            _ => Err(ProtocolError::InvalidField {
                field: "name_folding",
                reason: "unknown folding value",
            }),
        }
    }
}

/// Destination name-comparison semantics, probed by the server for the
/// negotiated root. The client maps this into its namespace preflight model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireNamespaceSemantics {
    pub case: NameFolding,
    pub normalization: NameFolding,
}

impl WireNamespaceSemantics {
    fn encode(self) -> u8 {
        (self.case as u8) | ((self.normalization as u8) << 2)
    }

    fn decode(value: u8) -> Result<Self> {
        if value & 0xF0 != 0 {
            return Err(ProtocolError::InvalidField {
                field: "namespace_semantics",
                reason: "reserved bits are set",
            });
        }
        Ok(Self {
            case: NameFolding::decode(value & 0x03)?,
            normalization: NameFolding::decode((value >> 2) & 0x03)?,
        })
    }
}

/// Confirms that the root was opened and reports capabilities for that concrete
/// endpoint/filesystem, which may be narrower than process-wide capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionReady {
    pub capabilities: CapabilitySet,
    /// Timestamp comparison resolution for this endpoint. Zero means unknown.
    pub modtime_precision_ns: u64,
    /// Root name-comparison semantics. Always present for negotiated
    /// protocol >= 3.1; `None` only when decoding a 3.0 peer's message.
    pub namespace_semantics: Option<WireNamespaceSemantics>,
}

impl SessionReady {
    pub const fn new(
        capabilities: CapabilitySet,
        modtime_precision_ns: u64,
        namespace_semantics: WireNamespaceSemantics,
    ) -> Self {
        Self {
            capabilities,
            modtime_precision_ns,
            namespace_semantics: Some(namespace_semantics),
        }
    }

    pub fn encode(self, version: ProtocolVersion) -> Bytes {
        let mut out = BytesMut::with_capacity(17);
        out.put_u64(self.capabilities.bits());
        out.put_u64(self.modtime_precision_ns);
        if version >= PROTOCOL_V3_1 {
            match self.namespace_semantics {
                Some(semantics) => out.put_u8(semantics.encode()),
                // A 3.1 encoder always sends a concrete answer; unknown axes
                // are values, not absent fields.
                None => out.put_u8(
                    WireNamespaceSemantics {
                        case: NameFolding::Unspecified,
                        normalization: NameFolding::Unspecified,
                    }
                    .encode(),
                ),
            }
        }
        out.freeze()
    }

    pub fn decode(payload: &[u8], version: ProtocolVersion) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let capabilities = CapabilitySet::from_bits_retain(reader.u64()?);
        let modtime_precision_ns = reader.u64()?;
        let namespace_semantics = if version >= PROTOCOL_V3_1 {
            Some(WireNamespaceSemantics::decode(reader.u8()?)?)
        } else {
            None
        };
        reader.finish()?;
        Ok(Self {
            capabilities,
            modtime_precision_ns,
            namespace_semantics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_open_round_trip_preserves_target_native_root() {
        let open = SessionOpen::new(
            Operation::Pull,
            WirePath::new(Bytes::from_static(&[b'C', 0, b':', 0, b'\\', 0])).unwrap(),
        );
        let decoded = SessionOpen::decode(&open.encode().unwrap()).unwrap();
        assert_eq!(decoded, open);
    }

    #[test]
    fn session_open_rejects_truncation_and_trailing_data() {
        let open = SessionOpen::new(
            Operation::Push,
            WirePath::new(Bytes::from_static(b"/srv/data")).unwrap(),
        );
        let encoded = open.encode().unwrap();
        for len in 0..encoded.len() {
            assert!(SessionOpen::decode(&encoded[..len]).is_err());
        }
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(SessionOpen::decode(&trailing).is_err());
    }

    #[test]
    fn session_ready_round_trip_retains_future_capability_bits() {
        let ready = SessionReady::new(
            CapabilitySet::from_bits_retain(1_u64 << 63),
            1,
            WireNamespaceSemantics {
                case: NameFolding::Folded,
                normalization: NameFolding::Unspecified,
            },
        );
        let decoded = SessionReady::decode(&ready.encode(PROTOCOL_V3_1), PROTOCOL_V3_1).unwrap();
        assert_eq!(decoded, ready);
    }

    #[test]
    fn session_ready_v3_0_shape_carries_no_semantics() {
        let ready = SessionReady::new(
            CapabilitySet::BLAKE3,
            7,
            WireNamespaceSemantics {
                case: NameFolding::Exact,
                normalization: NameFolding::Exact,
            },
        );
        let legacy = ready.encode(super::super::handshake::PROTOCOL_V3);
        assert_eq!(legacy.len(), 16);
        let decoded = SessionReady::decode(&legacy, super::super::handshake::PROTOCOL_V3).unwrap();
        assert_eq!(decoded.namespace_semantics, None);
        assert_eq!(decoded.capabilities, CapabilitySet::BLAKE3);

        // Shape follows the negotiated version, so mismatched expectations
        // fail loudly instead of silently reading the wrong fields.
        assert!(SessionReady::decode(&legacy, PROTOCOL_V3_1).is_err());
        assert!(SessionReady::decode(
            &ready.encode(PROTOCOL_V3_1),
            super::super::handshake::PROTOCOL_V3
        )
        .is_err());
    }

    #[test]
    fn wire_namespace_semantics_reject_invalid_values() {
        // Reserved high bits.
        assert!(WireNamespaceSemantics::decode(0x10).is_err());
        // Field value 3 is undefined on both axes.
        assert!(WireNamespaceSemantics::decode(0x03).is_err());
        assert!(WireNamespaceSemantics::decode(0x0C).is_err());
        // All defined combinations round trip.
        for case in [
            NameFolding::Exact,
            NameFolding::Folded,
            NameFolding::Unspecified,
        ] {
            for normalization in [
                NameFolding::Exact,
                NameFolding::Folded,
                NameFolding::Unspecified,
            ] {
                let semantics = WireNamespaceSemantics {
                    case,
                    normalization,
                };
                assert_eq!(
                    WireNamespaceSemantics::decode(semantics.encode()).unwrap(),
                    semantics
                );
            }
        }
    }
}
