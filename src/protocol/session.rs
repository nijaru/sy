use super::codec::SliceReader;
use super::operand::{get_path, put_path};
use super::{CapabilitySet, ProtocolError, Result, WirePath};
use super::{OperandRequest, ResolvedOperand, SourceShape};
use bytes::{BufMut, Bytes, BytesMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Operation {
    Push = 1,
    Pull = 2,
    /// Read-only destination inspection, including an observed absent root.
    PreviewPush = 3,
}

impl TryFrom<u8> for Operation {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Push),
            2 => Ok(Self::Pull),
            3 => Ok(Self::PreviewPush),
            _ => Err(ProtocolError::InvalidField {
                field: "operation",
                reason: "unknown operation value",
            }),
        }
    }
}

/// Opens the remote endpoint after protocol/platform negotiation.
///
/// The operand path is encoded for the server platform announced in
/// `ServerHello`. Its role and source shape determine the actual held root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOpen {
    pub operation: Operation,
    pub path: WirePath,
    pub operand: OperandRequest,
}

impl SessionOpen {
    /// Explicit directory-root API used by low-level endpoint callers.
    pub fn new(operation: Operation, path: WirePath) -> Self {
        let operand = match operation {
            Operation::Pull => OperandRequest::Source { contents: true },
            Operation::Push | Operation::PreviewPush => OperandRequest::Destination {
                source: SourceShape::Directory { basename: None },
            },
        };
        Self {
            operation,
            path,
            operand,
        }
    }

    fn validate(&self) -> Result<()> {
        if matches!(
            (&self.operation, &self.operand),
            (Operation::Pull, OperandRequest::Source { .. })
                | (
                    Operation::Push | Operation::PreviewPush,
                    OperandRequest::Destination { .. }
                )
        ) {
            Ok(())
        } else {
            Err(ProtocolError::InvalidMessage(
                "operand role disagrees with operation",
            ))
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let mut out = BytesMut::new();
        out.put_u8(self.operation as u8);
        self.operand.put(&mut out)?;
        put_path(&mut out, &self.path)?;
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let operation = Operation::try_from(reader.u8()?)?;
        let operand = OperandRequest::get(&mut reader)?;
        let path = get_path(&mut reader)?;
        reader.finish()?;
        let open = Self {
            operation,
            path,
            operand,
        };
        open.validate()?;
        Ok(open)
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

/// Reports the resolved physical query and its held or pending root authority.
/// A pending destination has no filesystem-qualified namespace profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReady {
    pub capabilities: CapabilitySet,
    /// Timestamp comparison resolution for this endpoint. Zero means unknown.
    pub modtime_precision_ns: u64,
    /// Name semantics of the actual root; pending roots are unqualified.
    pub namespace_semantics: WireNamespaceSemantics,
    pub resolved: ResolvedOperand,
    pub source_shape: Option<SourceShape>,
    pub pending: bool,
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
            namespace_semantics,
            resolved: ResolvedOperand::Tree,
            source_shape: None,
            pending: false,
        }
    }

