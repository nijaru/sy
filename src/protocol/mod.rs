mod acl;
mod bsdflags;
mod codec;
mod directory;
pub use directory::{
    WireDirectoryAction, WireDirectoryMetadata, WireDirectoryPreservation, DIRECTORY_FINALIZE,
    DIRECTORY_READ,
};
mod entry;
mod fetch;
mod frame;
mod handshake;
mod hash;
mod metadata;
mod mutation;
mod path;
mod scan;
mod session;
mod signature;
mod source_metadata;
mod transfer;
mod xattr;

pub use acl::{WireAcl, WireAclResult, MAX_ACL_TEXT_BYTES};
pub use bsdflags::WireBsdFlagsResult;
pub use entry::{WireEntry, WireEntryKind};
pub use fetch::WireFileFetchRequest;
pub use frame::{
    read_frame, read_frame_or_eof, write_frame, Frame, FrameFlags, FrameKind, ReadFrame, StreamId,
    MAX_FRAME_PAYLOAD,
};
pub use handshake::{
    negotiate_version, CapabilitySet, ClientHello, Platform, PlatformArch, PlatformOs,
    ProtocolVersion, ServerHello, VersionRange, PROTOCOL_V3, PROTOCOL_V3_1, PROTOCOL_V3_2,
    PROTOCOL_V3_3, PROTOCOL_V3_4, PROTOCOL_V3_5, PROTOCOL_V3_6, SUPPORTED_VERSIONS,
};
pub use hash::{WireHashRequest, WireHashResult, HASH_DIGEST_LEN, HASH_IDENTITY_LEN};
pub use metadata::{WireMetadata, WireMetadataTarget};
pub use mutation::{WireMutation, WireMutationKind};
pub use path::{
    RelativeWirePath, WirePath, MAX_WIRE_COMPONENTS, MAX_WIRE_COMPONENT_BYTES, MAX_WIRE_PATH_BYTES,
};
pub use scan::WireScanRequest;
pub use session::{NameFolding, Operation, SessionOpen, SessionReady, WireNamespaceSemantics};
pub use signature::{
    SignatureBlockSize, WireSignature, WireSignatureEnd, WireSignatureRequest,
    MAX_SIGNATURE_BLOCK_SIZE, MIN_SIGNATURE_BLOCK_SIZE, STRONG_SIGNATURE_LEN,
};
pub use source_metadata::WireSourceMetadataRead;
pub use transfer::{
    WireData, WireDeltaCopy, WireFileAck, WireFileBasis, WireFileBegin, WireFileEnd,
    MAX_DELTA_COPY_SIZE, MAX_TRANSFER_DATA_SIZE, TRANSFER_BASIS_IDENTITY_LEN, TRANSFER_DIGEST_LEN,
};
pub use xattr::{
    WireXattr, WireXattrResult, MAX_XATTR_ENTRIES, MAX_XATTR_NAME_BYTES, MAX_XATTR_TOTAL_BYTES,
};

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("protocol I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("clean end of stream at a frame boundary")]
    CleanEof,

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

    #[error("wire path component exceeds maximum length: {len} bytes (maximum {max})")]
    PathComponentTooLong { len: usize, max: usize },

    #[error("wire path has too many components: {count} (maximum {max})")]
    TooManyPathComponents { count: usize, max: usize },

    #[error("invalid relative wire path: {0}")]
    InvalidRelativePath(&'static str),

    #[error("no compatible protocol version (client {client:?}, server {server:?})")]
    NoCompatibleVersion {
        client: VersionRange,
        server: VersionRange,
    },

    #[error("xattr name exceeds maximum length: {len} bytes (maximum {max})")]
    XattrNameTooLong { len: usize, max: usize },

    #[error("xattr set exceeds maximum size: {len} bytes (maximum {max})")]
    XattrPayloadTooLarge { len: usize, max: usize },

    #[error("too many xattr entries: {count} (maximum {max})")]
    TooManyXattrs { count: usize, max: usize },

    #[error("acl text exceeds maximum size: {len} bytes (maximum {max})")]
    AclTextTooLarge { len: usize, max: usize },
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
