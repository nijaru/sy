use crate::endpoint::FileMetadata;
use crate::error::Result;
use async_trait::async_trait;
use std::pin::Pin;
use tokio::io::AsyncRead;

/// A streaming reader returned by an endpoint.
///
/// The transfer layer must consume this incrementally. Endpoint boundaries
/// should never require materializing an entire file in memory.
pub type BoxReader = Pin<Box<dyn AsyncRead + Send>>;

/// Transactional destination write.
///
/// Implementations write into endpoint-private staging state. `commit` makes
/// the staged object visible at the destination path; dropping or aborting a
/// writer must leave the previous destination intact whenever the endpoint can
/// provide atomic replacement semantics.
#[async_trait]
pub trait StagedWriter: Send {
    /// Append a chunk of file data to the staged object.
    async fn write(&mut self, data: &[u8]) -> Result<()>;

    /// Apply file metadata to the staged object before it becomes visible.
    async fn set_metadata(&mut self, metadata: &FileMetadata) -> Result<()>;

    /// Make the staged object visible at its final destination.
    async fn commit(self: Box<Self>) -> Result<()>;

    /// Explicitly discard staged state.
    ///
    /// Dropping a writer without committing must also clean up best-effort.
    async fn abort(self: Box<Self>) -> Result<()>;
}