    pub(crate) fn validate_for(&self, open: &SessionOpen) -> Result<()> {
        let valid = match (&open.operand, &self.source_shape, &self.resolved) {
            (
                OperandRequest::Source { contents },
                Some(SourceShape::Directory { basename }),
                ResolvedOperand::Tree,
            ) => !self.pending && (!contents || basename.is_none()),
            (
                OperandRequest::Source { .. },
                Some(SourceShape::Leaf { name }),
                ResolvedOperand::Entry { name: selected },
            ) => !self.pending && name == selected,
            (
                OperandRequest::Destination {
                    source: SourceShape::Directory { .. },
                },
                None,
                ResolvedOperand::Tree,
            )
            | (
                OperandRequest::Destination {
                    source: SourceShape::Leaf { .. },
                },
                None,
                ResolvedOperand::Entry { .. },
            ) => true,
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(ProtocolError::InvalidMessage(
                "resolved operand disagrees with request",
            ))
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut out = BytesMut::new();
        out.put_u64(self.capabilities.bits());
        out.put_u64(self.modtime_precision_ns);
        out.put_u8(self.namespace_semantics.encode());
        self.resolved.put(&mut out)?;
        match &self.source_shape {
            None => out.put_u8(0),
            Some(shape) => {
                out.put_u8(1);
                shape.put(&mut out)?;
            }
        }
        out.put_u8(u8::from(self.pending));
        Ok(out.freeze())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let capabilities = CapabilitySet::from_bits_retain(reader.u64()?);
        let modtime_precision_ns = reader.u64()?;
        let namespace_semantics = WireNamespaceSemantics::decode(reader.u8()?)?;
        let resolved = ResolvedOperand::get(&mut reader)?;
        let source_shape = match reader.u8()? {
            0 => None,
            1 => Some(SourceShape::get(&mut reader)?),
            _ => {
                return Err(ProtocolError::InvalidMessage(
                    "unknown source shape presence",
                ))
            }
        };
        let pending = match reader.u8()? {
            0 => false,
            1 => true,
            _ => return Err(ProtocolError::InvalidMessage("unknown pending root flag")),
        };
        reader.finish()?;
        Ok(Self {
            capabilities,
            modtime_precision_ns,
            namespace_semantics,
            resolved,
            source_shape,
            pending,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_WIRE_PATH_BYTES;

    #[test]
    fn session_open_round_trip_preserves_target_native_root() {
        for operation in [Operation::Push, Operation::Pull, Operation::PreviewPush] {
            let open = SessionOpen::new(
                operation,
                WirePath::new(Bytes::from_static(&[b'C', 0, b':', 0, b'\\', 0])).unwrap(),
            );
            let decoded = SessionOpen::decode(&open.encode().unwrap()).unwrap();
            assert_eq!(decoded, open);
        }
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
        let decoded = SessionReady::decode(&ready.encode().unwrap()).unwrap();
        assert_eq!(decoded, ready);
    }

    #[test]
    fn operand_layout_rejects_invalid_tags_lengths_and_truncation() {
        let name = WirePath::new(b"name\xff".to_vec()).unwrap();
        let open = SessionOpen {
            operation: Operation::Push,
            path: WirePath::new(b"/target".to_vec()).unwrap(),
            operand: OperandRequest::Destination {
                source: SourceShape::Leaf { name: name.clone() },
            },
        };
        let encoded = open.encode().unwrap();
        assert_eq!(SessionOpen::decode(&encoded).unwrap(), open);
        for len in 0..encoded.len() {
            assert!(SessionOpen::decode(&encoded[..len]).is_err());
        }
        for offset in [0, 1, 2] {
            let mut invalid = encoded.to_vec();
            invalid[offset] = 255;
            assert!(SessionOpen::decode(&invalid).is_err());
        }
        let mut oversized = encoded.to_vec();
        oversized[3..7].copy_from_slice(&((MAX_WIRE_PATH_BYTES + 1) as u32).to_be_bytes());
        assert!(matches!(
            SessionOpen::decode(&oversized),
            Err(ProtocolError::PathTooLong { .. })
        ));
        // Pull uses a distinct source request and a strictly boolean contents
        // tag; destination shapes cannot be smuggled into that role.
        let source = SessionOpen::new(Operation::Pull, WirePath::new(b"/source".to_vec()).unwrap());
        let mut invalid = source.encode().unwrap().to_vec();
        invalid[2] = 2;
        assert!(SessionOpen::decode(&invalid).is_err());
        let mut invalid = encoded.to_vec();
        invalid[0] = Operation::Pull as u8;
        assert!(SessionOpen::decode(&invalid).is_err());
        let mut ready = SessionReady::new(
            CapabilitySet::BLAKE3,
            0,
            WireNamespaceSemantics {
                case: NameFolding::Unspecified,
                normalization: NameFolding::Unspecified,
            },
        );
        ready.resolved = ResolvedOperand::Entry { name: name.clone() };
        ready.source_shape = Some(SourceShape::Leaf { name });
        let encoded = ready.encode().unwrap();
        assert_eq!(SessionReady::decode(&encoded).unwrap(), ready);
        for len in 0..encoded.len() {
            assert!(SessionReady::decode(&encoded[..len]).is_err());
        }
        for offset in [16, 17, 27, 28, encoded.len() - 1] {
            let mut invalid = encoded.to_vec();
            invalid[offset] = 255;
            assert!(SessionReady::decode(&invalid).is_err());
        }
        let mut oversized = encoded.to_vec();
        oversized[18..22].copy_from_slice(&((MAX_WIRE_PATH_BYTES + 1) as u32).to_be_bytes());
        assert!(matches!(
            SessionReady::decode(&oversized),
            Err(ProtocolError::PathTooLong { .. })
        ));
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
