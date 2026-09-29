use crate::endpoint::{Endpoint, FileMetadata};
use crate::error::{Result, SyncError};
use async_trait::async_trait;
use std::fs::File;
use std::path::Path;
use std::pin::Pin;
use sy::engine::domain::EntryIdentity;
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

/// Destination state a staged write must still observe at commit.
#[derive(Debug, Clone, Copy)]
pub enum ExpectedDestination {
    /// A create: the destination path must remain absent through commit.
    Absent,
    /// An update: the destination must still carry the scanned identity.
    Unchanged(EntryIdentity),
    /// Capture the current destination identity when staging begins.
    SnapshotAtOpen,
    /// No scan observation exists; destination race checks are skipped.
    Unverified,
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
/// Implementations write into endpoint-private staging state. The writer must
/// honor the `ExpectedDestination` passed to `Endpoint::begin_write` both when
/// staging begins and immediately before commit. `commit` makes the staged
/// object visible at the destination path; dropping or aborting a writer must
/// leave the previous destination intact whenever the endpoint can provide
/// atomic replacement semantics.
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

    /// Copy from an endpoint-provided held file into private staging. This is
    /// reserved for native same-host strategies; generic endpoint pairs use
    /// the bounded async `write` path. The source handle is returned so the
    /// transfer layer can validate the same observation after copying.
    async fn copy_from_native_file(&mut self, _source: File) -> Result<(File, u64)> {
        Err(SyncError::Config(
            "destination cannot copy from a native source handle".to_string(),
        ))
    }

    /// Attempt a sparse-aware copy from a held source into private staging.
    /// `None` means sparse extents are unavailable or the source has no holes;
    /// in that case staging is unchanged and the returned source can be copied
    /// through `copy_from_native_file` instead. Implementations must not retain
    /// an extent list proportional to file fragmentation.
    async fn copy_sparse_from_native_file(&mut self, source: File) -> Result<(File, Option<u64>)> {
        Ok((source, None))
    }

    async fn commit(self: Box<Self>) -> Result<()>;
    async fn abort(self: Box<Self>) -> Result<()>;
}

/// Abort owned staging while retaining both the operation and cleanup failures.
pub(crate) async fn abort_staged_writer(
    writer: Box<dyn StagedWriter>,
    operation: &SyncError,
) -> Result<()> {
    writer
        .abort()
        .await
        .map_err(|abort| SyncError::StagingAbortFailed {
            operation: operation.to_string(),
            abort: abort.to_string(),
        })
}

/// Apply common metadata/preservation, verify staged bytes, then commit.
///
/// `expected_hash` is computed from the trusted source byte stream. Comparing
/// it to the endpoint's staged hash proves the bytes to be published are the
/// bytes that were received, not merely that the incoming stream was valid.
/// Any pre-commit failure aborts only this writer's private staging.
pub(crate) async fn finalize_staged_writer(
    mut writer: Box<dyn StagedWriter>,
    metadata: &FileMetadata,
    preservation: &Preservation,
    expected_hash: Option<blake3::Hash>,
    pre_commit: Option<&(dyn Fn() -> Result<()> + Send + Sync)>,
) -> Result<VerificationStatus> {
    let prepared = async {
        writer.set_metadata(metadata).await?;
        writer
            .apply_preservation(preservation, metadata.preserved_mode())
            .await?;

        let verification = if let Some(expected) = expected_hash {
            let actual = writer.staged_hash().await?.ok_or_else(|| {
                SyncError::Config("destination cannot verify staged bytes before commit".into())
            })?;
            if expected != actual {
                return Ok(VerificationStatus::Failed { expected, actual });
            }
            VerificationStatus::Verified
        } else {
            VerificationStatus::NotRequested
        };

        if let Some(pre_commit) = pre_commit {
            pre_commit()?;
        }
        Ok(verification)
    }
    .await;

    match prepared {
        Ok(VerificationStatus::Failed { expected, actual }) => {
            let operation =
                format!("staged content hash mismatch: expected {expected}, got {actual}");
            let cause = SyncError::Config(operation);
            // An abort failure means staging cleanup is uncertain and must not
            // be reported as an ordinary verification result.
            abort_staged_writer(writer, &cause).await?;
            Ok(VerificationStatus::Failed { expected, actual })
        }
        Ok(verification) => {
            writer.commit().await?;
            Ok(verification)
        }
        Err(operation) => {
            abort_staged_writer(writer, &operation).await?;
            Err(operation)
        }
    }
}

/// Result of a bounded streaming copy.
#[derive(Debug, Clone, Copy)]
pub struct StreamCopyResult {
    pub bytes_written: u64,
    pub verification: VerificationStatus,
}

