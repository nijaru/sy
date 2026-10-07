//! Capability-driven file transfer selection for the v0.5 architecture.
//!
//! The common path prefers endpoint-native filesystem primitives when both
//! sides expose native paths. Generic endpoint pairs fall back to bounded
//! staged streaming. Local updates may use a reflink clone + patch when that
//! reduces physical writes; non-COW local files use a normal whole-file copy.

use crate::endpoint::io::{
    copy_file_streaming_from_reader, ExpectedDestination, Preservation, PreservationRequest,
    StreamCopyPolicy, VerificationStatus,
};
use crate::endpoint::receipt::PublishedDestinationReceipt;
use crate::endpoint::{Endpoint, FileMetadata};
use crate::error::{Result, SyncError};
use std::future::Future;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use sy::engine::domain::{EntryIdentity, Timestamp};

const TRANSFER_BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStrategy {
    NativeSparseCopy,
    ReflinkPatch,
    NativeWholeCopy,
    Streaming,
}

#[derive(Debug, Clone)]
pub struct TransferOptions {
    pub update: bool,
    pub verify: bool,
    /// --copy-links: the source entry is the TARGET of a symlink; the scan
    /// classified it by target kind. The transfer guard and size decisions
    /// must therefore stat through the link, and plain opens follow it.
    pub follow_symlinks: bool,
    /// Optional shared bandwidth pacing in bytes per second (`--bwlimit`).
    /// When set, native kernel fast paths (fs::copy, reflink) are bypassed:
    /// pacing requires bytes to flow through this process where the token
    /// bucket can meter them.
    pub rate_limiter: Option<std::sync::Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// Scan-time identity expectations validated at transfer start and commit.
    pub identity: TransferIdentity,
    /// Preservation payload read from the validated source; applied to staging
    /// before commit so a failure aborts instead of committing bare content.
    pub preservation: Preservation,
    /// Attributes to capture from the same opened source file that supplies
    /// bytes. Requested fields replace the corresponding payload values.
    pub preservation_request: PreservationRequest,
    /// When true, the resulting publication receipt remains unfinalized until
    /// the caller executes required post-commit finalization.
    pub pending_finalization: bool,
    /// Requested mode and mtime to apply to staging instead of source metadata.
    pub metadata: Option<TransferMetadata>,
}

/// Policy-selected mode and timestamp for a staged transfer.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransferMetadata {
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
}

pub(crate) fn timestamp_to_system_time(timestamp: Timestamp) -> Option<std::time::SystemTime> {
    let nanos = u64::from(timestamp.nanoseconds());
    if timestamp.seconds() >= 0 {
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(
            u64::try_from(timestamp.seconds()).ok()?,
            timestamp.nanoseconds(),
        ))
    } else {
        // Timestamps use a non-negative nanosecond field even before the epoch:
        // (-1, 999_999_999) is one nanosecond before it.
        let delta = std::time::Duration::from_secs(timestamp.seconds().unsigned_abs())
            .checked_sub(std::time::Duration::from_nanos(nanos))?;
        std::time::UNIX_EPOCH.checked_sub(delta)
    }
}

fn apply_requested_metadata(
    metadata: &mut FileMetadata,
    requested: Option<TransferMetadata>,
    source_path: &Path,
) -> Result<()> {
    if let Some(transfer_metadata) = requested {
        if let Some(modified) = transfer_metadata.modified {
            metadata.modified = timestamp_to_system_time(modified).ok_or_else(|| {
                SyncError::Config(format!(
                    "requested timestamp for {} is outside the supported range",
                    source_path.display()
                ))
            })?;
        }
        #[cfg(unix)]
        if let Some(mode) = transfer_metadata.unix_mode {
            metadata.mode = mode;
        }
        #[cfg(not(unix))]
        if transfer_metadata.unix_mode.is_some() {
            return Err(SyncError::Config(
                "Unix mode preservation is not supported on this platform".to_string(),
            ));
        }
    }
    Ok(())
}

async fn generic_source_metadata(
    source: &dyn Endpoint,
    source_path: &Path,
    follow_symlinks: bool,
    requested: Option<TransferMetadata>,
) -> Result<FileMetadata> {
    let mut metadata = source.metadata(source_path).await?;
    if follow_symlinks && metadata.is_symlink {
        // The scan reported the target; preserve its metadata as well as its
        // bytes when --copy-links is active.
        metadata = source.metadata_following(source_path).await?;
    }
    if metadata.is_dir || metadata.is_symlink {
        return Err(SyncError::Config(format!(
            "file transfer requested for non-regular source {}",
            source_path.display()
        )));
    }
    apply_requested_metadata(&mut metadata, requested, source_path)?;
    Ok(metadata)
}

fn file_metadata_from_open_file(file: &std::fs::File) -> Result<FileMetadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SyncError::Config(
            "native transfer source is not a regular file".to_string(),
        ));
    }
    Ok(FileMetadata {
        size: metadata.len(),
        modified: metadata.modified()?,
        is_dir: false,
        is_symlink: false,
        #[cfg(unix)]
        mode: metadata.mode(),
    })
}

