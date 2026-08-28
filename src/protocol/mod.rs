mod frame;
mod handshake;
mod path;

pub use frame::{read_frame, write_frame, Frame, FrameFlags, FrameKind, StreamId, MAX_FRAME_PAYLOAD};
pub use handshake::{
    negotiate_version, CapabilitySet, ClientHello, Operation, Platform, ProtocolVersion,
    ServerHello, VersionRange, PROTOCOL_V3,
};
pub use path::{RelativeWirePath, WirePath, MAX_WIRE_PATH_BYTES};

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("protocol I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("frame payload is too large: {len} bytes (maximum {max})")]
    PayloadTooLarge { len: usize, max: usize },

    #[error("unknown frame kind: {0}")]
    UnknownFrameKind(u8),

    #[error("unknown frame flags: 0x{0:02x}")]
    UnknownFrameFlags(u8),

    #[error("frame reserved field must be zero, got 0x{0:04x}")]
    NonZeroReserved(u16),

    #[error("invalid protocol message: {0}")]
    InvalidMessage(&'static str),

    #[error("invalid protocol field {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },

    #[error("wire path exceeds maximum length: {len} bytes (maximum {max})")]
    PathTooLong { len: usize, max: usize },

    #[error("invalid relative wire path: {0}")]
    InvalidRelativePath(&'static str),

    #[error("no compatible protocol version (client {client:?}, server {server:?})")]
    NoCompatibleVersion {
        client: VersionRange,
        server: VersionRange,
    },
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