/// Inputs for one bounded streaming copy.
pub struct StreamCopyPolicy<'a> {
    /// Metadata observed for the selected source object (including a followed
    /// symlink target) and requested transfer overrides.
    pub metadata: &'a FileMetadata,
    /// Hash bytes as they flow and verify the staged result before commit.
    pub verify: bool,
    /// Destination state required at staging and again at commit.
    pub expected_destination: ExpectedDestination,
    /// Shared `--bwlimit` pacing applied at the userspace byte stream.
    pub rate_limiter:
        Option<&'a std::sync::Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// Preservation payload applied to staging before commit.
    pub preservation: &'a Preservation,
    /// Last-moment source race validation before commit. The staged writer
    /// validates its expected destination state itself.
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
/// immediately before the endpoint commit to validate source state. The
/// writer independently validates its expected destination state at commit.
/// Any error aborts staging.
pub async fn copy_file_streaming(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    policy: &StreamCopyPolicy<'_>,
) -> Result<StreamCopyResult> {
    const BUFFER_SIZE: usize = 1024 * 1024;

    let metadata = policy.metadata;
    let mut reader = source.open_reader(source_path).await?;
    let mut writer = dest
        .begin_write(dest_path, policy.expected_destination)
        .await?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = policy.verify.then(blake3::Hasher::new);
    let mut bytes_written = 0_u64;

    loop {
        let read = match reader.as_mut().read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                let operation = SyncError::Io(error);
                abort_staged_writer(writer, &operation).await?;
                return Err(operation);
            }
        };

        if read == 0 {
            break;
        }

        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&buffer[..read]);
        }
        if let Some(limiter) = policy.rate_limiter {
            // Discard the poisoned mutex error (which owns the guard) before
            // awaiting staging cleanup; no blocking guard may cross an await.
            let sleep = limiter
                .lock()
                .map(|mut limiter| limiter.consume(read as u64))
                .ok();
            let Some(sleep) = sleep else {
                let operation = SyncError::Config("rate limiter poisoned".to_string());
                abort_staged_writer(writer, &operation).await?;
                return Err(operation);
            };
            if !sleep.is_zero() {
                tokio::time::sleep(sleep).await;
            }
        }
        if let Err(operation) = writer.write(&buffer[..read]).await {
            abort_staged_writer(writer, &operation).await?;
            return Err(operation);
        }
        bytes_written += read as u64;
    }

    let expected_hash = hasher.map(|hasher| hasher.finalize());
    let verification = finalize_staged_writer(
        writer,
        metadata,
        policy.preservation,
        expected_hash,
        policy.pre_commit,
    )
    .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    struct RecordingWriter {
        events: Arc<Mutex<Vec<&'static str>>>,
        hash: blake3::Hash,
    }

    #[async_trait::async_trait]
    impl StagedWriter for RecordingWriter {
        async fn write(&mut self, _data: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn set_metadata(&mut self, _metadata: &FileMetadata) -> Result<()> {
            self.events.lock().unwrap().push("metadata");
            Ok(())
        }

        async fn apply_preservation(
            &mut self,
            _preservation: &Preservation,
            _expected_mode: Option<u32>,
        ) -> Result<()> {
            self.events.lock().unwrap().push("preservation");
            Ok(())
        }

        async fn staged_hash(&mut self) -> Result<Option<blake3::Hash>> {
            self.events.lock().unwrap().push("verification");
            Ok(Some(self.hash))
        }

        async fn commit(self: Box<Self>) -> Result<()> {
            self.events.lock().unwrap().push("commit");
            Ok(())
        }

        async fn abort(self: Box<Self>) -> Result<()> {
            self.events.lock().unwrap().push("abort");
            Ok(())
        }
    }

    #[tokio::test]
    async fn finalization_applies_metadata_before_verifying_and_committing() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let expected = blake3::hash(b"bytes");
        let writer = Box::new(RecordingWriter {
            events: Arc::clone(&events),
            hash: expected,
        });
        let metadata = FileMetadata {
            size: 5,
            modified: SystemTime::UNIX_EPOCH,
            is_dir: false,
            is_symlink: false,
            #[cfg(unix)]
            mode: 0o640,
        };
        let preservation = Preservation::default();
        let pre_commit_events = Arc::clone(&events);
        let pre_commit = move || {
            pre_commit_events.lock().unwrap().push("pre-commit");
            Ok(())
        };

        let verification = finalize_staged_writer(
            writer,
            &metadata,
            &preservation,
            Some(expected),
            Some(&pre_commit),
        )
        .await
        .unwrap();

        assert_eq!(verification, VerificationStatus::Verified);
        assert_eq!(
            *events.lock().unwrap(),
            [
                "metadata",
                "preservation",
                "verification",
                "pre-commit",
                "commit"
            ]
        );
    }
}
