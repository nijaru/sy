use crate::endpoint::{Endpoint, FileMetadata};
use crate::error::{Result, SyncError};
use async_trait::async_trait;
use std::path::Path;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt};

/// A streaming reader returned by an endpoint.
pub type BoxReader = Pin<Box<dyn AsyncRead + Send>>;

/// Verification state for a staged transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    NotRequested,
    Verified,
    Failed {
        expected: blake3::Hash,
        actual: blake3::Hash,
    },
}

/// Transactional destination write.
///
/// Implementations write into endpoint-private staging state. `commit` makes
/// the staged object visible at the destination path; dropping or aborting a
/// writer must leave the previous destination intact whenever the endpoint can
/// provide atomic replacement semantics.
#[async_trait]
pub trait StagedWriter: Send {
    async fn write(&mut self, data: &[u8]) -> Result<()>;
    async fn set_metadata(&mut self, metadata: &FileMetadata) -> Result<()>;

    /// Hash the bytes currently in staging without making them visible.
    ///
    /// Endpoints that advertise staged verification must return `Some(hash)`.
    async fn staged_hash(&mut self) -> Result<Option<blake3::Hash>> {
        Ok(None)
    }

    async fn commit(self: Box<Self>) -> Result<()>;
    async fn abort(self: Box<Self>) -> Result<()>;
}

/// Result of a bounded streaming copy.
#[derive(Debug, Clone, Copy)]
pub struct StreamCopyResult {
    pub bytes_written: u64,
    pub verification: VerificationStatus,
}

/// Copy one file between endpoints without whole-file buffering.
///
/// Hashing is opt-in. When verification is requested, the source is hashed as
/// bytes flow through the pipeline and the staged destination is hashed before
/// commit. A mismatch aborts staging and leaves the old destination intact.
pub async fn copy_file_streaming(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    verify: bool,
) -> Result<StreamCopyResult> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let metadata = source.metadata(source_path).await?;
    let mut reader = source.open_reader(source_path).await?;
    let mut writer = dest.begin_write(dest_path).await?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = verify.then(blake3::Hasher::new);
    let mut bytes_written = 0_u64;

    loop {
        let read = match reader.as_mut().read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                let _ = writer.abort().await;
                return Err(error.into());
            }
        };

        if read == 0 {
            break;
        }

        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&buffer[..read]);
        }
        if let Err(error) = writer.write(&buffer[..read]).await {
            let _ = writer.abort().await;
            return Err(error);
        }
        bytes_written += read as u64;
    }

    let verification = if let Some(hasher) = hasher {
        let expected = hasher.finalize();
        let actual = match writer.staged_hash().await {
            Ok(Some(hash)) => hash,
            Ok(None) => {
                let _ = writer.abort().await;
                return Err(SyncError::Config(format!(
                    "{:?} destination does not support pre-commit verification",
                    dest.endpoint_type()
                )));
            }
            Err(error) => {
                let _ = writer.abort().await;
                return Err(error);
            }
        };

        if expected != actual {
            let _ = writer.abort().await;
            return Ok(StreamCopyResult {
                bytes_written,
                verification: VerificationStatus::Failed { expected, actual },
            });
        }
        VerificationStatus::Verified
    } else {
        VerificationStatus::NotRequested
    };

    if let Err(error) = writer.set_metadata(&metadata).await {
        let _ = writer.abort().await;
        return Err(error);
    }

    writer.commit().await?;
    Ok(StreamCopyResult {
        bytes_written,
        verification,
    })
}

/// Hash a visible file through the endpoint streaming API.
pub async fn hash_file_streaming(endpoint: &dyn Endpoint, path: &Path) -> Result<blake3::Hash> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let mut reader = endpoint.open_reader(path).await?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();

    loop {
        let read = reader.as_mut().read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.finalize())
}
