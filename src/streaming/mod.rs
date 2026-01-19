//! Streaming protocol v2 for sy.
//!
//! Replaces request-response model with rsync-style unidirectional streaming.
//! Three-task pipeline: Generator -> Sender -> Receiver
//!
//! # Architecture
//!
//! ```text
//! Push (local -> remote):
//! +--------------+     +--------------+     +--------------+
//! |  Generator   | --> |    Sender    | --> |   Receiver   |
//! | (local scan) |     | (delta/data) |     | (remote write)|
//! +--------------+     +--------------+     +--------------+
//! ```
//!
//! # Protocol v2
//!
//! Two-phase design:
//! 1. Initial Exchange - Receiver streams DEST_FILE_ENTRY with checksums
//! 2. Streaming Transfer - Pure unidirectional flow, no round-trips
//!
//! See `ai/design/streaming-protocol-v0.3.0.md` for full specification.

#![allow(unused_imports, dead_code)]

pub mod channel;
pub mod generator;
pub mod pipeline;
pub mod protocol;
pub mod receiver;
pub mod sender;

pub use channel::{
    DataChunk, DeltaInfo, DestFileState, DestIndex, FileJob, FileJobReceiver, FileJobSender,
    GeneratorMessage, SyncDirection, SyncStats, DATA_CHUNK_SIZE, DELTA_MIN_SIZE,
    GENERATOR_CHANNEL_SIZE, SENDER_CHANNEL_SIZE,
};

pub use generator::{Generator, GeneratorConfig};
pub use pipeline::StreamingSync;
pub use receiver::{Receiver, ReceiverConfig};
pub use sender::{Sender, SenderConfig};

pub use protocol::{
    BlockChecksum, Data, DataEnd, DataFlags, Delete, DeleteEnd, DestFileEnd, DestFileEntry,
    DestFileFlags, Done, Error, ErrorCode, Fatal, FileEnd, FileEntry, FileFlags, Hello, HelloFlags,
    MessageType, Mkdir, Progress, Symlink, Xattr, XattrEntry, PROTOCOL_VERSION,
    PROTOCOL_VERSION_MAX, PROTOCOL_VERSION_MIN, PROTOCOL_VERSION_V1,
};

pub use protocol::{
    is_legacy_protocol, is_streaming_protocol, negotiate_version, read_frame, write_frame,
    VersionNegotiationResult,
};
