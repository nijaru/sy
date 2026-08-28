use super::codec::SliceReader;
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

/// Confirms that the root was opened and reports capabilities for that concrete
/// endpoint/filesystem, which may be narrower than process-wide capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionReady {
    pub capabilities: CapabilitySet,
    /// Timestamp comparison resolution for this endpoint. Zero means unknown.
    pub modtime_precision_ns: u64,
}

impl SessionReady {
    pub const fn new(capabilities: CapabilitySet, modtime_precision_ns: u64) -> Self {
        Self {
            capabilities,
            modtime_precision_ns,
        }
    }

    pub fn encode(self) -> Bytes {
        let mut out = BytesMut::with_capacity(16);
        out.put_u64(self.capabilities.bits());
        out.put_u64(self.modtime_precision_ns);
        out.freeze()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut reader = SliceReader::new(payload);
        let capabilities = CapabilitySet::from_bits_retain(reader.u64()?);
        let modtime_precision_ns = reader.u64()?;
        reader.finish()?;
        Ok(Self {
            capabilities,
            modtime_precision_ns,
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
        let ready = SessionReady::new(CapabilitySet::from_bits_retain(1_u64 << 63), 1);
        let decoded = SessionReady::decode(&ready.encode()).unwrap();
        assert_eq!(decoded, ready);
    }
}