fn verify_open_source_identity(
    file: &std::fs::File,
    expected: Option<EntryIdentity>,
    path: &Path,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let metadata = file.metadata()?;
    let actual = crate::endpoint::local_identity::metadata_identity(
        &metadata,
        sy::engine::domain::EntryKind::File,
    );
    if actual != Some(expected) {
        return Err(SyncError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn verify_open_source_size(file: &std::fs::File, expected_size: u64, path: &Path) -> Result<()> {
    if file.metadata()?.len() != expected_size {
        return Err(SyncError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_open_destination_identity(
    file: &std::fs::File,
    expected: EntryIdentity,
    path: &Path,
) -> Result<()> {
    let metadata = file.metadata()?;
    let actual = crate::endpoint::local_identity::metadata_identity(
        &metadata,
        sy::engine::domain::EntryKind::File,
    );
    if actual != Some(expected) {
        return Err(SyncError::DestinationChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferResult {
    pub bytes_written: u64,
    pub strategy: TransferStrategy,
    pub verification: VerificationStatus,
    pub receipt: PublishedDestinationReceipt,
}

#[derive(Debug, Clone, Copy)]
struct NativeTransferResult {
    bytes_written: u64,
    verification: VerificationStatus,
}

struct NativeStagedCopyPolicy {
    metadata: FileMetadata,
    expected_destination: ExpectedDestination,
    verify: bool,
    checks: CommitChecks,
    preservation: Preservation,
}

struct NativeReflinkPatchPolicy {
    source_path: PathBuf,
    metadata: FileMetadata,
    expected_destination: ExpectedDestination,
    verify: bool,
    checks: CommitChecks,
    preservation: Preservation,
}

/// What the transfer must prove about the source before committing bytes.
#[derive(Debug, Clone, Copy)]
pub enum SourceExpectation {
    /// Scan-time identity: validated at transfer start and again at commit.
    Scanned(EntryIdentity),
    /// No scan observation exists (single-file copy): capture an identity at
    /// transfer start and validate it at commit to detect mid-transfer edits.
    SnapshotAtOpen,
    /// No observation is available; source race checks are skipped and the
    /// weaker guarantee is the caller's documented limitation.
    Unverified,
}

/// Scan-time identity expectations for one transfer.
#[derive(Debug, Clone, Copy)]
pub struct TransferIdentity {
    pub source: SourceExpectation,
    pub destination: ExpectedDestination,
}

impl TransferIdentity {
    /// No race observations at all. Only for callers that genuinely cannot
    /// observe identity on their platform.
    pub const fn unverified() -> Self {
        Self {
            source: SourceExpectation::Unverified,
            destination: ExpectedDestination::Unverified,
        }
    }
}

/// One observed identity state of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observation {
    Identified(EntryIdentity),
    /// The platform cannot produce identity tokens (weaker guarantee).
    Unidentified,
    Missing,
}

fn observe(path: &Path, follow_symlinks: bool) -> Result<Observation> {
    let metadata = if follow_symlinks {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    match metadata {
        Ok(metadata) => Ok(
            match crate::endpoint::local_identity::identity_for_metadata(&metadata) {
                Some(identity) => Observation::Identified(identity),
                None => Observation::Unidentified,
            },
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Observation::Missing),
        Err(error) => Err(SyncError::Io(error)),
    }
}

#[derive(Debug, Clone)]
struct SourceCheck {
    path: PathBuf,
    follow_symlinks: bool,
    identity: EntryIdentity,
}

#[derive(Debug, Clone)]
enum DestinationCheck {
    Absent(PathBuf),
    Unchanged {
        path: PathBuf,
        identity: EntryIdentity,
    },
    Unverified,
}

/// Deterministic race-injection point: runs immediately before the commit
/// checks, inside the scan/commit race window. Production callers pass `None`;
/// unit tests inject filesystem mutations here to exercise abort paths.
type RaceHook = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Which validation point is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckPoint {
    /// Right after expectation resolution, before any staging work.
    Open,
    /// Immediately before the visible commit.
    Commit,
}

/// Commit-time race expectations for one transfer.
///
/// Portable filesystem race detection observes source and destination with
/// separate stat/rename syscalls. Ordinary concurrent edits, replacements,
/// truncations, and growth change the identity token and abort staging before
/// commit; an adversarial ABA replacement that restores every observed field
/// (including ctime) is outside what portable stat can prove.
#[derive(Clone)]
struct CommitChecks {
    source: Option<SourceCheck>,
    destination: DestinationCheck,
    test_hook: Option<RaceHook>,
}

fn require_native(path: Option<&Path>, role: &str) -> Result<PathBuf> {
    path.map(Path::to_path_buf).ok_or_else(|| {
        SyncError::Config(format!(
            "endpoint cannot re-observe {role} identity for transfer race checks"
        ))
    })
}

impl CommitChecks {
    fn resolve(
        identity: TransferIdentity,
        source_native: Option<&Path>,
        dest_native: Option<&Path>,
        follow_symlinks: bool,
        test_hook: Option<RaceHook>,
    ) -> Result<Self> {
        let source = match identity.source {
            SourceExpectation::Scanned(expected) => {
                let path = require_native(source_native, "source")?;
                Some(SourceCheck {
                    path,
                    follow_symlinks,
                    identity: expected,
                })
            }
            SourceExpectation::SnapshotAtOpen => {
                let path = require_native(source_native, "source")?;
                match observe(&path, follow_symlinks)? {
                    Observation::Identified(identity) => Some(SourceCheck {
                        path,
                        follow_symlinks,
                        identity,
                    }),
                    Observation::Missing => {
                        return Err(SyncError::SourceChanged { path });
                    }
                    Observation::Unidentified => None,
                }
            }
            SourceExpectation::Unverified => None,
        };
        let destination = match identity.destination {
            ExpectedDestination::Absent => {
                let path = require_native(dest_native, "destination")?;
                DestinationCheck::Absent(path)
            }
            ExpectedDestination::Unchanged(expected) => {
                let path = require_native(dest_native, "destination")?;
                DestinationCheck::Unchanged {
                    path,
                    identity: expected,
                }
            }
            ExpectedDestination::SnapshotAtOpen => {
                let path = require_native(dest_native, "destination")?;
                match observe(&path, false)? {
                    Observation::Identified(identity) => {
                        DestinationCheck::Unchanged { path, identity }
                    }
                    Observation::Missing => {
                        return Err(SyncError::DestinationChanged { path });
                    }
                    Observation::Unidentified => DestinationCheck::Unverified,
                }
            }
            ExpectedDestination::Unverified => DestinationCheck::Unverified,
        };
        Ok(Self {
            source,
            destination,
            test_hook,
        })
    }

    /// Prove the source observation still holds. The test hook runs at the
    /// final check so destination mutations are caught by the staged writer.
    fn verify_source(&self, checkpoint: CheckPoint) -> Result<()> {
        if checkpoint == CheckPoint::Commit {
            if let Some(hook) = &self.test_hook {
                hook();
            }
        }
        if let Some(expected) = &self.source {
            if observe(&expected.path, expected.follow_symlinks)?
                != Observation::Identified(expected.identity)
            {
                return Err(SyncError::SourceChanged {
                    path: expected.path.clone(),
                });
            }
        }
        Ok(())
    }

    fn verify_destination(&self) -> Result<()> {
        match &self.destination {
            DestinationCheck::Absent(path) => {
                if observe(path, false)? != Observation::Missing {
                    return Err(SyncError::DestinationChanged { path: path.clone() });
                }
            }
            DestinationCheck::Unchanged { path, identity } => {
                if observe(path, false)? != Observation::Identified(*identity) {
                    return Err(SyncError::DestinationChanged { path: path.clone() });
                }
            }
            DestinationCheck::Unverified => {}
        }
        Ok(())
    }

    fn expected_source_identity(&self) -> Option<EntryIdentity> {
        self.source.as_ref().map(|source| source.identity)
    }

    fn expected_destination(&self) -> ExpectedDestination {
        match self.destination {
            DestinationCheck::Absent(_) => ExpectedDestination::Absent,
            DestinationCheck::Unchanged { identity, .. } => {
                ExpectedDestination::Unchanged(identity)
            }
            DestinationCheck::Unverified => ExpectedDestination::Unverified,
        }
    }

    fn verify(&self, checkpoint: CheckPoint) -> Result<()> {
        self.verify_source(checkpoint)?;
        self.verify_destination()
    }
}

/// Transfer a file using endpoint capabilities rather than endpoint-specific
/// policy in the caller.
pub async fn transfer_file(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    options: TransferOptions,
) -> Result<TransferResult> {
    transfer_file_inner(
        source,
        source_path,
        dest,
        dest_path,
        options,
        None,
        || async { Ok(()) },
    )
    .await
}

/// Transfer after asynchronously preparing any caller-owned destination
/// side effects, once source preservation has been captured and before staging
/// begins.
pub(crate) async fn transfer_file_with_before_stage<F, Fut>(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    options: TransferOptions,
    before_stage: F,
) -> Result<TransferResult>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send,
{
    transfer_file_inner(
        source,
        source_path,
        dest,
        dest_path,
        options,
        None,
        before_stage,
    )
    .await
}

/// Transfer with a deterministic race hook for unit tests.
#[cfg(test)]
pub(crate) async fn transfer_file_with_hook(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    options: TransferOptions,
    test_hook: RaceHook,
) -> Result<TransferResult> {
    transfer_file_inner(
        source,
        source_path,
        dest,
        dest_path,
        options,
        Some(test_hook),
        || async { Ok(()) },
    )
    .await
}

async fn transfer_file_inner<F, Fut>(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    options: TransferOptions,
    test_hook: Option<RaceHook>,
    before_stage: F,
) -> Result<TransferResult>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send,
{
    let source_caps = source.capabilities();
    let dest_caps = dest.capabilities();

    tracing::trace!(
        source = ?source.endpoint_type(),
        dest = ?dest.endpoint_type(),
        source_streaming = source_caps.streaming_read,
        dest_staged = dest_caps.staged_write,
        dest_atomic = dest_caps.atomic_rename,
        dest_server_copy = dest_caps.server_side_copy,
        dest_mtime_precision_ns = dest_caps.modtime_precision.as_nanos(),
        verify = options.verify,
        "selecting transfer strategy"
    );

    // Race checks need re-observable native paths; strategies below may still
    // prefer streaming. Native strategy selection additionally requires the
    // bytes to flow through this process when pacing is requested.
    let source_native = source.native_path(source_path);
    let dest_native = dest.native_path(dest_path);
    let checks = CommitChecks::resolve(
        options.identity,
        source_native.as_deref(),
        dest_native.as_deref(),
        options.follow_symlinks,
        test_hook,
    )?;
    checks.verify(CheckPoint::Open)?;
    let expected_destination = checks.expected_destination();
    let mut before_stage = Some(before_stage);
    let dest_relative = sy::engine::domain::RelativePath::new(dest_path.to_path_buf())
        .map_err(|error| SyncError::Config(error.to_string()))?;
    let pending_finalization = options.pending_finalization;

    let native_strategy = options.rate_limiter.is_none()
        && source_native.is_some()
        && dest_native.is_some()
        && dest_caps.atomic_rename
        && !options.follow_symlinks;
    let preservation_requested =
        options.preservation_request.xattrs || options.preservation_request.acl;
    // Bind bytes and metadata to one opened observation even when bandwidth
    // pacing or destination capabilities select streaming rather than native
    // copy. Path checks alone miss an ancestor swap restored after reader open.
    let mut source_file = if options.follow_symlinks {
        source.open_native_file_following(source_path).await?
    } else {
        source.open_native_file(source_path).await?
    };
    let metadata = if let Some(file) = source_file.as_ref() {
        verify_open_source_identity(file, checks.expected_source_identity(), source_path)?;
        let mut metadata = file_metadata_from_open_file(file)?;
        apply_requested_metadata(&mut metadata, options.metadata, source_path)?;
        metadata
    } else {
        if source_native.is_some() || checks.expected_source_identity().is_some() {
            return Err(SyncError::Config(format!(
                "{:?} endpoint cannot bind source identity to the file supplying transfer bytes",
                source.endpoint_type()
            )));
        }
        // Generic endpoints may stream under the caller's explicit Unverified
        // contract; never downgrade a requested native identity guarantee.
        generic_source_metadata(
            source,
            source_path,
            options.follow_symlinks,
            options.metadata,
        )
        .await?
    };
    let mut preservation = options.preservation;
    if preservation_requested {
        let Some(file) = source_file.as_ref() else {
            return Err(SyncError::Config(format!(
                "{:?} endpoint cannot capture requested preservation from the file supplying transfer bytes",
                source.endpoint_type()
            )));
        };
        let expected_source = checks.expected_source_identity();
        verify_open_source_identity(file, expected_source, source_path)?;
        verify_open_source_size(file, metadata.size, source_path)?;
        let captured = source
            .read_open_file_preservation(source_path, file, options.preservation_request)
            .await?;
        if options.preservation_request.xattrs {
            preservation.xattrs = captured.xattrs;
        }
        if options.preservation_request.acl {
            preservation.acl = captured.acl;
        }
    }

    let expected_source = checks.expected_source_identity();
    let make_receipt = move |verification: &VerificationStatus| {
        PublishedDestinationReceipt::for_file(
            dest_relative.clone(),
            expected_source,
            verification,
            preservation_requested,
            !pending_finalization,
        )
    };

    if native_strategy {
        if let Some(mut source_file) = source_file.take() {
            if let Some(before_stage) = before_stage.take() {
                before_stage().await?;
            }

            let sparse_candidate = if source_caps.sparse && dest_caps.sparse {
                match native_file_is_sparse(&source_file) {
                    Ok(true) => source.native_file_has_sparse_holes(&source_file).await?,
                    Ok(false) => false,
                    Err(error) => {
                        tracing::debug!("native sparse candidate check failed: {error}");
                        false
                    }
                }
            } else {
                false
            };
            if sparse_candidate {
                let (returned_source, result) = native_sparse_staged_copy(
                    source_file,
                    source_path,
                    dest,
                    dest_path,
                    NativeStagedCopyPolicy {
                        metadata: metadata.clone(),
                        expected_destination,
                        verify: options.verify,
                        checks: checks.clone(),
                        preservation: preservation.clone(),
                    },
                )
                .await?;
                source_file = returned_source;
                if let Some(result) = result {
                    let receipt = make_receipt(&result.verification);
                    return Ok(TransferResult {
                        bytes_written: result.bytes_written,
                        strategy: TransferStrategy::NativeSparseCopy,
                        verification: result.verification,
                        receipt,
                    });
                }
            }

            #[cfg(target_os = "linux")]
            if options.update
                && metadata.size >= 16 * 1024 * 1024
                && source_caps.random_read
                && dest_caps.random_write
                && dest_caps.reflink
                && dest_native
                    .as_deref()
                    .is_some_and(crate::fs_util::supports_cow_reflinks)
            {
                let (returned_source, result) = reflink_patch(
                    source_file,
                    dest,
                    dest_path,
                    NativeReflinkPatchPolicy {
                        source_path: source_path.to_path_buf(),
                        metadata: metadata.clone(),
                        expected_destination,
                        verify: options.verify,
                        checks: checks.clone(),
                        preservation: preservation.clone(),
                    },
                )
                .await?;
                source_file = returned_source;
                if let Some(result) = result {
                    let receipt = make_receipt(&result.verification);
                    return Ok(TransferResult {
                        bytes_written: result.bytes_written,
                        strategy: TransferStrategy::ReflinkPatch,
                        verification: result.verification,
                        receipt,
                    });
                }
            }

            let result = native_whole_staged_copy(
                source_file,
                source_path,
                dest,
                dest_path,
                NativeStagedCopyPolicy {
                    metadata,
                    expected_destination,
                    verify: options.verify,
                    checks,
                    preservation,
                },
            )
            .await?;
            let receipt = make_receipt(&result.verification);
            return Ok(TransferResult {
                bytes_written: result.bytes_written,
                strategy: TransferStrategy::NativeWholeCopy,
                verification: result.verification,
                receipt,
            });
        }
    }

    if source_caps.streaming_read && dest_caps.staged_write {
        if options.verify && !dest_caps.staged_verify {
            return Err(SyncError::Config(format!(
                "{:?} destination cannot verify staged bytes before commit",
                dest.endpoint_type()
            )));
        }

        let mut identity_file = None;
        let reader = if let Some(file) = source_file.take() {
            identity_file = Some(file.try_clone()?);
            Box::pin(tokio::fs::File::from_std(file)) as crate::endpoint::BoxReader
        } else {
            source.open_reader(source_path).await?
        };
        let expected_identity = checks.expected_source_identity();
        let expected_size = metadata.size;
        let pre_commit = || {
            checks.verify_source(CheckPoint::Commit)?;
            if let Some(file) = identity_file.as_ref() {
                verify_open_source_identity(file, expected_identity, source_path)?;
                verify_open_source_size(file, expected_size, source_path)?;
            }
            Ok(())
        };
        if let Some(before_stage) = before_stage.take() {
            before_stage().await?;
        }
        let result = copy_file_streaming_from_reader(
            reader,
            source_path,
            dest,
            dest_path,
            &StreamCopyPolicy {
                metadata: &metadata,
                verify: options.verify,
                expected_destination,
                rate_limiter: options.rate_limiter.as_ref(),
                preservation: &preservation,
                pre_commit: Some(&pre_commit),
            },
        )
        .await?;
        let receipt = make_receipt(&result.verification);
        return Ok(TransferResult {
            bytes_written: result.bytes_written,
            strategy: TransferStrategy::Streaming,
            verification: result.verification,
            receipt,
        });
    }

    Err(SyncError::Config(format!(
        "no safe transfer strategy for {:?} -> {:?}",
        source.endpoint_type(),
        dest.endpoint_type()
    )))
}

async fn native_whole_staged_copy(
    mut source_file: std::fs::File,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    policy: NativeStagedCopyPolicy,
) -> Result<NativeTransferResult> {
    let expected_hash = if policy.verify {
        let (source, hash) = tokio::task::spawn_blocking(move || {
            let hash = hash_native_open_file(&mut source_file)?;
            Ok::<_, SyncError>((source_file, hash))
        })
        .await
        .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))??;
        source_file = source;
        Some(hash)
    } else {
        None
    };
    let mut writer = dest
        .begin_write(dest_path, policy.expected_destination)
        .await?;
    let (source_file, bytes_written) = match writer.copy_from_native_file(source_file).await {
        Ok(copied) => copied,
        Err(operation) => {
            crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
            return Err(operation);
        }
    };
    if bytes_written != policy.metadata.size {
        let operation = SyncError::SourceChanged {
            path: source_path.to_path_buf(),
        };
        crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
        return Err(operation);
    }

    let expected_source = policy.checks.expected_source_identity();
    let pre_commit = || {
        policy.checks.verify_source(CheckPoint::Commit)?;
        verify_open_source_identity(&source_file, expected_source, source_path)?;
        verify_open_source_size(&source_file, policy.metadata.size, source_path)
    };
    let verification = crate::endpoint::io::finalize_staged_writer(
        writer,
        &policy.metadata,
        &policy.preservation,
        expected_hash,
        Some(&pre_commit),
    )
    .await?;

    Ok(NativeTransferResult {
        bytes_written,
        verification,
    })
}

#[cfg(target_os = "linux")]
async fn reflink_patch(
    source_file: std::fs::File,
    dest: &dyn Endpoint,
    dest_path: &Path,
    policy: NativeReflinkPatchPolicy,
) -> Result<(std::fs::File, Option<NativeTransferResult>)> {
    let NativeReflinkPatchPolicy {
        source_path,
        metadata,
        expected_destination,
        verify,
        checks,
        preservation,
    } = policy;
    let ExpectedDestination::Unchanged(expected_destination_identity) = expected_destination else {
        return Ok((source_file, None));
    };
    let Some(destination_basis) = (match dest.open_native_file(dest_path).await {
        Ok(file) => file,
        Err(SyncError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SyncError::DestinationChanged {
                path: dest_path.to_path_buf(),
            });
        }
        Err(error) => {
            tracing::debug!("could not open reflink basis, falling back to whole copy: {error}");
            return Ok((source_file, None));
        }
    }) else {
        return Ok((source_file, None));
    };
    verify_open_destination_identity(&destination_basis, expected_destination_identity, dest_path)?;
    if destination_basis.metadata()?.nlink() > 1 {
        return Ok((source_file, None));
    }

    // Reflink patching trades an extra destination read for fewer physical
    // writes. Keep it conservative until the benchmark suite tunes this.
    let (source_file, destination_basis, ratio) = tokio::task::spawn_blocking(move || {
        let mut source_file = source_file;
        let mut destination_basis = destination_basis;
        let ratio = sy::transfer::ratio::estimate_change_ratio_files(
            &mut source_file,
            &mut destination_basis,
            TRANSFER_BUFFER_SIZE,
            Some(16),
            Some(0.25),
        );
        (source_file, destination_basis, ratio)
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
    let ratio = match ratio {
        Ok(ratio) if ratio.use_delta => ratio,
        Ok(_) => return Ok((source_file, None)),
        Err(error) => {
            tracing::debug!("reflink change sampling failed: {error}");
            return Ok((source_file, None));
        }
    };
    tracing::debug!(
        changed = %ratio.change_ratio_percent(),
        "using reflink patch strategy"
    );

    let (source_file, expected_hash) = if verify {
        tokio::task::spawn_blocking(move || {
            let mut source_file = source_file;
            let hash = hash_native_open_file(&mut source_file)?;
            Ok::<_, std::io::Error>((source_file, Some(hash)))
        })
        .await
        .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
        .map_err(SyncError::Io)?
    } else {
        (source_file, None)
    };

    let mut writer = dest.begin_write(dest_path, expected_destination).await?;
    let (source_file, _destination_basis, bytes_written) = match writer
        .reflink_patch_from_native_files(source_file, destination_basis, metadata.size)
        .await
    {
        Ok(result) => result,
        Err(SyncError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            let operation = SyncError::SourceChanged {
                path: source_path.to_path_buf(),
            };
            crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
            return Err(operation);
        }
        Err(operation) => {
            crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
            return Err(operation);
        }
    };
    let Some(bytes_written) = bytes_written else {
        writer.abort().await?;
        return Ok((source_file, None));
    };

    let expected_source = checks.expected_source_identity();
    let pre_commit = || {
        checks.verify_source(CheckPoint::Commit)?;
        verify_open_source_identity(&source_file, expected_source, &source_path)?;
        verify_open_source_size(&source_file, metadata.size, &source_path)
    };
    let verification = crate::endpoint::io::finalize_staged_writer(
        writer,
        &metadata,
        &preservation,
        expected_hash,
        Some(&pre_commit),
    )
    .await?;

    Ok((
        source_file,
        Some(NativeTransferResult {
            bytes_written,
            verification,
        }),
    ))
}

#[cfg(unix)]
fn native_file_is_sparse(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    let allocated = metadata.blocks().saturating_mul(512);
    Ok(metadata.len() > 4096 && allocated < metadata.len().saturating_sub(4096))
}

#[cfg(not(unix))]
fn native_file_is_sparse(_file: &std::fs::File) -> std::io::Result<bool> {
    Ok(false)
}

async fn native_sparse_staged_copy(
    mut source_file: std::fs::File,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    policy: NativeStagedCopyPolicy,
) -> Result<(std::fs::File, Option<NativeTransferResult>)> {
    let expected_hash = if policy.verify {
        let (source, hash) = tokio::task::spawn_blocking(move || {
            let hash = hash_native_open_file(&mut source_file)?;
            Ok::<_, SyncError>((source_file, hash))
        })
        .await
        .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))??;
        source_file = source;
        Some(hash)
    } else {
        None
    };

    let mut writer = dest
        .begin_write(dest_path, policy.expected_destination)
        .await?;
    let (source_file, bytes_written) = match writer.copy_sparse_from_native_file(source_file).await
    {
        Ok(copied) => copied,
        Err(SyncError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            let operation = SyncError::SourceChanged {
                path: source_path.to_path_buf(),
            };
            crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
            return Err(operation);
        }
        Err(operation) => {
            crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
            return Err(operation);
        }
    };
    let Some(bytes_written) = bytes_written else {
        writer.abort().await?;
        return Ok((source_file, None));
    };

    let source_size = match source_file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            let operation = SyncError::Io(error);
            crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
            return Err(operation);
        }
    };
    if source_size != policy.metadata.size {
        let operation = SyncError::SourceChanged {
            path: source_path.to_path_buf(),
        };
        crate::endpoint::io::abort_staged_writer(writer, &operation).await?;
        return Err(operation);
    }
    let expected_source = policy.checks.expected_source_identity();
    let pre_commit = || {
        policy.checks.verify_source(CheckPoint::Commit)?;
        verify_open_source_identity(&source_file, expected_source, source_path)?;
        verify_open_source_size(&source_file, policy.metadata.size, source_path)
    };
    let verification = crate::endpoint::io::finalize_staged_writer(
        writer,
        &policy.metadata,
        &policy.preservation,
        expected_hash,
        Some(&pre_commit),
    )
    .await?;

    Ok((
        source_file,
        Some(NativeTransferResult {
            bytes_written,
            verification,
        }),
    ))
}

fn verify_native_staging(
    source: &Path,
    staged: &mut std::fs::File,
    verify: bool,
) -> std::io::Result<VerificationStatus> {
    if !verify {
        return Ok(VerificationStatus::NotRequested);
    }

    let expected = hash_native_file(source)?;
    let actual = hash_native_open_file(staged)?;
    if expected == actual {
        Ok(VerificationStatus::Verified)
    } else {
        Ok(VerificationStatus::Failed { expected, actual })
    }
}

fn hash_native_file(path: &Path) -> std::io::Result<blake3::Hash> {
    let mut file = std::fs::File::open(path)?;
    hash_native_open_file(&mut file)
}

fn hash_native_open_file(file: &mut std::fs::File) -> std::io::Result<blake3::Hash> {
    use std::io::{Read, Seek, SeekFrom};

    file.seek(SeekFrom::Start(0))?;
    let mut buffer = vec![0_u8; TRANSFER_BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    file.seek(SeekFrom::End(0))?;
    Ok(hasher.finalize())
}

fn apply_metadata(path: &Path, metadata: &FileMetadata) -> std::io::Result<()> {
    filetime::set_file_mtime(
        path,
        filetime::FileTime::from_system_time(metadata.modified),
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(metadata.mode))?;
    }

    Ok(())
}

#[cfg(unix)]
fn strip_xattrs(path: &Path) -> std::io::Result<()> {
    let attributes = match xattr::list(path) {
        Ok(attributes) => attributes,
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return Ok(()),
        Err(error) => return Err(error),
    };

    for attribute in attributes {
        match xattr::remove(path, &attribute) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(libc::EPERM)
                    || error.raw_os_error() == Some(libc::EACCES) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn strip_xattrs(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::endpoint::local::LocalEndpoint;
    use crate::endpoint::local_identity::metadata_identity;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use sy::engine::domain::EntryKind;

    const NAME: &str = "file";

    fn identity_of(path: &Path) -> EntryIdentity {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        metadata_identity(&metadata, EntryKind::File).unwrap()
    }

    struct Fixture {
        source_root: tempfile::TempDir,
        dest_root: tempfile::TempDir,
    }

    #[cfg(feature = "acl")]
    struct ReplacePathDuringCapture {
        inner: LocalEndpoint,
        root: PathBuf,
        replacement_acl: String,
    }

    #[cfg(feature = "acl")]
    #[async_trait::async_trait]
    impl Endpoint for ReplacePathDuringCapture {
        fn endpoint_type(&self) -> crate::endpoint::EndpointType {
            self.inner.endpoint_type()
        }

        fn capabilities(&self) -> &crate::endpoint::Capabilities {
            self.inner.capabilities()
        }

        fn root(&self) -> &Path {
            self.inner.root()
        }

        fn native_path(&self, path: &Path) -> Option<PathBuf> {
            self.inner.native_path(path)
        }

        async fn open_native_file(&self, path: &Path) -> Result<Option<std::fs::File>> {
            self.inner.open_native_file(path).await
        }

        async fn read_open_file_preservation(
            &self,
            path: &Path,
            file: &std::fs::File,
            request: PreservationRequest,
        ) -> Result<Preservation> {
            let visible = self.root.join(path);
            let displaced = self.root.join("held-source");
            std::fs::rename(&visible, &displaced)?;
            std::fs::write(&visible, b"replacement bytes")?;
            xattr::set(&visible, "user.sy-transfer", b"replacement")?;
            if request.acl {
                self.inner.write_acl(path, &self.replacement_acl).await?;
            }
            self.inner
                .read_open_file_preservation(path, file, request)
                .await
        }

        async fn exists(&self, path: &Path) -> Result<bool> {
            self.inner.exists(path).await
        }

        async fn metadata(&self, path: &Path) -> Result<FileMetadata> {
            self.inner.metadata(path).await
        }

        async fn remove(&self, path: &Path, recursive: bool) -> Result<()> {
            self.inner.remove(path, recursive).await
        }

        async fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.inner.create_dir_all(path).await
        }

        async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
            self.inner.create_symlink(target, dest).await
        }

        async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
            self.inner.create_hardlink(source, dest).await
        }
    }

    /// Model a streaming-only endpoint, with or without a locally observable
    /// path. A path alone cannot bind the reader to a scanned identity.
    struct ReaderOnlyEndpoint {
        inner: LocalEndpoint,
        expose_native_path: bool,
    }

    #[async_trait::async_trait]
    impl Endpoint for ReaderOnlyEndpoint {
        fn endpoint_type(&self) -> crate::endpoint::EndpointType {
            self.inner.endpoint_type()
        }

        fn capabilities(&self) -> &crate::endpoint::Capabilities {
            self.inner.capabilities()
        }

        fn root(&self) -> &Path {
            self.inner.root()
        }

        fn native_path(&self, path: &Path) -> Option<PathBuf> {
            self.expose_native_path
                .then(|| self.inner.root().join(path))
        }

        async fn open_reader(&self, path: &Path) -> Result<crate::endpoint::BoxReader> {
            self.inner.open_reader(path).await
        }

        async fn exists(&self, path: &Path) -> Result<bool> {
            self.inner.exists(path).await
        }

        async fn metadata(&self, path: &Path) -> Result<FileMetadata> {
            self.inner.metadata(path).await
        }

        async fn remove(&self, path: &Path, recursive: bool) -> Result<()> {
            self.inner.remove(path, recursive).await
        }

        async fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.inner.create_dir_all(path).await
        }

        async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
            self.inner.create_symlink(target, dest).await
        }

        async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
            self.inner.create_hardlink(source, dest).await
        }
    }

    /// Replace a real ancestor only while opening the byte source. Restoring
    /// the directory leaves the original file's identity untouched, so path
    /// checks before and after the transfer cannot detect the wrong reader.
    struct SwapAncestorAtOpen {
        inner: LocalEndpoint,
    }

    impl SwapAncestorAtOpen {
        async fn open_swapped_file(&self, path: &Path) -> Result<std::fs::File> {
            let root = self.inner.root();
            let visible = root.join("nested");
            let held = root.join("held");
            let replacement = root.join("replacement");
            std::fs::rename(&visible, &held)?;
            std::fs::rename(&replacement, &visible)?;
            let opened = self.inner.open_native_file(path).await;
            std::fs::rename(&visible, &replacement)?;
            std::fs::rename(&held, &visible)?;
            opened?.ok_or_else(|| SyncError::Config("test requires native source handles".into()))
        }
    }

    #[async_trait::async_trait]
    impl Endpoint for SwapAncestorAtOpen {
        fn endpoint_type(&self) -> crate::endpoint::EndpointType {
            self.inner.endpoint_type()
        }

        fn capabilities(&self) -> &crate::endpoint::Capabilities {
            self.inner.capabilities()
        }

        fn root(&self) -> &Path {
            self.inner.root()
        }

        fn native_path(&self, path: &Path) -> Option<PathBuf> {
            self.inner.native_path(path)
        }

        async fn open_native_file(&self, path: &Path) -> Result<Option<std::fs::File>> {
            self.open_swapped_file(path).await.map(Some)
        }

        async fn open_reader(&self, path: &Path) -> Result<crate::endpoint::BoxReader> {
            let file = self.open_swapped_file(path).await?;
            Ok(Box::pin(tokio::fs::File::from_std(file)))
        }

        async fn exists(&self, path: &Path) -> Result<bool> {
            self.inner.exists(path).await
        }

        async fn metadata(&self, path: &Path) -> Result<FileMetadata> {
            self.inner.metadata(path).await
        }

        async fn remove(&self, path: &Path, recursive: bool) -> Result<()> {
            self.inner.remove(path, recursive).await
        }

        async fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.inner.create_dir_all(path).await
        }

        async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
            self.inner.create_symlink(target, dest).await
        }

        async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
            self.inner.create_hardlink(source, dest).await
        }
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                source_root: tempfile::tempdir().unwrap(),
                dest_root: tempfile::tempdir().unwrap(),
            }
        }

        fn source_file(&self) -> PathBuf {
            self.source_root.path().join(NAME)
        }

        fn dest_file(&self) -> PathBuf {
            self.dest_root.path().join(NAME)
        }

        fn source_endpoint(&self) -> LocalEndpoint {
            LocalEndpoint::new(self.source_root.path().to_path_buf())
        }

        fn dest_endpoint(&self) -> LocalEndpoint {
            LocalEndpoint::new(self.dest_root.path().to_path_buf())
        }

        /// Visible entries in the destination root; staging must be gone after
        /// every abort.
        fn dest_entries(&self) -> usize {
            std::fs::read_dir(self.dest_root.path()).unwrap().count()
        }
    }

    fn update_identity(fixture: &Fixture) -> TransferIdentity {
        TransferIdentity {
            source: SourceExpectation::Scanned(identity_of(&fixture.source_file())),
            destination: ExpectedDestination::Unchanged(identity_of(&fixture.dest_file())),
        }
    }

    fn create_identity(fixture: &Fixture) -> TransferIdentity {
        TransferIdentity {
            source: SourceExpectation::Scanned(identity_of(&fixture.source_file())),
            destination: ExpectedDestination::Absent,
        }
    }

    fn options(identity: TransferIdentity, update: bool) -> TransferOptions {
        TransferOptions {
            update,
            verify: false,
            follow_symlinks: false,
            rate_limiter: None,
            preservation: Preservation::default(),
            preservation_request: PreservationRequest::default(),
            pending_finalization: false,
            identity,
            metadata: None,
        }
    }

    fn streaming_options(identity: TransferIdentity, update: bool) -> TransferOptions {
        TransferOptions {
            rate_limiter: Some(Arc::new(std::sync::Mutex::new(
                crate::sync::ratelimit::RateLimiter::new(1_u64 << 40),
            ))),
            ..options(identity, update)
        }
    }

    fn hook(mutate: impl Fn() + Send + Sync + 'static) -> RaceHook {
        Arc::new(mutate)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn requested_xattrs_are_captured_from_the_byte_source() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        let source = fixture.source_endpoint();
        let destination = fixture.dest_endpoint();
        source
            .write_xattrs(
                Path::new(NAME),
                &[(
                    std::ffi::OsString::from("user.sy-transfer"),
                    b"bound".to_vec(),
                )],
            )
            .await
            .unwrap();
        let mut transfer_options = options(create_identity(&fixture), false);
        transfer_options.preservation_request.xattrs = true;

        transfer_file(
            &source,
            Path::new(NAME),
            &destination,
            Path::new(NAME),
            transfer_options,
        )
        .await
        .unwrap();

        assert!(destination
            .read_xattrs(Path::new(NAME))
            .await
            .unwrap()
            .contains(&(
                std::ffi::OsString::from("user.sy-transfer"),
                b"bound".to_vec()
            )));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_requested_xattrs_stay_bound_to_the_open_source_file() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        let source = fixture.source_endpoint();
        let destination = fixture.dest_endpoint();
        source
            .write_xattrs(
                Path::new(NAME),
                &[(
                    std::ffi::OsString::from("user.sy-stream"),
                    b"bound".to_vec(),
                )],
            )
            .await
            .unwrap();
        let mut transfer_options = streaming_options(create_identity(&fixture), false);
        transfer_options.preservation_request.xattrs = true;

        let result = transfer_file(
            &source,
            Path::new(NAME),
            &destination,
            Path::new(NAME),
            transfer_options,
        )
        .await
        .unwrap();

        assert_eq!(result.strategy, TransferStrategy::Streaming);
        assert!(destination
            .read_xattrs(Path::new(NAME))
            .await
            .unwrap()
            .contains(&(
                std::ffi::OsString::from("user.sy-stream"),
                b"bound".to_vec()
            )));
    }

    #[cfg(feature = "acl")]
    async fn assert_held_preservation_after_source_path_replacement(rate_limited: bool) {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"original bytes").unwrap();
        let source_endpoint = fixture.source_endpoint();
        source_endpoint
            .write_xattrs(
                Path::new(NAME),
                &[(
                    std::ffi::OsString::from("user.sy-transfer"),
                    b"original".to_vec(),
                )],
            )
            .await
            .unwrap();

        let base_acl = exacl::getfacl(fixture.source_file(), None).unwrap();
        let base_acl_text = exacl::to_string(&base_acl).unwrap();
        // SAFETY: `getuid` has no pointer arguments or other preconditions.
        let uid = unsafe { libc::getuid() };
        let mut original_acl = base_acl;
        original_acl.push(exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        exacl::setfacl(&[&fixture.source_file()], &original_acl, None).unwrap();
        let original_acl_text =
            exacl::to_string(&exacl::getfacl(fixture.source_file(), None).unwrap())
                .unwrap()
                .trim()
                .to_string();

        let source = ReplacePathDuringCapture {
            inner: source_endpoint,
            root: fixture.source_root.path().to_path_buf(),
            replacement_acl: base_acl_text,
        };
        let destination = fixture.dest_endpoint();
        let identity = TransferIdentity {
            source: SourceExpectation::Unverified,
            destination: ExpectedDestination::Absent,
        };
        let mut transfer_options = if rate_limited {
            streaming_options(identity, false)
        } else {
            options(identity, false)
        };
        transfer_options.preservation_request = PreservationRequest {
            xattrs: true,
            acl: true,
        };

        let source_during_staging = fixture.source_file();
        let result = transfer_file_with_before_stage(
            &source,
            Path::new(NAME),
            &destination,
            Path::new(NAME),
            transfer_options,
            move || async move {
                assert_eq!(
                    std::fs::read(source_during_staging).unwrap(),
                    b"replacement bytes"
                );
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(fixture.dest_file()).unwrap(),
            b"original bytes"
        );
        assert!(destination
            .read_xattrs(Path::new(NAME))
            .await
            .unwrap()
            .contains(&(
                std::ffi::OsString::from("user.sy-transfer"),
                b"original".to_vec()
            )));
        assert_eq!(
            destination
                .read_acl(Path::new(NAME))
                .await
                .unwrap()
                .unwrap()
                .trim(),
            original_acl_text
        );
        if rate_limited {
            assert_eq!(result.strategy, TransferStrategy::Streaming);
        } else {
            assert_eq!(result.strategy, TransferStrategy::NativeWholeCopy);
        }

        std::fs::remove_file(fixture.source_file()).unwrap();
        std::fs::rename(
            fixture.source_root.path().join("held-source"),
            fixture.source_file(),
        )
        .unwrap();
    }

    #[cfg(feature = "acl")]
    #[tokio::test]
    async fn native_and_streaming_preservation_stays_with_held_byte_source() {
        assert_held_preservation_after_source_path_replacement(false).await;
        assert_held_preservation_after_source_path_replacement(true).await;
    }

    #[tokio::test]
    async fn matching_expectations_commit() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);

        let result = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
        )
        .await
        .unwrap();

        assert_eq!(result.strategy, TransferStrategy::NativeWholeCopy);
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"NEW!");
    }

    #[tokio::test]
    async fn native_whole_copy_refuses_parent_moved_outside_root() {
        let fixture = Fixture::new();
        let source_parent = fixture.source_root.path().join("nested");
        let dest_parent = fixture.dest_root.path().join("nested");
        let moved_parent = fixture.dest_root.path().join("moved");
        std::fs::create_dir(&source_parent).unwrap();
        std::fs::create_dir(&dest_parent).unwrap();
        std::fs::write(source_parent.join(NAME), b"NEW!").unwrap();
        std::fs::write(dest_parent.join(NAME), b"OLD!").unwrap();
        let identity = TransferIdentity {
            source: SourceExpectation::Scanned(identity_of(&source_parent.join(NAME))),
            destination: ExpectedDestination::Unchanged(identity_of(&dest_parent.join(NAME))),
        };
        let racing_parent = dest_parent.clone();
        let racing_moved = moved_parent.clone();
        let racing = hook(move || {
            std::fs::rename(&racing_parent, &racing_moved).unwrap();
            std::fs::create_dir(&racing_parent).unwrap();
        });

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new("nested/file"),
            &fixture.dest_endpoint(),
            Path::new("nested/file"),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::DestinationChanged { .. }));
        assert_eq!(std::fs::read(moved_parent.join(NAME)).unwrap(), b"OLD!");
        assert!(!dest_parent.join(NAME).exists());
        assert_eq!(std::fs::read_dir(&dest_parent).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_links_streaming_uses_target_metadata() {
        let fixture = Fixture::new();
        let target = fixture.source_root.path().join("target");
        std::fs::write(&target, b"target bytes").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let target_time = filetime::FileTime::from_unix_time(123, 456_000_000);
        filetime::set_file_mtime(&target, target_time).unwrap();
        std::os::unix::fs::symlink("target", fixture.source_file()).unwrap();
        let identity = TransferIdentity {
            source: SourceExpectation::Scanned(identity_of(&target)),
            destination: ExpectedDestination::Absent,
        };
        let mut options = options(identity, false);
        options.follow_symlinks = true;

        let result = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options,
        )
        .await
        .unwrap();

        assert_eq!(result.strategy, TransferStrategy::Streaming);
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"target bytes");
        let metadata = std::fs::metadata(fixture.dest_file()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(metadata.modified().unwrap(), target_time.into());
    }

    #[tokio::test]
    async fn streaming_matching_expectations_commit() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);

        let result = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            streaming_options(identity, true),
        )
        .await
        .unwrap();

        assert_eq!(result.strategy, TransferStrategy::Streaming);
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"NEW!");
    }

    #[tokio::test]
    async fn reader_only_endpoints_do_not_silently_downgrade_identity_guarantees() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let mut source = ReaderOnlyEndpoint {
            inner: fixture.source_endpoint(),
            expose_native_path: true,
        };
        let error = transfer_file(
            &source,
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            streaming_options(update_identity(&fixture), true),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SyncError::Config(_)));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
        assert_eq!(fixture.dest_entries(), 1);

        source.expose_native_path = false;
        let identity = TransferIdentity {
            source: SourceExpectation::Unverified,
            destination: ExpectedDestination::Unchanged(identity_of(&fixture.dest_file())),
        };
        let result = transfer_file(
            &source,
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            streaming_options(identity, true),
        )
        .await
        .unwrap();
        assert_eq!(result.strategy, TransferStrategy::Streaming);
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"NEW!");
        assert_eq!(std::fs::read(fixture.source_file()).unwrap(), b"NEW!");
    }

    #[tokio::test]
    async fn streaming_rejects_swapped_source_reader_even_after_ancestor_is_restored() {
        let fixture = Fixture::new();
        let source_parent = fixture.source_root.path().join("nested");
        let replacement_parent = fixture.source_root.path().join("replacement");
        std::fs::create_dir(&source_parent).unwrap();
        std::fs::create_dir(&replacement_parent).unwrap();
        std::fs::write(source_parent.join(NAME), b"NEW!").unwrap();
        std::fs::write(replacement_parent.join(NAME), b"FAKE").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let scanned_source = identity_of(&source_parent.join(NAME));
        let identity = TransferIdentity {
            source: SourceExpectation::Scanned(scanned_source),
            destination: ExpectedDestination::Unchanged(identity_of(&fixture.dest_file())),
        };
        let source = SwapAncestorAtOpen {
            inner: fixture.source_endpoint(),
        };
        // No xattrs/ACLs or verification: only --bwlimit forces streaming.
        let error = transfer_file(
            &source,
            Path::new("nested/file"),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            streaming_options(identity, true),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(identity_of(&source_parent.join(NAME)), scanned_source);
        assert_eq!(std::fs::read(source_parent.join(NAME)).unwrap(), b"NEW!");
        assert_eq!(
            std::fs::read(replacement_parent.join(NAME)).unwrap(),
            b"FAKE"
        );
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
        assert_eq!(fixture.dest_entries(), 1);
        assert_eq!(
            std::fs::read_dir(fixture.source_root.path())
                .unwrap()
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn requested_file_metadata_is_staged_before_commit() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        std::fs::set_permissions(
            fixture.source_file(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::set_permissions(fixture.dest_file(), std::fs::Permissions::from_mode(0o600))
            .unwrap();

        let old_time = timestamp_to_system_time(Timestamp::new(50, 0).unwrap()).unwrap();
        let requested_time =
            timestamp_to_system_time(Timestamp::new(123, 456_789_000).unwrap()).unwrap();
        filetime::set_file_mtime(
            fixture.dest_file(),
            filetime::FileTime::from_system_time(old_time),
        )
        .unwrap();
        let identity = update_identity(&fixture);
        let dest_file = fixture.dest_file();
        let hook = hook(move || {
            assert_eq!(std::fs::read(&dest_file).unwrap(), b"OLD!");
            let metadata = std::fs::metadata(&dest_file).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
            assert_eq!(metadata.modified().unwrap(), old_time);
        });
        let mut transfer_options = options(identity, true);
        transfer_options.verify = true;
        transfer_options.metadata = Some(TransferMetadata {
            unix_mode: Some(0o200),
            modified: Some(Timestamp::new(123, 456_789_000).unwrap()),
        });

        transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            transfer_options,
            hook,
        )
        .await
        .unwrap();

        let metadata = std::fs::metadata(fixture.dest_file()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o200);
        assert_eq!(metadata.modified().unwrap(), requested_time);
        std::fs::set_permissions(fixture.dest_file(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"NEW!");
    }

    #[test]
    fn timestamp_conversion_handles_pre_epoch_values() {
        let one_nanosecond_before_epoch = std::time::UNIX_EPOCH
            .checked_sub(std::time::Duration::from_nanos(1))
            .unwrap();
        assert_eq!(
            timestamp_to_system_time(Timestamp::new(-1, 999_999_999).unwrap()),
            Some(one_nanosecond_before_epoch)
        );
    }

    #[tokio::test]
    async fn scan_to_open_race_detected_without_hook() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let decoy = fixture.source_root.path().join("decoy");
        std::fs::write(&decoy, b"decoy content").unwrap();

        // The scanned identity belongs to another entry: the source changed
        // between scan and transfer start.
        let identity = TransferIdentity {
            source: SourceExpectation::Scanned(identity_of(&decoy)),
            destination: ExpectedDestination::Unchanged(identity_of(&fixture.dest_file())),
        };

        let error = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
    }

    #[tokio::test]
    async fn source_size_change_before_commit_aborts_and_preserves_destination() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let source_file = fixture.source_file();
        let racing = hook(move || std::fs::write(&source_file, b"RACED").unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
        assert_eq!(fixture.dest_entries(), 1);
    }

    #[tokio::test]
    async fn source_same_size_rewrite_detected() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let source_file = fixture.source_file();
        // Same length, different content and mode: only identity fields catch
        // this, not size comparison.
        let racing = hook(move || {
            std::fs::write(&source_file, b"XXXX").unwrap();
            std::fs::set_permissions(&source_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        });

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
    }

    #[tokio::test]
    async fn source_replacement_before_commit_aborts() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let source_file = fixture.source_file();
        let replacement = fixture.source_root.path().join("replacement");
        let racing = hook(move || {
            std::fs::write(&replacement, b"ZZZZ").unwrap();
            std::fs::rename(&replacement, &source_file).unwrap();
        });

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
    }

    #[tokio::test]
    async fn source_removed_before_commit_aborts() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let source_file = fixture.source_file();
        let racing = hook(move || std::fs::remove_file(&source_file).unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
    }

    #[tokio::test]
    async fn source_root_rename_before_commit_aborts() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let old_root = fixture.source_root.path().to_path_buf();
        let moved_root = old_root.with_extension("moved");
        let racing = hook(move || std::fs::rename(&old_root, &moved_root).unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
    }

    #[tokio::test]
    async fn destination_appearing_during_create_aborts() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        let identity = create_identity(&fixture);
        let dest_file = fixture.dest_file();
        let racing = hook(move || std::fs::write(&dest_file, b"KEEP").unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, false),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::DestinationChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"KEEP");
        assert_eq!(fixture.dest_entries(), 1);
    }

    #[tokio::test]
    async fn destination_appearing_before_transfer_start_aborts() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        let identity = create_identity(&fixture);
        // The raced-in destination exists before the transfer even begins.
        std::fs::write(fixture.dest_file(), b"KEEP").unwrap();

        let error = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, false),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::DestinationChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"KEEP");
    }

    #[tokio::test]
    async fn destination_edit_during_update_aborts_and_preserves_edit() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let dest_file = fixture.dest_file();
        let racing = hook(move || std::fs::write(&dest_file, b"OLD!XX").unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::DestinationChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!XX");
        assert_eq!(fixture.dest_entries(), 1);
    }

    #[tokio::test]
    async fn streaming_path_enforces_commit_checks() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let source_file = fixture.source_file();
        let racing = hook(move || std::fs::write(&source_file, b"RACED").unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            streaming_options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
        assert_eq!(fixture.dest_entries(), 1);
    }

    #[tokio::test]
    async fn sparse_path_enforces_commit_checks() {
        let fixture = Fixture::new();
        // Sparse source: preallocated length with a small tail write.
        let source_file = fixture.source_file();
        let file = std::fs::File::create(&source_file).unwrap();
        file.set_len(1024 * 1024).unwrap();
        drop(file);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&source_file)
            .unwrap();
        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(1024 * 1024 - 4)).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);

        let identity = create_identity(&fixture);
        let dest_file = fixture.dest_file();
        let racing = hook(move || std::fs::write(&dest_file, b"KEEP").unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, false),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::DestinationChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"KEEP");
        assert_eq!(fixture.dest_entries(), 1);
    }

    #[tokio::test]
    async fn sparse_staged_copy_preserves_logical_bytes_and_verifies() {
        let fixture = Fixture::new();
        let source_path = fixture.source_file();
        let size = 1024 * 1024;
        let mut file = std::fs::File::create(&source_path).unwrap();
        file.set_len(size).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(128 * 1024)).unwrap();
        file.write_all(b"middle").unwrap();
        file.seek(SeekFrom::Start(size - 4)).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);

        let mut expected = vec![0; size as usize];
        expected[128 * 1024..128 * 1024 + 6].copy_from_slice(b"middle");
        expected[size as usize - 4..].copy_from_slice(b"tail");
        let result = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            TransferOptions {
                verify: true,
                ..options(create_identity(&fixture), false)
            },
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), expected);
        assert_eq!(result.verification, VerificationStatus::Verified);
        assert!(matches!(
            result.strategy,
            TransferStrategy::NativeSparseCopy | TransferStrategy::NativeWholeCopy
        ));
        if result.strategy == TransferStrategy::NativeSparseCopy {
            assert!(result.bytes_written < size);
        }
    }

    #[tokio::test]
    async fn sparse_staged_copy_preserves_data_before_a_trailing_hole_without_verify() {
        let fixture = Fixture::new();
        let source_path = fixture.source_file();
        let size = 1024 * 1024;
        use std::io::Write;
        let mut file = std::fs::File::create(&source_path).unwrap();
        file.write_all(b"prefix-data").unwrap();
        file.set_len(size).unwrap();
        drop(file);
        let source_endpoint = fixture.source_endpoint();
        let source_handle = source_endpoint
            .open_native_file(Path::new(NAME))
            .await
            .unwrap()
            .unwrap();
        let expects_sparse = native_file_is_sparse(&source_handle).unwrap()
            && source_endpoint
                .native_file_has_sparse_holes(&source_handle)
                .await
                .unwrap();

        let result = transfer_file(
            &source_endpoint,
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(create_identity(&fixture), false),
        )
        .await
        .unwrap();

        let copied = std::fs::read(fixture.dest_file()).unwrap();
        assert_eq!(copied.len(), size as usize);
        assert_eq!(&copied[..b"prefix-data".len()], b"prefix-data");
        assert!(copied[b"prefix-data".len()..].iter().all(|byte| *byte == 0));
        if expects_sparse {
            assert_eq!(result.strategy, TransferStrategy::NativeSparseCopy);
            assert!(result.bytes_written < size);
        }
    }

    fn cow_fixture() -> Option<Fixture> {
        let fixture = Fixture::new();
        // Reflink needs a COW destination; skip where the filesystem lacks it.
        if crate::fs_util::supports_cow_reflinks(fixture.dest_root.path()) {
            Some(fixture)
        } else {
            None
        }
    }

    /// A large, mostly-unchanged update is eligible for reflink patching on COW
    /// filesystems; the strategy must still honor the commit checks.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reflink_patch_enforces_commit_checks() {
        let Some(fixture) = cow_fixture() else {
            return;
        };
        let size = 16 * 1024 * 1024 + 1;
        let mut content = vec![b'A'; size];
        content[0] = b'B';
        std::fs::write(fixture.source_file(), &content).unwrap();
        content[0] = b'A';
        std::fs::write(fixture.dest_file(), &content).unwrap();
        let identity = update_identity(&fixture);
        let source_file = fixture.source_file();
        let racing = hook(move || std::fs::write(&source_file, b"raced").unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap().len(), size);
        assert_eq!(fixture.dest_entries(), 1);
    }

    /// Pins that the reflink branch is actually exercised where COW exists.
    #[tokio::test]
    async fn reflink_patch_selected_on_cow_filesystems() {
        let Some(fixture) = cow_fixture() else {
            return;
        };
        let size = 16 * 1024 * 1024 + 1;
        let mut source_content = vec![b'A'; size];
        source_content[0] = b'B';
        std::fs::write(fixture.source_file(), &source_content).unwrap();
        let mut dest_content = source_content.clone();
        dest_content[0] = b'A';
        std::fs::write(fixture.dest_file(), &dest_content).unwrap();
        let identity = update_identity(&fixture);

        let result = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
        )
        .await
        .unwrap();

        #[cfg(target_os = "linux")]
        assert_eq!(result.strategy, TransferStrategy::ReflinkPatch);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(result.strategy, TransferStrategy::NativeWholeCopy);
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), source_content);
    }

    #[tokio::test]
    async fn snapshot_expectations_detect_mid_transfer_edits() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = TransferIdentity {
            source: SourceExpectation::SnapshotAtOpen,
            destination: ExpectedDestination::SnapshotAtOpen,
        };
        let source_file = fixture.source_file();
        let racing = hook(move || std::fs::write(&source_file, b"RACED").unwrap());

        let error = transfer_file_with_hook(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
            racing,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged { .. }));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
    }

    /// A preservation payload that cannot be applied aborts staging: the old
    /// destination survives and no temp file is left behind.
    #[tokio::test]
    async fn preservation_failure_aborts_and_preserves_destination() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        // Oversized attribute names fail in every setxattr implementation.
        let oversized = format!("user.{}", "x".repeat(300));
        let mut opts = options(identity, true);
        opts.preservation = Preservation {
            xattrs: Some(vec![(std::ffi::OsString::from(oversized), vec![1_u8])]),
            acl: None,
        };

        let error = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            opts,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::Io(_)));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
        assert_eq!(fixture.dest_entries(), 1);
    }

    /// Preservation is applied to staging before commit, so the committed
    /// destination already carries the source's attributes.
    #[tokio::test]
    async fn preservation_is_applied_before_commit() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        let identity = create_identity(&fixture);
        let mut opts = options(identity, false);
        opts.preservation = Preservation {
            xattrs: Some(vec![(
                std::ffi::OsString::from("user.sy_test"),
                b"v".to_vec(),
            )]),
            acl: None,
        };

        transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            opts,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"NEW!");
        let value = xattr::get(fixture.dest_file(), "user.sy_test").unwrap();
        assert_eq!(value.as_deref(), Some(&b"v"[..]));
    }

    /// Unparseable ACL text aborts staging rather than committing bare
    /// content with broken protection.
    #[cfg(all(unix, feature = "acl"))]
    #[tokio::test]
    async fn invalid_acl_preservation_aborts() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);
        let mut opts = options(identity, true);
        opts.preservation = Preservation {
            xattrs: None,
            acl: Some("not an acl entry".to_string()),
        };

        let error = transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            opts,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::Io(_)));
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"OLD!");
        assert_eq!(fixture.dest_entries(), 1);
    }

    #[tokio::test]
    async fn unverified_expectations_still_transfer() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();

        transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(TransferIdentity::unverified(), true),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"NEW!");
    }
}
