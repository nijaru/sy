//! Capability-driven file transfer selection for the v0.5 architecture.
//!
//! The common path prefers endpoint-native filesystem primitives when both
//! sides expose native paths. Generic endpoint pairs fall back to bounded
//! staged streaming. Local updates may use a reflink clone + patch when that
//! reduces physical writes; non-COW local files use a normal whole-file copy.

use crate::endpoint::io::{copy_file_streaming, VerificationStatus};
use crate::endpoint::{Endpoint, FileMetadata};
use crate::error::{Result, SyncError};
use crate::temp_file::TempFileGuard;
use std::path::{Path, PathBuf};
use sy::engine::domain::EntryIdentity;

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
}

#[derive(Debug, Clone, Copy)]
pub struct TransferResult {
    pub bytes_written: u64,
    pub strategy: TransferStrategy,
    pub verification: VerificationStatus,
}

#[derive(Debug, Clone, Copy)]
struct NativeTransferResult {
    bytes_written: u64,
    verification: VerificationStatus,
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

/// Destination state the commit must still observe.
#[derive(Debug, Clone, Copy)]
pub enum ExpectedDestination {
    /// A create: the destination path must remain absent through commit.
    Absent,
    /// An update: the destination must still carry the scanned identity, so a
    /// concurrent edit is never silently overwritten.
    Unchanged(EntryIdentity),
    /// No scan observation exists (single-file copy): capture the identity at
    /// transfer start and require it unchanged at commit.
    SnapshotAtOpen,
    /// No observation is available; destination race checks are skipped.
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

