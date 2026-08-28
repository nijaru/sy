use crate::endpoint::{Endpoint, FileMetadata};
use crate::error::Result;
use async_trait::async_trait;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt};

/// A streaming reader returned by an endpoint.
///
/// The transfer layer must consume this incrementally. Endpoint boundaries
/// should never require materializing an entire file in memory.
pub type BoxReader = Box<dyn AsyncRead + Send + Unpin>;

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

/// Result of a bounded streaming copy.
#[derive(Debug, Clone)]
pub struct StreamCopyResult {
    pub bytes_written: u64,
    /// BLAKE3 of the source bytes as they passed through the transfer pipeline.
    /// This can be compared with a destination stream hash when verification is
    /// requested without rereading the source.
    pub source_hash: blake3::Hash,
}

/// Copy one file between endpoints without whole-file buffering.
///
/// Data is read in fixed-size chunks, hashed as it passes through the pipeline,
/// written to endpoint-private staging state, and made visible only by commit.
/// Any read/write/metadata failure aborts the staged destination best-effort.
pub async fn copy_file_streaming(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
) -> Result<StreamCopyResult> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let metadata = source.metadata(source_path).await?;
    let mut reader = source.open_reader(source_path).await?;
    let mut writer = dest.begin_write(dest_path).await?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();
    let mut bytes_written = 0_u64;

    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                let _ = writer.abort().await;
                return Err(error.into());
            }
        };

        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
        if let Err(error) = writer.write(&buffer[..read]).await {
            let _ = writer.abort().await;
            return Err(error);
        }
        bytes_written += read as u64;
    }

    if let Err(error) = writer.set_metadata(&metadata).await {
        let _ = writer.abort().await;
        return Err(error);
    }

    writer.commit().await?;

    Ok(StreamCopyResult {
        bytes_written,
        source_hash: hasher.finalize(),
    })
}

/// Hash a file through the endpoint streaming API.
///
/// Used by transfer verification so the destination can be checked without
/// ever materializing the file in memory.
pub async fn hash_file_streaming(endpoint: &dyn Endpoint, path: &Path) -> Result<blake3::Hash> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let mut reader = endpoint.open_reader(path).await?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize())
}
