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

/// Preservation payload read from a validated source before any destination
/// mutation and applied to private staging before commit.
///
/// Applying preservation before commit means a failure aborts staging and the
/// previous destination survives; no entry commits without its requested
/// xattrs/ACLs. BSD/platform flags that can block rename stay post-commit
/// finalization (see the transfer layer).
#[derive(Debug, Clone, Default)]
pub struct Preservation {
    pub xattrs: Option<Vec<(std::ffi::OsString, Vec<u8>)>>,
    pub acl: Option<String>,
}

impl Preservation {
    pub const fn is_empty(&self) -> bool {
        self.xattrs.is_none() && self.acl.is_none()
    }
}

/// Verify the staged file's effective mode after preservation application.
///
/// ACL application can rewrite the mode bits it shares (the POSIX mask), so
/// the transfer layer proves the interaction held instead of assuming the
/// order was harmless.
#[cfg(unix)]
pub(crate) fn verify_staged_mode(path: &Path, expected_mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let Some(expected_mode) = expected_mode else {
        return Ok(());
    };
    // `FileMetadata.mode` may carry the full st_mode; permission bits are the
    // contract here.
    let expected = expected_mode & 0o7777;
    let observed = std::fs::symlink_metadata(path)?.permissions().mode() & 0o7777;
    if observed != expected {
        return Err(SyncError::PreservationConflict {
            path: path.to_path_buf(),
            expected,
            actual: observed,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn verify_staged_mode(_path: &Path, _expected_mode: Option<u32>) -> Result<()> {
    Ok(())
}

impl FileMetadata {
    /// The mode this platform preserves, when it records one.
    pub const fn preserved_mode(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            Some(self.mode)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
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

    /// Apply the preservation payload (xattrs/ACLs) to private staging state
    /// before commit, then prove the effective mode still matches `metadata`.
    ///
    /// An error aborts staging: preservation failures never commit content
    /// without its requested attributes.
    async fn apply_preservation(
        &mut self,
        preservation: &Preservation,
        expected_mode: Option<u32>,
    ) -> Result<()> {
        let _ = (preservation, expected_mode);
        Err(SyncError::Config(
            "endpoint cannot apply staged preservation".to_string(),
        ))
    }

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

/// Inputs for one bounded streaming copy.
pub struct StreamCopyPolicy<'a> {
    /// Hash bytes as they flow and verify the staged result before commit.
    pub verify: bool,
    /// Shared `--bwlimit` pacing applied at the userspace byte stream.
    pub rate_limiter:
        Option<&'a std::sync::Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// Preservation payload applied to staging before commit.
    pub preservation: &'a Preservation,
    /// Last-moment validation (source/destination race checks) before commit.
    pub pre_commit: Option<&'a (dyn Fn() -> Result<()> + Send + Sync)>,
}

/// Copy one file between endpoints without whole-file buffering.
///
/// Hashing is opt-in. When verification is requested, the source is hashed as
/// bytes flow through the pipeline and the staged destination is hashed before
/// commit. A mismatch aborts staging and leaves the old destination intact.
///
/// `policy.rate_limiter` optionally paces the copy: each chunk consumes tokens
/// from the shared `--bwlimit` bucket before it is written, so the pacing point
/// is exactly the userspace byte stream (native kernel copies bypass this path
/// entirely when a limit is set).
///
/// `policy.pre_commit` runs after all bytes and preservation are staged and
/// immediately before the endpoint commit — the transfer layer's last chance
/// to validate source/destination race expectations. An error aborts staging.
pub async fn copy_file_streaming(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    policy: &StreamCopyPolicy<'_>,
) -> Result<StreamCopyResult> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let metadata = source.metadata(source_path).await?;
    let mut reader = source.open_reader(source_path).await?;
    let mut writer = dest.begin_write(dest_path).await?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = policy.verify.then(blake3::Hasher::new);
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
        if let Some(limiter) = policy.rate_limiter {
            let sleep = limiter
                .lock()
                .map_err(|_| SyncError::Config("rate limiter poisoned".to_string()))?
                .consume(u64::try_from(read).unwrap_or(0));
            if !sleep.is_zero() {
                tokio::time::sleep(sleep).await;
            }
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

    if let Err(error) = writer
        .apply_preservation(policy.preservation, metadata.preserved_mode())
        .await
    {
        let _ = writer.abort().await;
        return Err(error);
    }

    if let Some(pre_commit) = policy.pre_commit {
        if let Err(error) = pre_commit() {
            let _ = writer.abort().await;
            return Err(error);
        }
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