    /// Prove the scanned (or snapshot) state still holds.
    ///
    /// Runs at transfer start (closing the scan-to-open window) and again
    /// immediately before the visible commit (closing the read window).
    fn verify(&self, checkpoint: CheckPoint) -> Result<()> {
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
    transfer_file_inner(source, source_path, dest, dest_path, options, None).await
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
    )
    .await
}

async fn transfer_file_inner(
    source: &dyn Endpoint,
    source_path: &Path,
    dest: &dyn Endpoint,
    dest_path: &Path,
    options: TransferOptions,
    test_hook: Option<RaceHook>,
) -> Result<TransferResult> {
    let mut metadata = source.metadata(source_path).await?;
    if options.follow_symlinks && metadata.is_symlink {
        // The scan reported the target; resolve the link so the transfer
        // guards and size decisions describe the bytes that will be copied.
        metadata = source.metadata_following(source_path).await?;
    }
    if metadata.is_dir || metadata.is_symlink {
        return Err(SyncError::Config(format!(
            "file transfer requested for non-regular source {}",
            source_path.display()
        )));
    }

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
    // Close the scan-to-open window before any staging work begins.
    checks.verify(CheckPoint::Open)?;

    let native_pair = if options.rate_limiter.is_none() {
        (source_native, dest_native)
    } else {
        (None, None)
    };
    if let (Some(source_native), Some(dest_native)) = native_pair {
        if source_caps.sparse
            && dest_caps.sparse
            && native_file_is_sparse(&source_native).unwrap_or(false)
        {
            if let Some(result) = native_sparse_copy(
                source_native.clone(),
                dest_native.clone(),
                metadata.clone(),
                options.verify,
                checks.clone(),
            )
            .await?
            {
                return Ok(TransferResult {
                    bytes_written: result.bytes_written,
                    strategy: TransferStrategy::NativeSparseCopy,
                    verification: result.verification,
                });
            }
        }

        if options.update
            && metadata.size >= 16 * 1024 * 1024
            && source_caps.random_read
            && dest_caps.random_write
            && dest_caps.reflink
            && dest_caps.atomic_rename
            && crate::fs_util::supports_cow_reflinks(&dest_native)
            && !crate::fs_util::has_hard_links(&dest_native)
        {
            if let Some(result) = reflink_patch(
                source_native.clone(),
                dest_native.clone(),
                metadata.clone(),
                options.verify,
                checks.clone(),
            )
            .await?
            {
                return Ok(TransferResult {
                    bytes_written: result.bytes_written,
                    strategy: TransferStrategy::ReflinkPatch,
                    verification: result.verification,
                });
            }
        }

        if dest_caps.atomic_rename {
            let result =
                native_whole_copy(source_native, dest_native, metadata, options.verify, checks)
                    .await?;
            return Ok(TransferResult {
                bytes_written: result.bytes_written,
                strategy: TransferStrategy::NativeWholeCopy,
                verification: result.verification,
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

        let result = copy_file_streaming(
            source,
            source_path,
            dest,
            dest_path,
            options.verify,
            options.rate_limiter.as_ref(),
            Some(&|| checks.verify(CheckPoint::Commit)),
        )
        .await?;
        return Ok(TransferResult {
            bytes_written: result.bytes_written,
            strategy: TransferStrategy::Streaming,
            verification: result.verification,
        });
    }

    Err(SyncError::Config(format!(
        "no safe transfer strategy for {:?} -> {:?}",
        source.endpoint_type(),
        dest.endpoint_type()
    )))
}

async fn native_whole_copy(
    source: PathBuf,
    dest: PathBuf,
    metadata: FileMetadata,
    verify: bool,
    checks: CommitChecks,
) -> Result<NativeTransferResult> {
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let temp = TempFileGuard::temp_path_for(&dest);
        let guard = TempFileGuard::new(&temp);
        let bytes_written = std::fs::copy(&source, &temp)?;
        strip_xattrs(&temp)?;

        let verification = verify_native_staging(&source, &temp, verify)?;
        if matches!(verification, VerificationStatus::Failed { .. }) {
            return Ok(NativeTransferResult {
                bytes_written,
                verification,
            });
        }

        checks.verify(CheckPoint::Commit)?;
        apply_metadata(&temp, &metadata)?;
        std::fs::rename(&temp, &dest)?;
        guard.defuse();
        Ok(NativeTransferResult {
            bytes_written,
            verification,
        })
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
}

async fn reflink_patch(
    source: PathBuf,
    dest: PathBuf,
    metadata: FileMetadata,
    verify: bool,
    checks: CommitChecks,
) -> Result<Option<NativeTransferResult>> {
    tokio::task::spawn_blocking(move || {
        if !dest.exists() {
            return Ok(None);
        }

        // Reflink patching trades an extra destination read for fewer physical
        // writes. Keep it conservative until the benchmark suite tunes this.
        let ratio = match sy::transfer::ratio::estimate_change_ratio(
            &source,
            &dest,
            TRANSFER_BUFFER_SIZE,
            Some(16),
            Some(0.25),
        ) {
            Ok(ratio) if ratio.use_delta => ratio,
            Ok(_) => return Ok(None),
            Err(error) => {
                tracing::debug!("reflink change sampling failed: {error}");
                return Ok(None);
            }
        };

        tracing::debug!(
            changed = %ratio.change_ratio_percent(),
            "using reflink patch strategy"
        );

        let temp = TempFileGuard::temp_path_for(&dest);
        let guard = TempFileGuard::new(&temp);
        if let Err(error) = reflink_clone(&dest, &temp) {
            tracing::debug!("reflink clone failed, falling back to whole copy: {error}");
            return Ok(None);
        }

        strip_xattrs(&temp)?;
        let bytes_written = patch_changed_blocks(&source, &dest, &temp, metadata.size)?;

        let verification = verify_native_staging(&source, &temp, verify)?;
        if matches!(verification, VerificationStatus::Failed { .. }) {
            return Ok(Some(NativeTransferResult {
                bytes_written,
                verification,
            }));
        }

        checks.verify(CheckPoint::Commit)?;
        apply_metadata(&temp, &metadata)?;
        std::fs::rename(&temp, &dest)?;
        guard.defuse();
        Ok(Some(NativeTransferResult {
            bytes_written,
            verification,
        }))
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
}

fn patch_changed_blocks(
    source: &Path,
    old_dest: &Path,
    staged: &Path,
    source_size: u64,
) -> std::io::Result<u64> {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut source_file = File::open(source)?;
    let mut dest_file = File::open(old_dest)?;
    let mut staged_file = OpenOptions::new().write(true).open(staged)?;
    let mut source_buf = vec![0_u8; TRANSFER_BUFFER_SIZE];
    let mut dest_buf = vec![0_u8; TRANSFER_BUFFER_SIZE];
    let mut offset = 0_u64;
    let mut bytes_written = 0_u64;

    loop {
        let source_read = source_file.read(&mut source_buf)?;
        if source_read == 0 {
            break;
        }
        let dest_read = dest_file.read(&mut dest_buf)?;

        if source_read != dest_read || source_buf[..source_read] != dest_buf[..dest_read] {
            staged_file.seek(SeekFrom::Start(offset))?;
            staged_file.write_all(&source_buf[..source_read])?;
            bytes_written += source_read as u64;
        }

        offset += source_read as u64;
    }

    staged_file.set_len(source_size)?;
    staged_file.flush()?;
    Ok(bytes_written)
}

#[cfg(target_os = "linux")]
fn reflink_clone(source: &Path, dest: &Path) -> std::io::Result<()> {
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;

    const FICLONE: libc::c_ulong = 0x4004_9409;

    let source_file = File::open(source)?;
    let dest_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dest)?;

    // SAFETY: both file descriptors are valid for the duration of the ioctl;
    // `source_file` is open for reading and `dest_file` is a distinct, newly
    // created writable file as required by FICLONE.
    let rc = unsafe { libc::ioctl(dest_file.as_raw_fd(), FICLONE, source_file.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        drop(dest_file);
        let _ = std::fs::remove_file(dest);
        Err(error)
    }
}

#[cfg(target_os = "macos")]
fn reflink_clone(source: &Path, dest: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn clonefile(
            source: *const libc::c_char,
            dest: *const libc::c_char,
            flags: libc::c_int,
        ) -> libc::c_int;
    }

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    let dest = CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;

    // SAFETY: both pointers come from live `CString` values and are therefore
    // NUL-terminated and valid for this call. The destination path is a fresh
    // staging path and flags=0 matches the documented clonefile contract.
    let rc = unsafe { clonefile(source.as_ptr(), dest.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn reflink_clone(_source: &Path, _dest: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "reflinks unsupported on this platform",
    ))
}

#[cfg(unix)]
fn native_file_is_sparse(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    let allocated = metadata.blocks() * 512;
    Ok(metadata.len() > 4096 && allocated < metadata.len().saturating_sub(4096))
}

#[cfg(not(unix))]
fn native_file_is_sparse(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
async fn native_sparse_copy(
    source: PathBuf,
    dest: PathBuf,
    metadata: FileMetadata,
    verify: bool,
    checks: CommitChecks,
) -> Result<Option<NativeTransferResult>> {
    tokio::task::spawn_blocking(move || {
        use std::fs::File;
        use std::io::{Read, Seek, SeekFrom, Write};

        let regions = match crate::sparse::detect_data_regions(&source) {
            Ok(regions) => regions,
            Err(error) => {
                tracing::debug!("sparse extent discovery failed, falling back: {error}");
                return Ok(None);
            }
        };

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = TempFileGuard::temp_path_for(&dest);
        let guard = TempFileGuard::new(&temp);
        let mut source_file = File::open(&source)?;
        let mut staged = File::create(&temp)?;
        staged.set_len(metadata.size)?;
        let mut buffer = vec![0_u8; TRANSFER_BUFFER_SIZE];
        let mut bytes_written = 0_u64;

        for region in regions {
            source_file.seek(SeekFrom::Start(region.offset))?;
            staged.seek(SeekFrom::Start(region.offset))?;
            let mut remaining = region.length;
            while remaining > 0 {
                let chunk = remaining.min(buffer.len() as u64) as usize;
                let read = source_file.read(&mut buffer[..chunk])?;
                if read == 0 {
                    break;
                }
                staged.write_all(&buffer[..read])?;
                remaining -= read as u64;
                bytes_written += read as u64;
            }
        }

        staged.flush()?;
        drop(staged);

        let verification = verify_native_staging(&source, &temp, verify)?;
        if matches!(verification, VerificationStatus::Failed { .. }) {
            return Ok(Some(NativeTransferResult {
                bytes_written,
                verification,
            }));
        }

        checks.verify(CheckPoint::Commit)?;
        apply_metadata(&temp, &metadata)?;
        std::fs::rename(&temp, &dest)?;
        guard.defuse();
        Ok(Some(NativeTransferResult {
            bytes_written,
            verification,
        }))
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
}

#[cfg(not(unix))]
async fn native_sparse_copy(
    _source: PathBuf,
    _dest: PathBuf,
    _metadata: FileMetadata,
    _verify: bool,
    _checks: CommitChecks,
) -> Result<Option<NativeTransferResult>> {
    Ok(None)
}

fn verify_native_staging(
    source: &Path,
    staged: &Path,
    verify: bool,
) -> std::io::Result<VerificationStatus> {
    if !verify {
        return Ok(VerificationStatus::NotRequested);
    }

    let expected = hash_native_file(source)?;
    let actual = hash_native_file(staged)?;
    if expected == actual {
        Ok(VerificationStatus::Verified)
    } else {
        Ok(VerificationStatus::Failed { expected, actual })
    }
}

fn hash_native_file(path: &Path) -> std::io::Result<blake3::Hash> {
    use std::fs::File;
    use std::io::Read;

    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; TRANSFER_BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

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
            identity,
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

    #[tokio::test]
    async fn matching_expectations_commit() {
        let fixture = Fixture::new();
        std::fs::write(fixture.source_file(), b"NEW!").unwrap();
        std::fs::write(fixture.dest_file(), b"OLD!").unwrap();
        let identity = update_identity(&fixture);

        transfer_file(
            &fixture.source_endpoint(),
            Path::new(NAME),
            &fixture.dest_endpoint(),
            Path::new(NAME),
            options(identity, true),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(fixture.dest_file()).unwrap(), b"NEW!");
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
        let mut content = vec![b'A'; size];
        content[0] = b'B';
        std::fs::write(fixture.source_file(), &content).unwrap();
        content[0] = b'A';
        std::fs::write(fixture.dest_file(), &content).unwrap();
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

        assert_eq!(result.strategy, TransferStrategy::ReflinkPatch);
        assert_eq!(std::fs::read(fixture.dest_file()).unwrap()[0], b'B');
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
