#[cfg(unix)]
mod scan;

#[cfg(all(target_os = "macos", feature = "acl"))]
mod acl_macos;

use crate::endpoint::ExpectedDestination;
use crate::engine::domain::{EntryIdentity, EntryKind, RelativePath, Timestamp};
use std::ffi::OsString;
use std::fs::File;
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::ffi::{CString, OsStr};
#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(unix)]
const TEMP_CREATE_ATTEMPTS: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum RootedFsError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("destination entry changed between scan and removal for {0}")]
    DestinationChanged(PathBuf),

    #[error("root directory identity changed after it was opened: {0}")]
    RootChanged(PathBuf),

    #[error("destination was committed at {path}, but its parent no longer matches the rooted path: {reason}")]
    CommittedParentChanged { path: PathBuf, reason: String },

    #[error("rooted filesystem path must contain only normal relative components")]
    InvalidRelativePath,

    #[error("filesystem path contains a NUL byte")]
    PathContainsNul,

    #[error("rooted file is not a regular file: {0}")]
    NotRegularFile(PathBuf),

    #[error("rooted entry kind mismatch for {path}: expected {expected:?}")]
    EntryKindMismatch { path: PathBuf, expected: EntryKind },

    #[error("symlink permissions are not a mutable portable property")]
    UnsupportedSymlinkMode,

    #[error("timestamp seconds are not representable by this platform")]
    TimestampOutOfRange,

    #[error("extended attributes exceed the bounded set size: {len} bytes (maximum {max})")]
    XattrSetTooLarge { len: usize, max: usize },

    #[error("preservation changed the staged mode: expected {expected:#o}, observed {actual:#o}")]
    PreservationModeConflict { expected: u32, actual: u32 },

    #[error("extended attributes are not preserved for symlinks")]
    UnsupportedSymlinkXattrs,

    #[error("access control lists exceed the bounded text size: {len} bytes (maximum {max})")]
    AclSetTooLarge { len: usize, max: usize },

    #[error("access control lists are not preserved for symlinks")]
    UnsupportedSymlinkAcls,

    #[error("bsd flags are not preserved for symlinks")]
    UnsupportedSymlinkBsdFlags,

    #[error("access control lists are unsupported: {0}")]
    AclUnsupported(&'static str),

    #[error("could not allocate a unique staging entry after {0} attempts")]
    StagingNameExhausted(usize),

    #[error("destination was committed at {path}, but private staging cleanup failed: {reason}")]
    CommittedCleanupPending { path: PathBuf, reason: String },

    #[error("staging directory identity changed; refusing to remove an unowned directory")]
    StagingDirectoryChanged,

    #[error("staging preparation failed ({operation}) and cleanup also failed ({cleanup})")]
    StagingPreparationCleanupFailed { operation: String, cleanup: String },

    #[error("staging abort cleanup failed (file: {file:?}, directory: {directory:?})")]
    StagingAbortFailed {
        file: Option<String>,
        directory: Option<String>,
    },

    #[error("staged operation failed ({operation}) and abort also failed ({abort})")]
    StagingOperationAbortFailed { operation: String, abort: String },

    #[error("staged commit was cancelled before publication")]
    CommitCancelled,

    #[error("held-root filesystem confinement is unsupported on this platform")]
    UnsupportedPlatform,

    #[error("rooted filesystem worker failed: {0}")]
    Worker(String),
}

pub type Result<T> = std::result::Result<T, RootedFsError>;

/// Filesystem authority pinned to one opened root directory.
///
/// Peer-influenced relative paths are resolved component-by-component from the
/// held root. Parent symlinks and a symlink leaf are never followed. The root
/// path itself is operator/session-selected and is resolved exactly once when
/// this handle is opened; later renames or symlink swaps of that pathname cannot
/// redirect operations performed through the held directory descriptor.
#[derive(Clone)]
pub struct RootedFs {
    root_path: Arc<PathBuf>,
    #[cfg(unix)]
    root_fd: Arc<OwnedFd>,
}

impl std::fmt::Debug for RootedFs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RootedFs")
            .field("root_path", &self.root_path)
            .finish_non_exhaustive()
    }
}

/// File staged inside a private same-filesystem directory held by descriptor.
///
/// The staging directory remains inaccessible to other users even after the
/// staged inode receives permissive final mode/ACL metadata. All namespace
/// operations stay relative to the held destination parent and staging dir.
pub struct RootedStagedFile {
    file: File,
    rooted: RootedFs,
    expected_destination: HeldDestinationExpectation,
    #[cfg(unix)]
    parent_fd: OwnedFd,
    #[cfg(unix)]
    staging_dir_fd: OwnedFd,
    #[cfg(unix)]
    staging_dir_name: OsString,
    #[cfg(unix)]
    temp_name: OsString,
    #[cfg(unix)]
    destination_name: OsString,
    destination_path: PathBuf,
    committed: bool,
}

impl std::fmt::Debug for RootedStagedFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RootedStagedFile")
            .field("committed", &self.committed)
            .finish_non_exhaustive()
    }
}

impl RootedStagedFile {
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// Duplicate the held descriptor for the async writer adapter. The
    /// duplicate refers to the same inode and open-file description; this
    /// transaction owner remains responsible for metadata and publication.
    pub fn try_clone_file(&self) -> Result<File> {
        Ok(self.file.try_clone()?)
    }

    /// Hash staged bytes through the already-open descriptor so restrictive
    /// final permissions do not prevent verification before commit.
    pub fn staged_hash_blocking(&mut self) -> Result<blake3::Hash> {
        use std::io::{Read, Seek, SeekFrom};

        self.file.seek(SeekFrom::Start(0))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut hasher = blake3::Hasher::new();
        loop {
            let read = self.file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        self.file.seek(SeekFrom::End(0))?;
        Ok(hasher.finalize())
    }

    /// Apply requested regular-file metadata to the still-private staging inode.
    /// This is intentionally performed before `commit`, so a metadata failure
    /// cannot expose a newly reconstructed file with temporary permissions.
    pub fn apply_metadata_blocking(
        &mut self,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            apply_fd_metadata(self.file.as_raw_fd(), unix_mode, modified)
        }
        #[cfg(not(unix))]
        {
            let _ = (unix_mode, modified);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }

    /// Mirror a preservation set onto the staging inode before commit, then
    /// prove the effective mode still matches `unix_mode` (ACL application can
    /// rewrite the shared mode bits). Staging is a fresh inode, so mirroring
    /// means setting exactly this set.
    pub fn apply_preservation_blocking(
        &mut self,
        xattrs: Option<&[(OsString, Vec<u8>)]>,
        acl: Option<&str>,
        unix_mode: Option<u32>,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            use xattr::FileExt;

            if let Some(xattrs) = xattrs {
                for (name, value) in xattrs {
                    self.file.set_xattr(name, value)?;
                }
            }
            if let Some(acl) = acl {
                #[cfg(feature = "acl")]
                apply_acl_fd(&self.file, acl)?;
                #[cfg(not(feature = "acl"))]
                {
                    let _ = acl;
                    return Err(RootedFsError::AclUnsupported(
                        "ACL preservation requires the acl feature; rebuild with --features acl",
                    ));
                }
            }
            if let Some(expected) = unix_mode.map(|mode| mode & 0o7777) {
                use std::os::unix::fs::PermissionsExt;
                let observed = self.file.metadata()?.permissions().mode() & 0o7777;
                if observed != expected {
                    return Err(RootedFsError::PreservationModeConflict {
                        expected,
                        actual: observed,
                    });
                }
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (xattrs, acl, unix_mode);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }

    /// Publish the staged inode atomically in the destination namespace.
    /// This is not a power-loss durability guarantee: no parent-directory
    /// persistence ordering is provided, and syncing every file without that
    /// stronger contract would penalize many-small-file syncs substantially.
    pub fn commit(self) -> Result<()> {
        self.commit_prepared()
    }

    /// Commit with cancellation serialized against the publication syscall.
    /// The caller sets the shared state when its awaiting future is dropped.
    pub fn commit_cancellable(mut self, cancellation: Arc<std::sync::Mutex<bool>>) -> Result<()> {
        let state = cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *state {
            drop(state);
            return Err(self.abort_after(RootedFsError::CommitCancelled));
        }
        self.commit_prepared()
    }

    fn commit_prepared(mut self) -> Result<()> {
        match self.commit_blocking() {
            Err(operation) if !self.committed => Err(self.abort_after(operation)),
            result => result,
        }
    }

    /// Explicitly abort and report cleanup failures instead of relying only on
    /// Drop's best-effort fallback.
    pub fn abort(mut self) -> Result<()> {
        self.abort_blocking()
    }

    fn abort_after(&mut self, operation: RootedFsError) -> RootedFsError {
        match self.abort_blocking() {
            Ok(()) => operation,
            Err(abort) => RootedFsError::StagingOperationAbortFailed {
                operation: operation.to_string(),
                abort: abort.to_string(),
            },
        }
    }

    #[cfg(unix)]
    fn abort_blocking(&mut self) -> Result<()> {
        let remove_file = match unlink_at(self.staging_dir_fd.as_raw_fd(), &self.temp_name, false) {
            Ok(()) => None,
            Err(RootedFsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(error),
        };
        let remove_dir = remove_owned_staging_dir_at(
            self.parent_fd.as_raw_fd(),
            self.staging_dir_fd.as_raw_fd(),
            &self.staging_dir_name,
        )
        .err();
        if remove_file.is_none() && remove_dir.is_none() {
            self.committed = true;
            return Ok(());
        }
        Err(RootedFsError::StagingAbortFailed {
            file: remove_file.map(|error| error.to_string()),
            directory: remove_dir.map(|error| error.to_string()),
        })
    }

    #[cfg(not(unix))]
    fn abort_blocking(&mut self) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn commit_blocking(&mut self) -> Result<()> {
        self.verify_parent_binding()?;
        self.verify_expected_destination()?;

        let dest_stat = stat_at_optional(self.parent_fd.as_raw_fd(), &self.destination_name)?;
        let staged_stat = stat_at_optional(self.staging_dir_fd.as_raw_fd(), &self.temp_name)?;
        let is_type_transition = match (dest_stat, staged_stat) {
            (Some(dest), Some(staged)) => {
                let dest_is_dir = (dest.st_mode & libc::S_IFMT) == libc::S_IFDIR;
                let staged_is_dir = (staged.st_mode & libc::S_IFMT) == libc::S_IFDIR;
                dest_is_dir != staged_is_dir
            }
            _ => false,
        };

        if is_type_transition {
            rename_exchange_at(
                self.staging_dir_fd.as_raw_fd(),
                &self.temp_name,
                self.parent_fd.as_raw_fd(),
                &self.destination_name,
            )?;
            self.committed = true;
            let old_dest_stat = stat_at_optional(self.staging_dir_fd.as_raw_fd(), &self.temp_name)?;
            if let Some(stat) = old_dest_stat {
                if (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR {
                    let _ = remove_dir_tree_at(self.staging_dir_fd.as_raw_fd(), &self.temp_name);
                } else {
                    let _ = unlink_at(self.staging_dir_fd.as_raw_fd(), &self.temp_name, false);
                }
            }
        } else {
            rename_between_at(
                self.staging_dir_fd.as_raw_fd(),
                &self.temp_name,
                self.parent_fd.as_raw_fd(),
                &self.destination_name,
            )?;
            self.committed = true;
        }

        remove_owned_staging_dir_at(
            self.parent_fd.as_raw_fd(),
            self.staging_dir_fd.as_raw_fd(),
            &self.staging_dir_name,
        )
        .map_err(|error| RootedFsError::CommittedCleanupPending {
            path: self.destination_path.clone(),
            reason: error.to_string(),
        })?;
        self.verify_parent_binding()
            .map_err(|error| RootedFsError::CommittedParentChanged {
                path: self.destination_path.clone(),
                reason: error.to_string(),
            })
    }

    #[cfg(not(unix))]
    fn commit_blocking(&mut self) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn verify_parent_binding(&self) -> Result<()> {
        let changed = || RootedFsError::DestinationChanged(self.destination_path.clone());
        let (parent, leaf) = self
            .rooted
            .open_parent_blocking(&self.destination_path)
            .map_err(|_| changed())?;
        if leaf != self.destination_name {
            return Err(changed());
        }
        let held = stat_fd(self.parent_fd.as_raw_fd())?;
        let current = stat_fd(parent.as_raw_fd())?;
        if held.st_dev == current.st_dev && held.st_ino == current.st_ino {
            Ok(())
        } else {
            Err(changed())
        }
    }

    #[cfg(unix)]
    fn verify_expected_destination(&self) -> Result<()> {
        match self.expected_destination {
            HeldDestinationExpectation::Unverified => Ok(()),
            HeldDestinationExpectation::Absent => {
                if stat_at_optional(self.parent_fd.as_raw_fd(), &self.destination_name)?.is_none() {
                    Ok(())
                } else {
                    Err(RootedFsError::DestinationChanged(
                        self.destination_path.clone(),
                    ))
                }
            }
            HeldDestinationExpectation::Unchanged(expected) => {
                let actual = stat_at_optional(self.parent_fd.as_raw_fd(), &self.destination_name)?
                    .and_then(|stat| identity_from_stat(&stat));
                if actual == Some(expected) {
                    Ok(())
                } else {
                    Err(RootedFsError::DestinationChanged(
                        self.destination_path.clone(),
                    ))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum HeldDestinationExpectation {
    Unverified,
    Absent,
    Unchanged(EntryIdentity),
}

impl Drop for RootedStagedFile {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        #[cfg(unix)]
        {
            let _ = unlink_at(self.staging_dir_fd.as_raw_fd(), &self.temp_name, false);
            let _ = remove_owned_staging_dir_at(
                self.parent_fd.as_raw_fd(),
                self.staging_dir_fd.as_raw_fd(),
                &self.staging_dir_name,
            );
        }
    }
}

#[cfg(unix)]
fn read_xattrs_from_file(file: &File) -> Result<Vec<(OsString, Vec<u8>)>> {
    use xattr::FileExt;

    let mut xattrs = Vec::new();
    let mut total = 0_usize;
    for name in file.list_xattr()? {
        // A value may be large on filesystems that store it out of line
        // (macOS resource forks). Check the aggregate bound before retaining it.
        let Some(value) = file.get_xattr(&name)? else {
            continue;
        };
        total = total
            .checked_add(name.as_bytes().len())
            .and_then(|value_total| value_total.checked_add(value.len()))
            .ok_or(RootedFsError::XattrSetTooLarge {
                len: usize::MAX,
                max: crate::protocol::MAX_XATTR_TOTAL_BYTES,
            })?;
        if total > crate::protocol::MAX_XATTR_TOTAL_BYTES {
            return Err(RootedFsError::XattrSetTooLarge {
                len: total,
                max: crate::protocol::MAX_XATTR_TOTAL_BYTES,
            });
        }
        xattrs.push((name, value));
    }
    Ok(xattrs)
}

#[cfg(not(unix))]
fn read_xattrs_from_file(_file: &File) -> Result<Vec<(OsString, Vec<u8>)>> {
    Err(RootedFsError::UnsupportedPlatform)
}

#[cfg(any(
    all(target_os = "linux", feature = "acl"),
    all(target_os = "macos", feature = "acl")
))]
fn check_acl_text_bounded(text: String) -> Result<Option<String>> {
    if text.len() > crate::protocol::MAX_ACL_TEXT_BYTES {
        return Err(RootedFsError::AclSetTooLarge {
            len: text.len(),
            max: crate::protocol::MAX_ACL_TEXT_BYTES,
        });
    }
    Ok(Some(text))
}

impl RootedFs {
    /// Open and pin a root directory without blocking a Tokio worker thread.
    pub async fn open(root: PathBuf) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::open_blocking(root))
            .await
            .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Keep the remote scan API available on unsupported platforms so the
    /// agent can reject the operation through the normal typed error stream.
    #[cfg(not(unix))]
    pub(crate) fn entry_stream(
        &self,
        _request: crate::engine::scan::ScanRequest,
    ) -> crate::engine::reconcile::EntryStream {
        Box::pin(futures::stream::once(async {
            Err(Box::new(RootedFsError::UnsupportedPlatform) as crate::engine::reconcile::BoxError)
        }))
    }

    /// Open one regular file relative to the pinned root without following any
    /// peer-controlled symlink component.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn open_regular_blocking(&self, relative: &RelativePath) -> Result<File> {
        self.open_regular_path_blocking(relative.as_path())
    }

    /// Create a same-directory temporary file for one destination beneath the
    /// pinned root. No peer-controlled parent or leaf symlink is followed while
    /// resolving the parent. The returned writer owns that resolved parent until
    /// atomic commit or cleanup.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn begin_staged_file_blocking(&self, relative: &RelativePath) -> Result<RootedStagedFile> {
        self.begin_staged_file_with_expectation_blocking(relative, ExpectedDestination::Unverified)
    }

    /// Begin a staged write while binding destination expectations to the same
    /// held parent descriptor used by commit.
    pub fn begin_staged_file_with_expectation_blocking(
        &self,
        relative: &RelativePath,
        expected: ExpectedDestination,
    ) -> Result<RootedStagedFile> {
        self.begin_staged_path_blocking(relative.as_path(), expected)
    }

    /// Create all missing parent directories beneath this held root.
    #[cfg(unix)]
    pub fn create_directories_blocking(&self, relative: &RelativePath) -> Result<()> {
        self.ensure_directories_blocking(relative.as_path())
    }

    #[cfg(not(unix))]
    pub fn create_directories_blocking(&self, _relative: &RelativePath) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// Check that the configured pathname still identifies the held root.
    /// Ordinary `RootedFs` operations stay pinned even if the pathname moves;
    /// adapters using a mixed rooted/path-based surface can use this to reject
    /// a divergent pathname.
    #[cfg(unix)]
    pub fn verify_root_path_blocking(&self) -> Result<()> {
        verify_root_path_identity(&self.root_path, self.root_fd.as_raw_fd())
    }

    #[cfg(not(unix))]
    pub fn verify_root_path_blocking(&self) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// Observe the identity of one root-relative entry without following a
    /// peer-controlled leaf symlink. `None` means the path is absent.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    #[cfg(unix)]
    pub fn path_identity_blocking(
        &self,
        relative: &RelativePath,
    ) -> Result<Option<(EntryKind, EntryIdentity)>> {
        let (parent, leaf) = self.open_parent_blocking(relative.as_path())?;
        let leaf_c = component_cstring(&leaf)?;
        let mut stat = MaybeUninit::<libc::stat>::zeroed();
        let stat_res = unsafe {
            // SAFETY: `parent` is live, `leaf_c` is a live single component, and
            // `stat` points to writable storage for one libc::stat value.
            libc::fstatat(
                parent.as_raw_fd(),
                leaf_c.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_res < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let stat = unsafe {
            // SAFETY: successful fstatat initialized the stat structure.
            stat.assume_init()
        };
        let file_type = stat.st_mode & libc::S_IFMT;
        let kind = if file_type == libc::S_IFDIR {
            EntryKind::Directory
        } else if file_type == libc::S_IFLNK {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };
        let identity = crate::endpoint::local_identity::stat_identity(&stat, kind)
            .ok_or_else(|| RootedFsError::DestinationChanged(relative.as_path().to_path_buf()))?;
        Ok(Some((kind, identity)))
    }

    #[cfg(not(unix))]
    pub fn path_identity_blocking(
        &self,
        _relative: &RelativePath,
    ) -> Result<Option<(EntryKind, EntryIdentity)>> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// Create one directory beneath the pinned root without following any
    /// peer-controlled parent symlink. Existing real directories are accepted
    /// so repeated create requests are idempotent; files and symlinks are not.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn create_directory_blocking(&self, relative: &RelativePath) -> Result<()> {
        self.create_directory_path_blocking(relative.as_path())
    }

    /// Link one root-relative path to an existing root-relative regular file
    /// (`-H/--preserve-hardlinks`). The source is verified as a regular file
    /// through its no-follow parent before linking; a symlink source is
    /// refused. The link is staged under a temporary name in the held
    /// destination parent and renamed over the destination, so replacing a
    /// file/symlink is atomic and a directory destination fails loudly
    /// instead of being recursed into. Destination parents are created
    /// root-confined, matching copy semantics.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn create_hardlink_blocking(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
    ) -> Result<()> {
        self.create_hardlink_path_blocking(source.as_path(), destination.as_path())
    }

    /// Copy one regular file to another root-relative path (`--backup`). The
    /// source is opened with the same no-follow, root-confined resolution as
    /// transfers; the destination is written through staged rename so a
    /// failed copy never exposes a partial backup. Destination parent
    /// directories are created root-confined. The copy preserves the source's
    /// mode and mtime.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn copy_file_blocking(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
    ) -> Result<()> {
        self.copy_file_path_blocking(source.as_path(), destination.as_path())
    }

    /// Atomically replace a non-directory destination with a symlink while the
    /// resolved parent directory remains pinned. The symlink target is stored as
    /// opaque native path data and is never resolved by this operation.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn replace_symlink_blocking(&self, relative: &RelativePath, target: &Path) -> Result<()> {
        self.replace_symlink_path_blocking(relative.as_path(), target)
    }

    /// Remove one file-like or directory leaf beneath the pinned root. Parent
    /// components are opened with no-follow semantics and `unlinkat` never
    /// follows the destination leaf.
    ///
    /// If `expected_identity` is provided, the existing entry is inspected
    /// through the held descriptor with no-follow semantics before removal,
    /// and `RootedFsError::DestinationChanged` is returned on identity or type mismatch.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn remove_blocking(
        &self,
        relative: &RelativePath,
        is_directory: bool,
        expected_identity: Option<EntryIdentity>,
    ) -> Result<()> {
        self.remove_path_blocking(relative.as_path(), is_directory, expected_identity)
    }

    /// Recursively remove a directory tree beneath the pinned root using
    /// directory-descriptor relative traversal with no-follow semantics.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    #[cfg(unix)]
    pub fn remove_dir_tree_blocking(&self, relative: &RelativePath) -> Result<()> {
        let (parent, leaf) = self.open_parent_blocking(relative.as_path())?;
        remove_dir_tree_at(parent.as_raw_fd(), &leaf)
    }

    #[cfg(not(unix))]
    pub fn remove_dir_tree_blocking(&self, _relative: &RelativePath) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// Read every extended attribute of one file or directory beneath the
    /// pinned root. The leaf is opened without following a symlink and the
    /// attributes are read through that held descriptor, so a raced path swap
    /// cannot redirect the read. The total set is bounded; an oversized set is
    /// refused loudly rather than truncated. Symlinks are refused (their
    /// attributes are not portable and reading them would resolve the target).
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn read_xattrs_blocking(
        &self,
        relative: &RelativePath,
        kind: EntryKind,
    ) -> Result<Vec<(OsString, Vec<u8>)>> {
        self.read_xattrs_path_blocking(relative.as_path(), kind)
    }

    /// Read preservation xattrs through a regular file handle opened beneath
    /// this root. The caller must pass the handle whose identity was validated
    /// for the in-flight transfer; this keeps metadata bound to those bytes.
    pub fn read_open_file_xattrs_blocking(
        &self,
        file: &File,
        relative: &RelativePath,
    ) -> Result<Vec<(OsString, Vec<u8>)>> {
        if !file.metadata()?.file_type().is_file() {
            return Err(RootedFsError::NotRegularFile(
                relative.as_path().to_path_buf(),
            ));
        }
        read_xattrs_from_file(file)
    }

    /// Mirror an extended-attribute set onto one file or directory beneath the
    /// pinned root: every requested attribute is set and every existing
    /// attribute absent from the request is removed. The leaf is opened without
    /// following a symlink and every mutation goes through that held
    /// descriptor. Symlinks are refused.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn write_xattrs_blocking(
        &self,
        relative: &RelativePath,
        kind: EntryKind,
        xattrs: &[(OsString, Vec<u8>)],
    ) -> Result<()> {
        self.write_xattrs_path_blocking(relative.as_path(), kind, xattrs)
    }

    /// Read the access-control list of one file or directory beneath the
    /// pinned root, as exacl unified-entries text (`None` when the entry
    /// carries no ACL). The leaf is opened without following a symlink and
    /// every read goes through that held descriptor, so a raced path swap
    /// cannot redirect the read. The text is bounded; an oversized list is
    /// refused loudly rather than truncated. Symlinks are refused (their
    /// ACLs are not portable and reading them would resolve the target).
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn read_acl_blocking(
        &self,
        relative: &RelativePath,
        kind: EntryKind,
    ) -> Result<Option<String>> {
        if kind == EntryKind::Symlink {
            return Err(RootedFsError::UnsupportedSymlinkAcls);
        }
        self.read_acl_path_blocking(relative.as_path(), kind)
    }

    /// Read a file's ACL through the already-open handle for its in-flight
    /// transfer, rather than reopening the visible path for a metadata RPC.
    pub fn read_open_file_acl_blocking(
        &self,
        file: &File,
        relative: &RelativePath,
    ) -> Result<Option<String>> {
        if !file.metadata()?.file_type().is_file() {
            return Err(RootedFsError::NotRegularFile(
                relative.as_path().to_path_buf(),
            ));
        }
        self.read_acl_from_file(file)
    }

    /// Mirror an access-control list, as exacl unified-entries text, onto one
    /// file or directory beneath the pinned root. An empty string clears the
    /// list (on Linux the mode-derived base entries are restored, matching
    /// `LocalEndpoint::write_acl`). The leaf is opened without following a
    /// symlink and every mutation goes through that held descriptor.
    /// Symlinks are refused.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn write_acl_blocking(
        &self,
        relative: &RelativePath,
        kind: EntryKind,
        acl: &str,
    ) -> Result<()> {
        if kind == EntryKind::Symlink {
            return Err(RootedFsError::UnsupportedSymlinkAcls);
        }
        if acl.len() > crate::protocol::MAX_ACL_TEXT_BYTES {
            return Err(RootedFsError::AclSetTooLarge {
                len: acl.len(),
                max: crate::protocol::MAX_ACL_TEXT_BYTES,
            });
        }
        self.write_acl_path_blocking(relative.as_path(), kind, acl)
    }

    /// Read the BSD file flags of one file or directory beneath the pinned
    /// root (macOS only). The leaf is opened without following a symlink and
    /// the flags are read through that held descriptor. Symlinks are refused,
    /// like xattrs and ACLs.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn read_bsd_flags_blocking(&self, relative: &RelativePath, kind: EntryKind) -> Result<u32> {
        if kind == EntryKind::Symlink {
            return Err(RootedFsError::UnsupportedSymlinkBsdFlags);
        }
        self.read_bsd_flags_path_blocking(relative.as_path(), kind)
    }

    /// Mirror BSD file flags onto one file or directory beneath the pinned
    /// root (macOS only): 0 clears every flag. The leaf is opened without
    /// following a symlink and the mutation goes through that held
    /// descriptor. Symlinks are refused.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn write_bsd_flags_blocking(
        &self,
        relative: &RelativePath,
        kind: EntryKind,
        flags: u32,
    ) -> Result<()> {
        if kind == EntryKind::Symlink {
            return Err(RootedFsError::UnsupportedSymlinkBsdFlags);
        }
        self.write_bsd_flags_path_blocking(relative.as_path(), kind, flags)
    }

    /// Apply requested metadata to an existing entry beneath the pinned root.
    /// Regular files and directories are opened without following the leaf;
    /// symlink timestamps use `utimensat(..., AT_SYMLINK_NOFOLLOW)` and never
    /// affect the symlink target.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn apply_metadata_blocking(
        &self,
        relative: &RelativePath,
        kind: EntryKind,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> Result<()> {
        self.apply_metadata_path_blocking(relative.as_path(), kind, unix_mode, modified)
    }

    #[cfg(unix)]
    fn open_blocking(root: PathBuf) -> Result<Self> {
        let root_c = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| RootedFsError::PathContainsNul)?;
        // The root is trusted session input. Follow any operator-selected root
        // symlink once, then pin the resulting directory inode by descriptor.
        let fd = unsafe {
            // SAFETY: `root_c` is a live NUL-terminated pathname. `open` only
            // borrows the pointer for the duration of this call.
            libc::open(
                root_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let root_fd = unsafe {
            // SAFETY: a successful `open` returned a fresh owned descriptor.
            OwnedFd::from_raw_fd(fd)
        };

        Ok(Self {
            root_path: Arc::new(root),
            root_fd: Arc::new(root_fd),
        })
    }

    #[cfg(not(unix))]
    fn open_blocking(_root: PathBuf) -> Result<Self> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn open_regular_path_blocking(&self, relative: &Path) -> Result<File> {
        let (parent, leaf) = self.open_parent_blocking(relative)?;
        let file = open_file_at(parent.as_raw_fd(), &leaf)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(RootedFsError::NotRegularFile(relative.to_path_buf()));
        }
        Ok(file)
    }

    #[cfg(not(unix))]
    fn open_regular_path_blocking(&self, _relative: &Path) -> Result<File> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn begin_staged_path_blocking(
        &self,
        relative: &Path,
        expected: ExpectedDestination,
    ) -> Result<RootedStagedFile> {
        let (parent_fd, destination_name) = self.open_parent_blocking(relative)?;
        let expected_destination = capture_destination_expectation(
            parent_fd.as_raw_fd(),
            &destination_name,
            relative,
            expected,
        )?;
        let temp_name = OsString::from("contents");

        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let staging_dir_name = next_temp_name();
            let staging_dir_fd =
                match create_private_staging_dir_at(parent_fd.as_raw_fd(), &staging_dir_name) {
                    Ok(fd) => fd,
                    Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::EEXIST) => {
                        continue;
                    }
                    Err(error) => return Err(error),
                };

            let file = match create_staging_file_at(staging_dir_fd.as_raw_fd(), &temp_name) {
                Ok(file) => file,
                Err(error) => {
                    return Err(cleanup_staging_directory_preparation(
                        error,
                        parent_fd.as_raw_fd(),
                        staging_dir_fd.as_raw_fd(),
                        &staging_dir_name,
                    ));
                }
            };
            if let Err(error) = verify_staging_file_group(parent_fd.as_raw_fd(), &file) {
                drop(file);
                return Err(cleanup_staging_file_preparation(
                    error,
                    parent_fd.as_raw_fd(),
                    staging_dir_fd.as_raw_fd(),
                    &temp_name,
                    &staging_dir_name,
                ));
            }
            return Ok(RootedStagedFile {
                file,
                rooted: self.clone(),
                expected_destination,
                parent_fd,
                staging_dir_fd,
                staging_dir_name,
                temp_name,
                destination_name,
                destination_path: relative.to_path_buf(),
                committed: false,
            });
        }

        Err(RootedFsError::StagingNameExhausted(TEMP_CREATE_ATTEMPTS))
    }

    #[cfg(not(unix))]
    fn begin_staged_path_blocking(
        &self,
        _relative: &Path,
        _expected: ExpectedDestination,
    ) -> Result<RootedStagedFile> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn create_hardlink_path_blocking(&self, source: &Path, destination: &Path) -> Result<()> {
        if let Some(parent) = destination.parent() {
            if !parent.as_os_str().is_empty() {
                self.ensure_directories_blocking(parent)?;
            }
        }
        // Resolve both parents without following peer-controlled symlinks.
        // The source leaf is opened O_NOFOLLOW and required to be a regular
        // file so a symlink source can never be linked through.
        let (source_parent, source_leaf) = self.open_parent_blocking(source)?;
        let source_file = open_file_at(source_parent.as_raw_fd(), &source_leaf)?;
        if !source_file.metadata()?.file_type().is_file() {
            return Err(RootedFsError::NotRegularFile(source.to_path_buf()));
        }
        let (destination_parent, destination_leaf) = self.open_parent_blocking(destination)?;
        let destination_parent_fd = destination_parent.as_raw_fd();
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let temp_name = next_temp_name();
            match link_at(
                source_parent.as_raw_fd(),
                &source_leaf,
                destination_parent_fd,
                &temp_name,
            ) {
                Ok(()) => {
                    return match rename_at(destination_parent_fd, &temp_name, &destination_leaf) {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            let _ = unlink_at(destination_parent_fd, &temp_name, false);
                            Err(error)
                        }
                    };
                }
                Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::EEXIST) => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(RootedFsError::StagingNameExhausted(TEMP_CREATE_ATTEMPTS))
    }

    #[cfg(not(unix))]
    fn create_hardlink_path_blocking(&self, _source: &Path, _destination: &Path) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn copy_file_path_blocking(&self, source: &Path, destination: &Path) -> Result<()> {
        // Parent directories of the backup location may not exist yet; create
        // each one root-confined (mkdir -p semantics beneath the pinned root).
        if let Some(parent) = destination.parent() {
            self.ensure_directories_blocking(parent)?;
        }

        let source_file = self.open_regular_path_blocking(source)?;
        let source_metadata = source_file.metadata()?;
        if !source_metadata.file_type().is_file() {
            return Err(RootedFsError::NotRegularFile(source.to_path_buf()));
        }
        let source_mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(source_metadata.permissions().mode() & 0o7777)
        };
        let source_mtime = source_metadata.modified().ok().and_then(|time| {
            let duration = time.duration_since(std::time::UNIX_EPOCH).ok()?;
            Timestamp::new(duration.as_secs() as i64, duration.subsec_nanos()).ok()
        });

        let mut staged =
            self.begin_staged_path_blocking(destination, ExpectedDestination::Unverified)?;
        std::io::copy(&mut &source_file, staged.file_mut())?;
        staged.apply_metadata_blocking(source_mode, source_mtime)?;
        staged.commit()
    }

    #[cfg(not(unix))]
    fn copy_file_path_blocking(&self, _source: &Path, _destination: &Path) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// Create each missing ancestor directory of `relative` beneath the
    /// pinned root without following symlink components. Existing real
    /// directories are accepted; other leaf kinds are errors.
    #[cfg(unix)]
    fn ensure_directories_blocking(&self, relative: &Path) -> Result<()> {
        let mut current_fd = self.root_fd.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            current_fd = match open_dir_at(current_fd.as_raw_fd(), name) {
                Ok(fd) => fd,
                Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::ENOENT) => {
                    let name_c = component_cstring(name)?;
                    let result = unsafe {
                        // SAFETY: `current_fd` remains open and `name_c` is a
                        // live single component. mkdirat creates only beneath
                        // the already-resolved parent.
                        libc::mkdirat(current_fd.as_raw_fd(), name_c.as_ptr(), 0o777)
                    };
                    if result != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::EEXIST) {
                            return Err(error.into());
                        }
                    }
                    // A concurrent creator may win mkdirat; reopen with the
                    // same no-follow directory checks in either case.
                    open_dir_at(current_fd.as_raw_fd(), name)?
                }
                Err(error) => return Err(error),
            };
        }
        Ok(())
    }

    #[cfg(unix)]
    fn create_directory_path_blocking(&self, relative: &Path) -> Result<()> {
        let (parent, leaf) = self.open_parent_blocking(relative)?;
        let leaf_c = component_cstring(&leaf)?;
        let result = unsafe {
            // SAFETY: `parent` remains open and `leaf_c` is a live single
            // component. mkdirat creates only beneath the already-resolved
            // parent; the process umask applies to the requested default mode.
            libc::mkdirat(parent.as_raw_fd(), leaf_c.as_ptr(), 0o777)
        };
        if result == 0 {
            return Ok(());
        }

        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EEXIST) {
            open_dir_at(parent.as_raw_fd(), &leaf)?;
            return Ok(());
        }
        Err(error.into())
    }

    #[cfg(not(unix))]
    fn create_directory_path_blocking(&self, _relative: &Path) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn replace_symlink_path_blocking(&self, relative: &Path, target: &Path) -> Result<()> {
        let (parent, destination_name) = self.open_parent_blocking(relative)?;
        let target = CString::new(target.as_os_str().as_bytes())
            .map_err(|_| RootedFsError::PathContainsNul)?;

        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let temp_name = next_temp_name();
            let temp = component_cstring(&temp_name)?;
            let result = unsafe {
                // SAFETY: `target` and `temp` are live NUL-terminated strings,
                // and `parent` pins the destination directory. symlinkat stores
                // the target bytes verbatim and does not resolve them.
                libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), temp.as_ptr())
            };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EEXIST) {
                    continue;
                }
                return Err(error.into());
            }

            let dest_stat = stat_at_optional(parent.as_raw_fd(), &destination_name)?;
            let is_dest_dir =
                dest_stat.is_some_and(|s| (s.st_mode & libc::S_IFMT) == libc::S_IFDIR);

            if is_dest_dir {
                match rename_exchange_at(
                    parent.as_raw_fd(),
                    &temp_name,
                    parent.as_raw_fd(),
                    &destination_name,
                ) {
                    Ok(()) => {
                        let _ = remove_dir_tree_at(parent.as_raw_fd(), &temp_name);
                        return Ok(());
                    }
                    Err(error) => {
                        let _ = unlink_at(parent.as_raw_fd(), &temp_name, false);
                        return Err(error);
                    }
                }
            } else {
                match rename_at(parent.as_raw_fd(), &temp_name, &destination_name) {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        let _ = unlink_at(parent.as_raw_fd(), &temp_name, false);
                        return Err(error);
                    }
                }
            }
        }

        Err(RootedFsError::StagingNameExhausted(TEMP_CREATE_ATTEMPTS))
    }

    #[cfg(not(unix))]
    fn replace_symlink_path_blocking(&self, _relative: &Path, _target: &Path) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn remove_path_blocking(
        &self,
        relative: &Path,
        is_directory: bool,
        expected_identity: Option<EntryIdentity>,
    ) -> Result<()> {
        let (parent, leaf) = self.open_parent_blocking(relative)?;
        let leaf_c = component_cstring(&leaf)?;
        let mut stat = MaybeUninit::<libc::stat>::zeroed();
        let stat_res = unsafe {
            // SAFETY: `parent` is live, `leaf_c` is a live single component, and
            // `stat` points to writable storage for one libc::stat value.
            libc::fstatat(
                parent.as_raw_fd(),
                leaf_c.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_res < 0 {
            let error = std::io::Error::last_os_error();
            // A vanished entry is idempotent success.
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(error.into());
        }
        let stat = unsafe {
            // SAFETY: successful fstatat initialized the stat structure.
            stat.assume_init()
        };

        let file_type = stat.st_mode & libc::S_IFMT;
        let actual_is_dir = file_type == libc::S_IFDIR;
        if is_directory != actual_is_dir {
            return Err(RootedFsError::DestinationChanged(relative.to_path_buf()));
        }

        if let Some(expected) = expected_identity {
            let kind = if actual_is_dir {
                EntryKind::Directory
            } else if file_type == libc::S_IFLNK {
                EntryKind::Symlink
            } else {
                EntryKind::File
            };
            let actual_identity = crate::endpoint::local_identity::stat_identity(&stat, kind)
                .ok_or_else(|| RootedFsError::DestinationChanged(relative.to_path_buf()))?;
            if actual_identity != expected {
                return Err(RootedFsError::DestinationChanged(relative.to_path_buf()));
            }
        }

        match unlink_at(parent.as_raw_fd(), &leaf, is_directory) {
            Ok(()) => Ok(()),
            // A vanished entry is idempotent success.
            Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            // A non-empty directory is kept, not an error: under --backup a
            // deletion's backup copy may legitimately repopulate the
            // directory right before its removal (rsync behaves the same).
            Err(RootedFsError::Io(error))
                if is_directory
                    && (error.raw_os_error() == Some(libc::ENOTEMPTY)
                        || error.raw_os_error() == Some(libc::EEXIST)) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(not(unix))]
    fn remove_path_blocking(
        &self,
        _relative: &Path,
        _is_directory: bool,
        _expected_identity: Option<EntryIdentity>,
    ) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn apply_metadata_path_blocking(
        &self,
        relative: &Path,
        kind: EntryKind,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> Result<()> {
        let (parent, leaf) = self.open_parent_blocking(relative)?;
        match kind {
            EntryKind::File => {
                let file = open_file_at(parent.as_raw_fd(), &leaf)?;
                if !file.metadata()?.file_type().is_file() {
                    return Err(RootedFsError::EntryKindMismatch {
                        path: relative.to_path_buf(),
                        expected: kind,
                    });
                }
                apply_fd_metadata(file.as_raw_fd(), unix_mode, modified)
            }
            EntryKind::Directory => {
                let directory = open_dir_at(parent.as_raw_fd(), &leaf)?;
                apply_fd_metadata(directory.as_raw_fd(), unix_mode, modified)
            }
            EntryKind::Symlink => {
                if unix_mode.is_some() {
                    return Err(RootedFsError::UnsupportedSymlinkMode);
                }
                ensure_symlink_at(parent.as_raw_fd(), &leaf, relative)?;
                if let Some(modified) = modified {
                    set_symlink_mtime_at(parent.as_raw_fd(), &leaf, modified)?;
                }
                Ok(())
            }
        }
    }

    #[cfg(not(unix))]
    fn apply_metadata_path_blocking(
        &self,
        _relative: &Path,
        _kind: EntryKind,
        _unix_mode: Option<u32>,
        _modified: Option<Timestamp>,
    ) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn read_xattrs_path_blocking(
        &self,
        relative: &Path,
        kind: EntryKind,
    ) -> Result<Vec<(OsString, Vec<u8>)>> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        read_xattrs_from_file(&file)
    }

    #[cfg(not(unix))]
    fn read_xattrs_path_blocking(
        &self,
        _relative: &Path,
        _kind: EntryKind,
    ) -> Result<Vec<(OsString, Vec<u8>)>> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn write_xattrs_path_blocking(
        &self,
        relative: &Path,
        kind: EntryKind,
        xattrs: &[(OsString, Vec<u8>)],
    ) -> Result<()> {
        use xattr::FileExt;

        let total = xattrs.iter().try_fold(0_usize, |total, (name, value)| {
            total
                .checked_add(name.as_bytes().len())
                .and_then(|value_total| value_total.checked_add(value.len()))
                .ok_or(RootedFsError::XattrSetTooLarge {
                    len: usize::MAX,
                    max: crate::protocol::MAX_XATTR_TOTAL_BYTES,
                })
        })?;
        if total > crate::protocol::MAX_XATTR_TOTAL_BYTES {
            return Err(RootedFsError::XattrSetTooLarge {
                len: total,
                max: crate::protocol::MAX_XATTR_TOTAL_BYTES,
            });
        }

        let file = self.open_xattr_entry_blocking(relative, kind)?;
        for (name, value) in xattrs {
            file.set_xattr(name, value)?;
        }
        // Mirror semantics: attributes that exist only on the destination are
        // removed so stale values cannot survive a sync.
        for existing in file.list_xattr()? {
            if !xattrs.iter().any(|(name, _)| name == &existing) {
                match file.remove_xattr(&existing) {
                    Ok(()) => {}
                    Err(error)
                        if error.kind() == std::io::ErrorKind::PermissionDenied
                            || error.raw_os_error() == Some(libc::EPERM)
                            || error.raw_os_error() == Some(libc::EACCES) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn write_xattrs_path_blocking(
        &self,
        _relative: &Path,
        _kind: EntryKind,
        _xattrs: &[(OsString, Vec<u8>)],
    ) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// Linux: exacl is path-based, but `/proc/self/fd/N` resolves to the
    /// already-open inode for `acl_get_file` (verified on Fedora: access
    /// reads, default-entry reads, and writes through read-only descriptors
    /// all follow the magic link). The fd comes from
    /// `open_xattr_entry_blocking`, so confinement is unchanged and no
    /// acl_t conversion is needed: the text is exacl's own unified format.
    /// If `/proc` is unavailable the lookup fails loudly instead of
    /// silently reading the wrong file.
    #[cfg(all(target_os = "linux", feature = "acl"))]
    fn read_acl_from_file(&self, file: &File) -> Result<Option<String>> {
        let entries = exacl::getfacl(fd_alias_path(file), None)?;
        if entries.is_empty() {
            return Ok(None);
        }
        let text = exacl::to_string(&entries)?;
        check_acl_text_bounded(text)
    }

    #[cfg(all(target_os = "linux", feature = "acl"))]
    fn read_acl_path_blocking(&self, relative: &Path, kind: EntryKind) -> Result<Option<String>> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        self.read_acl_from_file(&file)
    }

    #[cfg(all(target_os = "linux", feature = "acl"))]
    fn write_acl_path_blocking(&self, relative: &Path, kind: EntryKind, acl: &str) -> Result<()> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        apply_acl_fd(&file, acl)
    }

    /// macOS: `/dev/fd/N` does NOT resolve to the open inode for
    /// `acl_get_file` (verified: it returns an empty list), so the fd-based
    /// syscalls in `acl_macos` carry the conversion instead. Same
    /// no-follow held descriptor, same exacl text format.
    #[cfg(all(target_os = "macos", feature = "acl"))]
    fn read_acl_from_file(&self, file: &File) -> Result<Option<String>> {
        let entries = acl_macos::read_fd_entries(file.as_raw_fd())?;
        if entries.is_empty() {
            return Ok(None);
        }
        let text = exacl::to_string(&entries)?;
        check_acl_text_bounded(text)
    }

    #[cfg(all(target_os = "macos", feature = "acl"))]
    fn read_acl_path_blocking(&self, relative: &Path, kind: EntryKind) -> Result<Option<String>> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        self.read_acl_from_file(&file)
    }

    #[cfg(all(target_os = "macos", feature = "acl"))]
    fn write_acl_path_blocking(&self, relative: &Path, kind: EntryKind, acl: &str) -> Result<()> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        apply_acl_fd(&file, acl)
    }

    #[cfg(all(unix, not(feature = "acl")))]
    fn read_acl_from_file(&self, _file: &File) -> Result<Option<String>> {
        Err(RootedFsError::AclUnsupported(
            "ACL preservation requires the acl feature; rebuild with --features acl",
        ))
    }

    #[cfg(all(unix, not(feature = "acl")))]
    fn read_acl_path_blocking(&self, _relative: &Path, _kind: EntryKind) -> Result<Option<String>> {
        Err(RootedFsError::AclUnsupported(
            "ACL preservation requires the acl feature; rebuild with --features acl",
        ))
    }

    #[cfg(all(unix, not(feature = "acl")))]
    fn write_acl_path_blocking(
        &self,
        _relative: &Path,
        _kind: EntryKind,
        _acl: &str,
    ) -> Result<()> {
        Err(RootedFsError::AclUnsupported(
            "ACL preservation requires the acl feature; rebuild with --features acl",
        ))
    }

    #[cfg(all(
        unix,
        feature = "acl",
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    fn read_acl_from_file(&self, _file: &File) -> Result<Option<String>> {
        Err(RootedFsError::AclUnsupported(
            "access control lists are only supported on Linux and macOS",
        ))
    }

    #[cfg(all(
        unix,
        feature = "acl",
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    fn read_acl_path_blocking(&self, _relative: &Path, _kind: EntryKind) -> Result<Option<String>> {
        Err(RootedFsError::AclUnsupported(
            "access control lists are only supported on Linux and macOS",
        ))
    }

    #[cfg(all(
        unix,
        feature = "acl",
        not(target_os = "linux"),
        not(target_os = "macos")
    ))]
    fn write_acl_path_blocking(
        &self,
        _relative: &Path,
        _kind: EntryKind,
        _acl: &str,
    ) -> Result<()> {
        Err(RootedFsError::AclUnsupported(
            "access control lists are only supported on Linux and macOS",
        ))
    }

    #[cfg(not(unix))]
    fn read_acl_from_file(&self, _file: &File) -> Result<Option<String>> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(not(unix))]
    fn read_acl_path_blocking(&self, _relative: &Path, _kind: EntryKind) -> Result<Option<String>> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(not(unix))]
    fn write_acl_path_blocking(
        &self,
        _relative: &Path,
        _kind: EntryKind,
        _acl: &str,
    ) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// macOS: read `st_flags` through the held no-follow leaf descriptor.
    /// `fstat` on an `O_NOFOLLOW` open reports the leaf itself, so neither
    /// parent nor leaf symlinks can redirect the read.
    #[cfg(target_os = "macos")]
    fn read_bsd_flags_path_blocking(&self, relative: &Path, kind: EntryKind) -> Result<u32> {
        use std::os::macos::fs::MetadataExt;

        let file = self.open_xattr_entry_blocking(relative, kind)?;
        Ok(file.metadata()?.st_flags())
    }

    /// macOS: `fchflags` on the held no-follow leaf descriptor replaces the
    /// whole flag word (0 clears), so the mirror is a single confined
    /// syscall with no path lookup at all.
    #[cfg(target_os = "macos")]
    fn write_bsd_flags_path_blocking(
        &self,
        relative: &Path,
        kind: EntryKind,
        flags: u32,
    ) -> Result<()> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        // SAFETY: `file` is a live held descriptor; `fchflags` only mutates
        // flags on the open file description.
        let ret = unsafe { libc::fchflags(file.as_raw_fd(), flags) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn read_bsd_flags_path_blocking(&self, _relative: &Path, _kind: EntryKind) -> Result<u32> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(not(target_os = "macos"))]
    fn write_bsd_flags_path_blocking(
        &self,
        _relative: &Path,
        _kind: EntryKind,
        _flags: u32,
    ) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    /// Open one file or directory leaf for descriptor-based attribute access.
    /// The parent is resolved without following peer-controlled symlinks and
    /// the leaf is opened without following a symlink, so both reading and
    /// writing attributes stay confined to the held root.
    #[cfg(unix)]
    fn open_xattr_entry_blocking(&self, relative: &Path, kind: EntryKind) -> Result<File> {
        let (parent, leaf) = self.open_parent_blocking(relative)?;
        let file = match kind {
            EntryKind::File => {
                let file = open_file_at(parent.as_raw_fd(), &leaf)?;
                if !file.metadata()?.file_type().is_file() {
                    return Err(RootedFsError::NotRegularFile(relative.to_path_buf()));
                }
                file
            }
            EntryKind::Directory => File::from(open_dir_at(parent.as_raw_fd(), &leaf)?),
            EntryKind::Symlink => return Err(RootedFsError::UnsupportedSymlinkXattrs),
        };
        Ok(file)
    }

    #[cfg(unix)]
    fn open_parent_blocking(&self, relative: &Path) -> Result<(OwnedFd, OsString)> {
        let mut components = relative.components().peekable();
        if components.peek().is_none() {
            return Err(RootedFsError::InvalidRelativePath);
        }

        let mut current_fd = self.root_fd.try_clone()?;
        let mut leaf = None;
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            if components.peek().is_some() {
                current_fd = open_dir_at(current_fd.as_raw_fd(), name)?;
            } else {
                leaf = Some(name.to_os_string());
            }
        }

        let leaf = leaf.ok_or(RootedFsError::InvalidRelativePath)?;
        Ok((current_fd, leaf))
    }
}

#[cfg(unix)]
fn next_temp_name() -> OsString {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!(".sy-stage-{}-{id}", std::process::id()))
}

#[cfg(unix)]
fn component_cstring(component: &OsStr) -> Result<CString> {
    CString::new(component.as_bytes()).map_err(|_| RootedFsError::PathContainsNul)
}

#[cfg(unix)]
fn open_dir_at(parent: RawFd, component: &OsStr) -> Result<OwnedFd> {
    let component = component_cstring(component)?;
    let fd = unsafe {
        // SAFETY: `parent` remains open for this call and `component` is a live
        // NUL-terminated single component. O_NOFOLLOW prevents a raced parent
        // symlink from redirecting traversal.
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe {
        // SAFETY: successful `openat` returned a fresh owned descriptor.
        OwnedFd::from_raw_fd(fd)
    })
}

#[cfg(unix)]
fn create_private_staging_dir_at(parent: RawFd, component: &OsStr) -> Result<OwnedFd> {
    let name = component_cstring(component)?;
    let result = unsafe {
        // SAFETY: `parent` is a held directory descriptor and `name` is a live
        // single component. The directory is created beneath that descriptor.
        libc::mkdirat(parent, name.as_ptr(), 0o700)
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let created = match stat_at_no_follow(parent, component) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(RootedFsError::StagingPreparationCleanupFailed {
                operation: error.to_string(),
                cleanup: "could not identify the created directory; left its name untouched"
                    .to_string(),
            });
        }
    };
    // Do not mutate or remove a same-name replacement if another actor raced
    // the mkdirat/openat interval.
    // SAFETY: `geteuid` reads effective process credentials without pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if created.st_mode & libc::S_IFMT != libc::S_IFDIR || created.st_uid != effective_uid {
        return Err(RootedFsError::StagingDirectoryChanged);
    }

    let directory = match open_dir_at(parent, component) {
        Ok(directory) => directory,
        Err(error) => {
            return Err(RootedFsError::StagingPreparationCleanupFailed {
                operation: error.to_string(),
                cleanup: "could not hold the created directory; left its name untouched"
                    .to_string(),
            });
        }
    };
    let opened = match stat_fd(directory.as_raw_fd()) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(cleanup_staging_directory_preparation(
                error,
                parent,
                directory.as_raw_fd(),
                component,
            ));
        }
    };
    if opened.st_dev != created.st_dev || opened.st_ino != created.st_ino {
        return Err(RootedFsError::StagingDirectoryChanged);
    }
    if let Err(error) = make_staging_directory_private(parent, directory.as_raw_fd()) {
        return match remove_owned_staging_dir_at(parent, directory.as_raw_fd(), component) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(RootedFsError::StagingPreparationCleanupFailed {
                operation: error.to_string(),
                cleanup: cleanup.to_string(),
            }),
        };
    }
    Ok(directory)
}

#[cfg(unix)]
fn cleanup_staging_directory_preparation(
    operation: RootedFsError,
    parent: RawFd,
    staging_dir: RawFd,
    staging_dir_name: &OsStr,
) -> RootedFsError {
    match remove_owned_staging_dir_at(parent, staging_dir, staging_dir_name) {
        Ok(()) => operation,
        Err(cleanup) => RootedFsError::StagingPreparationCleanupFailed {
            operation: operation.to_string(),
            cleanup: cleanup.to_string(),
        },
    }
}

#[cfg(unix)]
fn cleanup_staging_file_preparation(
    operation: RootedFsError,
    parent: RawFd,
    staging_dir: RawFd,
    temp_name: &OsStr,
    staging_dir_name: &OsStr,
) -> RootedFsError {
    let remove_file = unlink_at(staging_dir, temp_name, false).err();
    let remove_dir = remove_owned_staging_dir_at(parent, staging_dir, staging_dir_name).err();
    let cleanup = remove_file
        .into_iter()
        .chain(remove_dir)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if cleanup.is_empty() {
        operation
    } else {
        RootedFsError::StagingPreparationCleanupFailed {
            operation: operation.to_string(),
            cleanup: cleanup.join("; "),
        }
    }
}

#[cfg(unix)]
fn verify_staging_file_group(parent: RawFd, file: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let parent = stat_fd(parent)?;
    if parent.st_mode & libc::S_ISGID != 0 && file.metadata()?.gid() != parent.st_gid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "staged file did not inherit its setgid destination group",
        )
        .into());
    }
    Ok(())
}

#[cfg(unix)]
fn capture_destination_expectation(
    parent: RawFd,
    component: &OsStr,
    relative: &Path,
    expected: ExpectedDestination,
) -> Result<HeldDestinationExpectation> {
    if matches!(expected, ExpectedDestination::Unverified) {
        return Ok(HeldDestinationExpectation::Unverified);
    }
    let observed = stat_at_optional(parent, component)?;
    let observed_identity = observed.as_ref().and_then(identity_from_stat);
    let changed = || RootedFsError::DestinationChanged(relative.to_path_buf());

    match expected {
        ExpectedDestination::Absent if observed.is_none() => Ok(HeldDestinationExpectation::Absent),
        ExpectedDestination::Absent => Err(changed()),
        ExpectedDestination::Unchanged(expected) if observed_identity == Some(expected) => {
            Ok(HeldDestinationExpectation::Unchanged(expected))
        }
        ExpectedDestination::Unchanged(_) => Err(changed()),
        ExpectedDestination::SnapshotAtOpen => match observed {
            None => Ok(HeldDestinationExpectation::Absent),
            Some(_) => observed_identity
                .map(HeldDestinationExpectation::Unchanged)
                .ok_or_else(changed),
        },
        ExpectedDestination::Unverified => Ok(HeldDestinationExpectation::Unverified),
    }
}

#[cfg(unix)]
fn stat_at_optional(parent: RawFd, component: &OsStr) -> Result<Option<libc::stat>> {
    let component = component_cstring(component)?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        // SAFETY: `parent` is held, `component` is a live single component,
        // and AT_SYMLINK_NOFOLLOW inspects the entry without following it.
        libc::fstatat(
            parent,
            component.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(unsafe {
        // SAFETY: successful fstatat initialized the complete stat structure.
        metadata.assume_init()
    }))
}

#[cfg(unix)]
fn identity_from_stat(metadata: &libc::stat) -> Option<EntryIdentity> {
    let kind = match metadata.st_mode & libc::S_IFMT {
        libc::S_IFDIR => EntryKind::Directory,
        libc::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::File,
    };
    crate::endpoint::local_identity::stat_identity(metadata, kind)
}

#[cfg(unix)]
fn stat_at_no_follow(parent: RawFd, component: &OsStr) -> Result<libc::stat> {
    let component = component_cstring(component)?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        // SAFETY: `parent` is held, `component` is a live single component,
        // and AT_SYMLINK_NOFOLLOW inspects the entry without following it.
        libc::fstatat(
            parent,
            component.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe {
        // SAFETY: successful fstatat initialized the complete stat structure.
        metadata.assume_init()
    })
}

#[cfg(unix)]
fn verify_root_path_identity(path: &Path, fd: RawFd) -> Result<()> {
    let held = stat_fd(fd)?;
    verify_root_path_identity_values(path, held.st_dev, held.st_ino)
}

#[cfg(unix)]
fn verify_root_path_identity_values(
    path: &Path,
    device: libc::dev_t,
    inode: libc::ino_t,
) -> Result<()> {
    let path_c =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| RootedFsError::PathContainsNul)?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        // SAFETY: `path_c` is a live NUL-terminated path and `metadata` is
        // writable storage for one complete libc::stat value.
        libc::stat(path_c.as_ptr(), metadata.as_mut_ptr())
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Err(RootedFsError::RootChanged(path.to_path_buf()));
        }
        return Err(error.into());
    }
    let metadata = unsafe {
        // SAFETY: successful stat initialized the complete metadata structure.
        metadata.assume_init()
    };
    if metadata.st_dev == device && metadata.st_ino == inode {
        Ok(())
    } else {
        Err(RootedFsError::RootChanged(path.to_path_buf()))
    }
}

#[cfg(unix)]
fn stat_fd(fd: RawFd) -> Result<libc::stat> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        // SAFETY: `fd` is live and `metadata` is writable stat storage.
        libc::fstat(fd, metadata.as_mut_ptr())
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe {
        // SAFETY: successful fstat initialized the complete stat structure.
        metadata.assume_init()
    })
}

#[cfg(unix)]
fn make_staging_directory_private(parent: RawFd, fd: RawFd) -> Result<()> {
    let initial = stat_fd(fd)?;
    let parent_metadata = stat_fd(parent)?;
    // SAFETY: `geteuid` reads effective process credentials without pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if initial.st_uid != effective_uid || initial.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(RootedFsError::StagingDirectoryChanged);
    }

    // macOS inherited allow ACEs are not constrained by POSIX mode bits.
    // Linux's inherited access-ACL mask is bounded by the 0700 mkdir mode, but
    // a copied default ACL would be inherited by the staged file and could be
    // widened later by its final chmod. Clear it before creating file content.
    // mkdirat applies the requested owner-only mode while inheriting the
    // filesystem's setgid/group policy. Avoid chmod: nonmembers can lose the
    // setgid bit even when they are allowed to create entries in that parent.
    let expected_mode = 0o700 | (initial.st_mode & libc::S_ISGID);
    if initial.st_mode & 0o7777 != expected_mode
        || (parent_metadata.st_mode & libc::S_ISGID != 0
            && initial.st_gid != parent_metadata.st_gid)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "staging directory did not retain private permissions and inherited group ownership",
        )
        .into());
    }

    #[cfg(target_os = "linux")]
    clear_linux_default_acl(fd)?;
    #[cfg(target_os = "macos")]
    clear_macos_acl(fd)?;

    let secured = stat_fd(fd)?;
    if secured.st_mode & 0o7777 != expected_mode
        || secured.st_uid != effective_uid
        || secured.st_gid != initial.st_gid
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "staging directory permissions or ownership changed during ACL isolation",
        )
        .into());
    }
    Ok(())
}

#[cfg(unix)]
fn remove_owned_staging_dir_at(parent: RawFd, staging_dir: RawFd, component: &OsStr) -> Result<()> {
    let mut held = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        // SAFETY: `staging_dir` is the held directory descriptor and `held` is
        // writable storage for fstat's complete result.
        libc::fstat(staging_dir, held.as_mut_ptr())
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let held = unsafe {
        // SAFETY: successful fstat initialized the complete stat structure.
        held.assume_init()
    };
    let Some(named) = stat_at_optional(parent, component)? else {
        return if held.st_nlink == 0 {
            Ok(())
        } else {
            Err(RootedFsError::StagingDirectoryChanged)
        };
    };
    if named.st_dev != held.st_dev
        || named.st_ino != held.st_ino
        || named.st_mode & libc::S_IFMT != libc::S_IFDIR
    {
        return Err(RootedFsError::StagingDirectoryChanged);
    }

    // A hostile concurrent rename can still race this final identity check;
    // portable unlinkat has no inode-compare-and-delete operation.
    match unlink_at(parent, component, true) {
        Ok(()) => Ok(()),
        Err(RootedFsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let after = stat_fd(staging_dir)?;
            if after.st_nlink == 0 {
                Ok(())
            } else {
                Err(RootedFsError::StagingDirectoryChanged)
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn clear_linux_default_acl(fd: RawFd) -> Result<()> {
    let name =
        CString::new("system.posix_acl_default").map_err(|_| RootedFsError::PathContainsNul)?;
    let result = unsafe {
        // SAFETY: `fd` is the live staging-directory descriptor and `name` is
        // a NUL-terminated xattr name valid for the duration of this syscall.
        libc::fremovexattr(fd, name.as_ptr())
    };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    let code = error.raw_os_error();
    if code == Some(libc::ENODATA) || code == Some(libc::ENOTSUP) || code == Some(libc::EOPNOTSUPP)
    {
        Ok(())
    } else {
        Err(error.into())
    }
}

#[cfg(target_os = "macos")]
fn clear_macos_acl(fd: RawFd) -> Result<()> {
    use std::ffi::c_void;

    unsafe extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut c_void;
        fn acl_init(count: libc::c_int) -> *mut c_void;
        fn acl_set_fd(fd: libc::c_int, acl: *mut c_void) -> libc::c_int;
        fn acl_free(object: *mut c_void) -> libc::c_int;
    }

    let inherited = unsafe {
        // SAFETY: `fd` is the live staging directory descriptor.
        acl_get_fd(fd)
    };
    if inherited.is_null() {
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error();
        if code == Some(libc::ENOENT)
            || code == Some(libc::ENOTSUP)
            || code == Some(libc::EOPNOTSUPP)
        {
            // No extended ACL is present or supported; owner-only mode remains
            // the complete access-control mechanism on this filesystem.
            return Ok(());
        }
        return Err(error.into());
    }
    unsafe {
        // SAFETY: `inherited` was returned by `acl_get_fd` and is freed exactly once.
        acl_free(inherited);
    }

    let acl = unsafe {
        // SAFETY: `acl_init(0)` allocates an empty ACL object owned by this call.
        acl_init(0)
    };
    if acl.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = unsafe {
        // SAFETY: `fd` is the live staging directory descriptor and `acl` is a
        // valid empty ACL; the call replaces inherited ACEs on this directory.
        acl_set_fd(fd, acl)
    };
    let error = (result != 0).then(std::io::Error::last_os_error);
    unsafe {
        // SAFETY: `acl` was returned by `acl_init` and is freed exactly once.
        acl_free(acl);
    }
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}

/// Alias an already-open leaf as a `/proc/self/fd/N` path.
///
/// Linux resolves this magic link to the open file description for
/// `acl_get_file`/`acl_set_file`, which lets exacl operate on the held
/// no-follow descriptor without any path-based lookup. The caller must keep
/// `file` alive for the whole exacl call.
#[cfg(all(target_os = "linux", feature = "acl"))]
fn fd_alias_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Replace one held descriptor's ACL from exacl-unified text.
///
/// Both the rooted entry writers and the staged-writer preservation path
/// apply through this fd-based core: no path-based lookup can race the
/// descriptor. Empty text restores the mode-derived base entries on Linux.
#[cfg(all(target_os = "linux", feature = "acl"))]
fn apply_acl_fd(file: &File, acl: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Mirror `LocalEndpoint::write_acl`: empty text restores the
    // mode-derived base entries instead of leaving a bare ACL.
    let entries = if acl.is_empty() {
        let mode = file.metadata()?.permissions().mode();
        exacl::from_mode(mode & 0o777)
    } else {
        exacl::from_str(acl)?
    };
    exacl::setfacl(&[fd_alias_path(file)], &entries, None)?;
    Ok(())
}

/// macOS: `/dev/fd/N` does NOT resolve to the open inode for exacl's path
/// API (verified: it returns an empty list), so the fd-based syscalls in
/// `acl_macos` carry the conversion instead. Same held descriptor, same
/// exacl text format.
#[cfg(all(target_os = "macos", feature = "acl"))]
fn apply_acl_fd(file: &File, acl: &str) -> Result<()> {
    let entries = exacl::from_str(acl)?;
    acl_macos::write_fd_entries(file.as_raw_fd(), &entries)?;
    Ok(())
}

#[cfg(all(
    unix,
    feature = "acl",
    not(target_os = "linux"),
    not(target_os = "macos")
))]
fn apply_acl_fd(_file: &File, _acl: &str) -> Result<()> {
    Err(RootedFsError::AclUnsupported(
        "access control lists are only supported on Linux and macOS",
    ))
}

#[cfg(unix)]
fn open_file_at(parent: RawFd, component: &OsStr) -> Result<File> {
    let component = component_cstring(component)?;
    let fd = unsafe {
        // SAFETY: `parent` remains open for this call and `component` is a live
        // NUL-terminated single component. O_NOFOLLOW prevents a raced leaf
        // symlink from redirecting the read.
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let owned = unsafe {
        // SAFETY: successful `openat` returned a fresh owned descriptor.
        OwnedFd::from_raw_fd(fd)
    };
    Ok(File::from(owned))
}

#[cfg(unix)]
fn create_staging_file_at(parent: RawFd, component: &OsStr) -> Result<File> {
    let component = component_cstring(component)?;
    let fd = unsafe {
        // SAFETY: `parent` remains open for this call and `component` is a live
        // NUL-terminated single component. O_EXCL prevents reuse of an attacker-
        // supplied leaf, while O_NOFOLLOW is defense in depth for that invariant.
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let owned = unsafe {
        // SAFETY: successful `openat` returned a fresh owned descriptor.
        OwnedFd::from_raw_fd(fd)
    };
    Ok(File::from(owned))
}

#[cfg(unix)]
fn apply_fd_metadata(fd: RawFd, unix_mode: Option<u32>, modified: Option<Timestamp>) -> Result<()> {
    if let Some(mode) = unix_mode {
        let result = unsafe {
            // SAFETY: `fd` is live for this call. chmod ignores file-type bits;
            // explicitly retaining only permission/special bits makes the wire
            // contract independent of platform-specific stat type constants.
            libc::fchmod(fd, (mode & 0o7777) as libc::mode_t)
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    if let Some(modified) = modified {
        let times = modified_timespecs(modified)?;
        let result = unsafe {
            // SAFETY: `fd` is live and `times` contains exactly two initialized
            // timespec values. UTIME_OMIT preserves the existing access time.
            libc::futimens(fd, times.as_ptr())
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_symlink_at(parent: RawFd, component: &OsStr, relative: &Path) -> Result<()> {
    let component = component_cstring(component)?;
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    let result = unsafe {
        // SAFETY: `parent` is live, `component` is a live single component, and
        // `stat` points to writable storage for one libc::stat value.
        libc::fstatat(
            parent,
            component.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe {
        // SAFETY: successful fstatat initialized the stat buffer.
        stat.assume_init()
    };
    if stat.st_mode & libc::S_IFMT != libc::S_IFLNK {
        return Err(RootedFsError::EntryKindMismatch {
            path: relative.to_path_buf(),
            expected: EntryKind::Symlink,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn set_symlink_mtime_at(parent: RawFd, component: &OsStr, modified: Timestamp) -> Result<()> {
    let component = component_cstring(component)?;
    let times = modified_timespecs(modified)?;
    let result = unsafe {
        // SAFETY: `parent` is live, `component` is one live relative leaf, and
        // AT_SYMLINK_NOFOLLOW applies the timestamp to the link itself.
        libc::utimensat(
            parent,
            component.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn modified_timespecs(modified: Timestamp) -> Result<[libc::timespec; 2]> {
    let seconds = libc::time_t::try_from(modified.seconds())
        .map_err(|_| RootedFsError::TimestampOutOfRange)?;
    Ok([
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT as _,
        },
        libc::timespec {
            tv_sec: seconds,
            tv_nsec: modified.nanoseconds().into(),
        },
    ])
}

#[cfg(unix)]
fn link_at(
    source_parent: RawFd,
    source_leaf: &OsStr,
    dest_parent: RawFd,
    dest_leaf: &OsStr,
) -> Result<()> {
    let source = component_cstring(source_leaf)?;
    let dest = component_cstring(dest_leaf)?;
    let result = unsafe {
        // SAFETY: both descriptors remain open and both names are live
        // NUL-terminated single components. Flags are zero so a symlink
        // source is refused rather than followed.
        libc::linkat(
            source_parent,
            source.as_ptr(),
            dest_parent,
            dest.as_ptr(),
            0,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn rename_at(parent: RawFd, from: &OsStr, to: &OsStr) -> Result<()> {
    rename_between_at(parent, from, parent, to)
}

#[cfg(unix)]
fn rename_between_at(
    source_parent: RawFd,
    source: &OsStr,
    destination_parent: RawFd,
    destination: &OsStr,
) -> Result<()> {
    let source = component_cstring(source)?;
    let destination = component_cstring(destination)?;
    let result = unsafe {
        // SAFETY: both directory descriptors remain open and both names are
        // live NUL-terminated single components. renameat moves only the leaf
        // between these held parents and cannot follow a symlink component.
        libc::renameat(
            source_parent,
            source.as_ptr(),
            destination_parent,
            destination.as_ptr(),
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_exchange_at(
    source_parent: RawFd,
    source: &OsStr,
    destination_parent: RawFd,
    destination: &OsStr,
) -> Result<()> {
    let source = component_cstring(source)?;
    let destination = component_cstring(destination)?;
    const RENAME_EXCHANGE: libc::c_uint = 2;
    let result = unsafe {
        // SAFETY: both directory descriptors remain open and both names are
        // live NUL-terminated single components. The Linux renameat2 syscall with
        // RENAME_EXCHANGE atomically swaps the two entries without following symlinks.
        libc::syscall(
            libc::SYS_renameat2,
            source_parent,
            source.as_ptr(),
            destination_parent,
            destination.as_ptr(),
            RENAME_EXCHANGE,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_exchange_at(
    source_parent: RawFd,
    source: &OsStr,
    destination_parent: RawFd,
    destination: &OsStr,
) -> Result<()> {
    let source = component_cstring(source)?;
    let destination = component_cstring(destination)?;
    const RENAME_SWAP: libc::c_uint = 2;
    extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    let result = unsafe {
        // SAFETY: both directory descriptors remain open and both names are
        // live NUL-terminated single components. renameatx_np with RENAME_SWAP
        // atomically swaps the two filesystem entries without following symlinks.
        renameatx_np(
            source_parent,
            source.as_ptr(),
            destination_parent,
            destination.as_ptr(),
            RENAME_SWAP,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn rename_exchange_at(
    _source_parent: RawFd,
    _source: &OsStr,
    _destination_parent: RawFd,
    _destination: &OsStr,
) -> Result<()> {
    Err(RootedFsError::UnsupportedPlatform)
}

#[cfg(unix)]
fn remove_dir_tree_at(parent: RawFd, component: &OsStr) -> Result<()> {
    let stat = match stat_at_optional(parent, component)? {
        Some(s) => s,
        None => return Ok(()),
    };

    let is_dir = (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR;
    if !is_dir {
        return unlink_at(parent, component, false);
    }

    let dir_fd = open_dir_at(parent, component)?;
    let _ = unsafe {
        // SAFETY: dir_fd is a valid open file descriptor pointing to the directory in private staging.
        libc::fchmod(dir_fd.as_raw_fd(), 0o700)
    };

    let child_names = scan::directory_names(dir_fd.as_raw_fd())
        .map_err(|err| std::io::Error::other(err.to_string()))?;

    for child in child_names {
        remove_dir_tree_at(dir_fd.as_raw_fd(), &child)?;
    }

    unlink_at(parent, component, true)
}

#[cfg(unix)]
fn unlink_at(parent: RawFd, component: &OsStr, is_directory: bool) -> Result<()> {
    let component = component_cstring(component)?;
    let flags = if is_directory { libc::AT_REMOVEDIR } else { 0 };
    let result = unsafe {
        // SAFETY: `parent` remains open and `component` is one live
        // NUL-terminated leaf. unlinkat removes the directory entry itself and
        // does not follow a symlink leaf.
        libc::unlinkat(parent, component.as_ptr(), flags)
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;

    fn relative(path: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(path)).unwrap()
    }

    /// Keep only the caller's attribute namespace. macOS attaches
    /// `com.apple.provenance` to freshly written files, so tests must not
    /// assume the file has exactly the attributes they set.
    fn user_xattrs(xattrs: Vec<(OsString, Vec<u8>)>) -> Vec<(OsString, Vec<u8>)> {
        xattrs
            .into_iter()
            .filter(|(name, _)| name.to_string_lossy().starts_with("user."))
            .collect()
    }

    #[tokio::test]
    async fn opens_nested_regular_file_beneath_held_root() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"inside").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        let mut file = rooted.open_regular_blocking(&relative("dir/file")).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "inside");
    }

    #[tokio::test]
    async fn refuses_parent_symlink_escape() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        assert!(rooted
            .open_regular_blocking(&relative("escape/secret"))
            .is_err());
    }

    #[tokio::test]
    async fn refuses_symlink_leaf() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("leaf"))
            .unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        assert!(rooted.open_regular_blocking(&relative("leaf")).is_err());
    }

    #[tokio::test]
    async fn staged_file_is_invisible_until_atomic_commit() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        let mut staged = rooted
            .begin_staged_file_blocking(&relative("dir/file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();
        assert_eq!(std::fs::read(root.path().join("dir/file")).unwrap(), b"old");
        staged.commit().unwrap();
        assert_eq!(std::fs::read(root.path().join("dir/file")).unwrap(), b"new");
    }

    #[cfg(all(target_os = "linux", feature = "acl"))]
    #[tokio::test]
    async fn inherited_linux_default_acl_is_not_published_with_staged_file() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let mut inherited = exacl::from_mode(0o777);
        inherited.push(exacl::AclEntry::allow_user(
            "2147483646",
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        exacl::setfacl(
            &[root.path()],
            &inherited,
            Some(exacl::AclOption::DEFAULT_ACL),
        )
        .unwrap();

        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();
        staged.apply_metadata_blocking(Some(0o644), None).unwrap();

        let staging_path = root.path().join(&staged.staging_dir_name);
        let staged_acl =
            exacl::to_string(&exacl::getfacl(staging_path.join(&staged.temp_name), None).unwrap())
                .unwrap();
        assert!(!staged_acl.contains("2147483646"));
        staged.commit().unwrap();

        let committed_acl =
            exacl::to_string(&exacl::getfacl(root.path().join("file"), None).unwrap()).unwrap();
        assert!(!committed_acl.contains("2147483646"));
    }

    #[tokio::test]
    async fn staged_commit_rejects_destination_replacement_through_held_parent() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (_, identity) = rooted
            .path_identity_blocking(&relative("file"))
            .unwrap()
            .unwrap();
        let mut staged = rooted
            .begin_staged_file_with_expectation_blocking(
                &relative("file"),
                ExpectedDestination::Unchanged(identity),
            )
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();

        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"raced").unwrap();
        std::fs::rename(replacement, root.path().join("file")).unwrap();

        assert!(matches!(
            staged.commit(),
            Err(RootedFsError::DestinationChanged(_))
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"raced");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn staged_commit_refuses_parent_moved_outside_root() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("dir/file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();

        let moved_parent = outside.path().join("moved-dir");
        std::fs::rename(root.path().join("dir"), &moved_parent).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"replacement").unwrap();

        assert!(matches!(
            staged.commit(),
            Err(RootedFsError::DestinationChanged(_))
        ));
        assert_eq!(std::fs::read(moved_parent.join("file")).unwrap(), b"old");
        assert_eq!(
            std::fs::read(root.path().join("dir/file")).unwrap(),
            b"replacement"
        );
        assert_eq!(std::fs::read_dir(&moved_parent).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn concurrent_parent_creation_reopens_winning_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let rooted = Arc::new(RootedFs::open(root.path().to_path_buf()).await.unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let rooted = Arc::clone(&rooted);
            let barrier = Arc::clone(&barrier);
            workers.push(tokio::task::spawn_blocking(move || {
                barrier.wait();
                rooted.create_directories_blocking(&relative("shared/nested"))
            }));
        }
        for worker in workers {
            worker.await.unwrap().unwrap();
        }
        assert!(root.path().join("shared/nested").is_dir());
    }

    #[tokio::test]
    async fn failed_commit_reports_unowned_staging_cleanup_pending() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (_, identity) = rooted
            .path_identity_blocking(&relative("file"))
            .unwrap()
            .unwrap();
        let mut staged = rooted
            .begin_staged_file_with_expectation_blocking(
                &relative("file"),
                ExpectedDestination::Unchanged(identity),
            )
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();

        let staging_path = root.path().join(&staged.staging_dir_name);
        let moved_path = root.path().join("moved-stage");
        std::fs::rename(&staging_path, &moved_path).unwrap();
        std::fs::create_dir(&staging_path).unwrap();
        std::fs::write(root.path().join("replacement"), b"raced").unwrap();
        std::fs::rename(root.path().join("replacement"), root.path().join("file")).unwrap();

        assert!(matches!(
            staged.commit(),
            Err(RootedFsError::StagingOperationAbortFailed { .. })
        ));
        assert!(staging_path.is_dir());
        assert!(moved_path.is_dir());
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"raced");
    }

    #[tokio::test]
    async fn staged_abort_reports_relocated_private_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        let relocated = root.path().join("relocated-stage");
        std::fs::rename(root.path().join(&staged.staging_dir_name), &relocated).unwrap();

        assert!(matches!(
            staged.abort(),
            Err(RootedFsError::StagingAbortFailed {
                directory: Some(_),
                ..
            })
        ));
        assert!(relocated.is_dir());
        std::fs::remove_dir(relocated).unwrap();
    }

    #[tokio::test]
    async fn staged_abort_reports_success_after_owned_cleanup() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();

        staged.abort().unwrap();
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cancelled_cancellable_commit_does_not_publish() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();
        let cancellation = Arc::new(std::sync::Mutex::new(true));

        assert!(matches!(
            staged.commit_cancellable(cancellation),
            Err(RootedFsError::CommitCancelled)
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn permissive_staged_mode_remains_behind_private_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();
        staged.apply_metadata_blocking(Some(0o777), None).unwrap();

        let staging_path = root.path().join(&staged.staging_dir_name);
        assert_eq!(
            std::fs::metadata(&staging_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(staging_path.join(&staged.temp_name))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o777
        );
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"old");

        staged.commit().unwrap();
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"new");
        assert!(!staging_path.exists());
    }

    #[cfg(all(target_os = "macos", feature = "acl"))]
    #[tokio::test]
    async fn inherited_macos_acl_is_cleared_on_private_staging_directory() {
        let root = tempfile::TempDir::new().unwrap();
        // SAFETY: `getuid` reads process credentials and has no pointer arguments.
        let uid = unsafe { libc::getuid() };
        let inherited = exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ | exacl::Perm::EXECUTE | exacl::Perm::READATTR,
            exacl::Flag::FILE_INHERIT | exacl::Flag::DIRECTORY_INHERIT,
        );
        exacl::setfacl(&[root.path()], &[inherited], None).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();

        let directory_acl = acl_macos::read_fd_entries(staged.staging_dir_fd.as_raw_fd()).unwrap();
        let file_acl = acl_macos::read_fd_entries(staged.file.as_raw_fd()).unwrap();
        assert!(directory_acl.is_empty());
        assert!(file_acl.is_empty());
        drop(staged);
    }

    #[tokio::test]
    async fn staging_preserves_setgid_parent_group_inheritance() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = tempfile::TempDir::new().unwrap();
        let parent = root.path().join("shared");
        std::fs::create_dir(&parent).unwrap();
        if let Some(group) = supplementary_group_other_than_effective() {
            std::os::unix::fs::chown(&parent, None, Some(group)).unwrap();
        }
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o2770)).unwrap();
        let parent_metadata = std::fs::metadata(&parent).unwrap();
        let parent_gid = parent_metadata.gid();
        let _parent_setgid = parent_metadata.mode() & 0o2000;
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("shared/file"))
            .unwrap();
        let staging_path = parent.join(&staged.staging_dir_name);

        let staging_mode = std::fs::metadata(&staging_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(staging_mode & 0o700, 0o700);
        assert_eq!(staging_mode & 0o077, 0);
        #[cfg(target_os = "linux")]
        assert_eq!(staging_mode & 0o2000, _parent_setgid);
        assert_eq!(std::fs::metadata(&staging_path).unwrap().gid(), parent_gid);
        assert_eq!(staged.file.metadata().unwrap().gid(), parent_gid);
        staged.file_mut().write_all(b"new").unwrap();
        staged.commit().unwrap();
        assert_eq!(
            std::fs::metadata(parent.join("file")).unwrap().gid(),
            parent_gid
        );
    }

    fn supplementary_group_other_than_effective() -> Option<libc::gid_t> {
        // SAFETY: a zero-sized getgroups query has no output pointer and only
        // asks for the number of supplementary groups.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if count <= 0 {
            return None;
        }
        let mut groups = vec![0; count as usize];
        // SAFETY: `groups` has `count` writable gid_t slots, matching the size.
        let written = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        if written < 0 {
            return None;
        }
        groups.truncate(written as usize);
        // SAFETY: getegid reads process credentials and has no pointer arguments.
        let effective = unsafe { libc::getegid() };
        groups.into_iter().find(|group| *group != effective)
    }

    #[tokio::test]
    async fn staging_cleanup_never_removes_replacement_directory() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();
        let staging_path = root.path().join(&staged.staging_dir_name);
        let moved_path = root.path().join("moved-private-stage");
        std::fs::rename(&staging_path, &moved_path).unwrap();
        std::fs::create_dir(&staging_path).unwrap();

        assert!(matches!(
            staged.commit(),
            Err(RootedFsError::CommittedCleanupPending { .. })
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"new");
        assert!(staging_path.is_dir());
        assert_eq!(std::fs::read_dir(&staging_path).unwrap().count(), 0);
        assert!(moved_path.is_dir());
        assert_eq!(std::fs::read_dir(moved_path).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn staged_metadata_is_applied_before_atomic_commit() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let modified = Timestamp::new(1_600_000_000, 123_456_789).unwrap();

        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();
        staged
            .apply_metadata_blocking(Some(0), Some(modified))
            .unwrap();
        assert_eq!(staged.staged_hash_blocking().unwrap(), blake3::hash(b"new"));
        staged.commit().unwrap();

        let metadata = std::fs::metadata(root.path().join("file")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0);
        assert_eq!(metadata.mtime(), modified.seconds());
        assert_eq!(metadata.mtime_nsec(), i64::from(modified.nanoseconds()));
    }

    #[tokio::test]
    async fn committed_stage_cleanup_failure_is_not_reported_as_rollback() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"new").unwrap();
        let staging_path = root.path().join(&staged.staging_dir_name);
        std::fs::write(staging_path.join("unexpected"), b"keep").unwrap();

        let result = staged.commit();
        assert!(matches!(
            result,
            Err(RootedFsError::CommittedCleanupPending { .. })
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(staging_path.join("unexpected")).unwrap(),
            b"keep"
        );
    }

    #[tokio::test]
    async fn hardlink_shares_inode_and_replaces_atomically() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("first"), b"shared").unwrap();
        std::fs::write(root.path().join("second"), b"replaced").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        rooted
            .create_hardlink_blocking(&relative("first"), &relative("linked"))
            .unwrap();
        let first = std::fs::metadata(root.path().join("first")).unwrap();
        let linked = std::fs::metadata(root.path().join("linked")).unwrap();
        assert_eq!(first.ino(), linked.ino());
        assert_eq!(
            std::fs::read(root.path().join("linked")).unwrap(),
            b"shared"
        );

        rooted
            .create_hardlink_blocking(&relative("first"), &relative("second"))
            .unwrap();
        let second = std::fs::metadata(root.path().join("second")).unwrap();
        assert_eq!(first.ino(), second.ino());
        assert_eq!(
            std::fs::read(root.path().join("second")).unwrap(),
            b"shared"
        );
    }

    #[tokio::test]
    async fn hardlink_refuses_symlink_source_and_parent_escape() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        std::fs::write(root.path().join("real"), b"real").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("leaf"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        assert!(rooted
            .create_hardlink_blocking(&relative("leaf"), &relative("linked"))
            .is_err());
        assert!(rooted
            .create_hardlink_blocking(&relative("escape/secret"), &relative("linked"))
            .is_err());
        assert!(!root.path().join("linked").exists());
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"outside"
        );
    }

    #[tokio::test]
    async fn rooted_metadata_updates_file_directory_and_symlink_without_following() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(outside.path().join("target"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path().join("target"), root.path().join("link"))
            .unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let file_time = Timestamp::new(1_600_000_001, 0).unwrap();
        let dir_time = Timestamp::new(1_600_000_002, 0).unwrap();
        let link_time = Timestamp::new(1_600_000_003, 0).unwrap();
        let outside_before = std::fs::metadata(outside.path().join("target"))
            .unwrap()
            .mtime();

        rooted
            .apply_metadata_blocking(
                &relative("file"),
                EntryKind::File,
                Some(0o600),
                Some(file_time),
            )
            .unwrap();
        rooted
            .apply_metadata_blocking(
                &relative("dir"),
                EntryKind::Directory,
                Some(0o750),
                Some(dir_time),
            )
            .unwrap();
        rooted
            .apply_metadata_blocking(&relative("link"), EntryKind::Symlink, None, Some(link_time))
            .unwrap();

        let file = std::fs::metadata(root.path().join("file")).unwrap();
        assert_eq!(file.mode() & 0o7777, 0o600);
        assert_eq!(file.mtime(), file_time.seconds());
        let dir = std::fs::metadata(root.path().join("dir")).unwrap();
        assert_eq!(dir.mode() & 0o7777, 0o750);
        assert_eq!(dir.mtime(), dir_time.seconds());
        let link = std::fs::symlink_metadata(root.path().join("link")).unwrap();
        assert_eq!(link.mtime(), link_time.seconds());
        assert_eq!(
            std::fs::metadata(outside.path().join("target"))
                .unwrap()
                .mtime(),
            outside_before
        );
    }

    #[tokio::test]
    async fn rooted_metadata_refuses_parent_symlink_escape() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("file"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let before = std::fs::metadata(outside.path().join("file"))
            .unwrap()
            .mode();

        assert!(rooted
            .apply_metadata_blocking(&relative("escape/file"), EntryKind::File, Some(0o600), None,)
            .is_err());
        assert_eq!(
            std::fs::metadata(outside.path().join("file"))
                .unwrap()
                .mode(),
            before
        );
    }

    #[tokio::test]
    async fn rooted_xattrs_round_trip_and_mirror_removes_stale_values() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let name = OsString::from("user.sy-test");

        rooted
            .write_xattrs_blocking(
                &relative("file"),
                EntryKind::File,
                &[(name.clone(), b"one".to_vec())],
            )
            .unwrap();
        assert_eq!(
            user_xattrs(
                rooted
                    .read_xattrs_blocking(&relative("file"), EntryKind::File)
                    .unwrap()
            ),
            vec![(name.clone(), b"one".to_vec())]
        );

        // Mirroring an empty set clears stale destination attributes.
        rooted
            .write_xattrs_blocking(&relative("file"), EntryKind::File, &[])
            .unwrap();
        assert!(user_xattrs(
            rooted
                .read_xattrs_blocking(&relative("file"), EntryKind::File)
                .unwrap()
        )
        .is_empty());

        rooted
            .write_xattrs_blocking(
                &relative("dir"),
                EntryKind::Directory,
                &[(name.clone(), b"dir".to_vec())],
            )
            .unwrap();
        assert_eq!(
            user_xattrs(
                rooted
                    .read_xattrs_blocking(&relative("dir"), EntryKind::Directory)
                    .unwrap()
            ),
            vec![(name, b"dir".to_vec())]
        );
    }

    #[tokio::test]
    async fn open_file_xattrs_stay_bound_after_path_replacement() {
        let root = tempfile::TempDir::new().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"scanned source").unwrap();
        xattr::set(&path, "user.sy-source", b"scanned").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let opened = rooted.open_regular_blocking(&relative("file")).unwrap();

        std::fs::rename(&path, root.path().join("old-file")).unwrap();
        std::fs::write(&path, b"replacement source").unwrap();
        xattr::set(&path, "user.sy-source", b"replacement").unwrap();

        assert_eq!(
            user_xattrs(
                rooted
                    .read_open_file_xattrs_blocking(&opened, &relative("file"))
                    .unwrap()
            ),
            vec![(OsString::from("user.sy-source"), b"scanned".to_vec())]
        );
    }

    #[tokio::test]
    async fn rooted_xattrs_refuse_parent_escape_and_symlink_leaf() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("file"), b"outside").unwrap();
        std::fs::write(root.path().join("real"), b"real").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("file"), root.path().join("leaf")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let name = OsString::from("user.sy-test");

        assert!(rooted
            .read_xattrs_blocking(&relative("escape/file"), EntryKind::File)
            .is_err());
        assert!(rooted
            .read_xattrs_blocking(&relative("leaf"), EntryKind::File)
            .is_err());
        assert!(rooted
            .write_xattrs_blocking(
                &relative("escape/file"),
                EntryKind::File,
                &[(name.clone(), b"escape".to_vec())],
            )
            .is_err());
        assert!(rooted
            .write_xattrs_blocking(
                &relative("leaf"),
                EntryKind::File,
                &[(name, b"escape".to_vec())],
            )
            .is_err());
        assert!(xattr::get(outside.path().join("file"), "user.sy-test")
            .unwrap()
            .is_none());
    }
    #[tokio::test]
    async fn dropped_stage_preserves_destination_and_removes_temp() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        {
            let mut staged = rooted
                .begin_staged_file_blocking(&relative("file"))
                .unwrap();
            staged.file_mut().write_all(b"new").unwrap();
        }

        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn staged_file_refuses_parent_symlink_escape() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        assert!(rooted
            .begin_staged_file_blocking(&relative("escape/file"))
            .is_err());
        assert!(!outside.path().join("file").exists());
    }

    #[tokio::test]
    async fn confined_directory_create_refuses_parent_symlink_escape() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        assert!(rooted
            .create_directory_blocking(&relative("escape/new-dir"))
            .is_err());
        assert!(!outside.path().join("new-dir").exists());
    }

    #[tokio::test]
    async fn confined_symlink_replace_is_atomic_and_preserves_target_bytes() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("entry"), b"old").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        rooted
            .replace_symlink_blocking(&relative("entry"), Path::new("../target"))
            .unwrap();
        let metadata = std::fs::symlink_metadata(root.path().join("entry")).unwrap();
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(root.path().join("entry")).unwrap(),
            Path::new("../target")
        );
    }

    #[tokio::test]
    async fn confined_remove_never_follows_parent_symlink() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("keep"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        assert!(rooted
            .remove_blocking(&relative("escape/keep"), false, None)
            .is_err());
        assert_eq!(
            std::fs::read(outside.path().join("keep")).unwrap(),
            b"outside"
        );
    }

    #[tokio::test]
    async fn confined_remove_handles_filelike_and_directory_leaves() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        rooted
            .remove_blocking(&relative("file"), false, None)
            .unwrap();
        rooted
            .remove_blocking(&relative("dir"), true, None)
            .unwrap();
        assert!(!root.path().join("file").exists());
        assert!(!root.path().join("dir").exists());
    }

    #[tokio::test]
    async fn confined_remove_validates_destination_identity() {
        let root = tempfile::TempDir::new().unwrap();
        let file_path = root.path().join("file");
        let dir_path = root.path().join("dir");
        std::fs::write(&file_path, b"initial content").unwrap();
        std::fs::create_dir(&dir_path).unwrap();

        let file_meta = std::fs::symlink_metadata(&file_path).unwrap();
        let file_id =
            crate::endpoint::local_identity::metadata_identity(&file_meta, EntryKind::File)
                .unwrap();

        let dir_meta = std::fs::symlink_metadata(&dir_path).unwrap();
        let dir_id =
            crate::endpoint::local_identity::metadata_identity(&dir_meta, EntryKind::Directory)
                .unwrap();

        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        // Vanished entry is idempotent success.
        rooted
            .remove_blocking(&relative("nonexistent"), false, Some(file_id))
            .unwrap();

        // Mismatched file identity fails and preserves the file.
        let wrong_id = EntryIdentity::from_bytes([99; 32]);
        let err = rooted
            .remove_blocking(&relative("file"), false, Some(wrong_id))
            .unwrap_err();
        assert!(matches!(err, RootedFsError::DestinationChanged(_)));
        assert!(file_path.exists());

        // Type mismatch (file requested as directory) fails.
        let err = rooted
            .remove_blocking(&relative("file"), true, None)
            .unwrap_err();
        assert!(matches!(err, RootedFsError::DestinationChanged(_)));
        assert!(file_path.exists());

        // Matching file identity succeeds.
        rooted
            .remove_blocking(&relative("file"), false, Some(file_id))
            .unwrap();
        assert!(!file_path.exists());

        // Mismatched directory identity fails and preserves the dir.
        let err = rooted
            .remove_blocking(&relative("dir"), true, Some(wrong_id))
            .unwrap_err();
        assert!(matches!(err, RootedFsError::DestinationChanged(_)));
        assert!(dir_path.exists());

        // Matching directory identity succeeds.
        rooted
            .remove_blocking(&relative("dir"), true, Some(dir_id))
            .unwrap();
        assert!(!dir_path.exists());
    }

    #[tokio::test]
    async fn root_descriptor_remains_pinned_after_path_swap() {
        let parent = tempfile::TempDir::new().unwrap();
        let root_path = parent.path().join("root");
        let moved_path = parent.path().join("moved");
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(root_path.join("file"), b"pinned").unwrap();
        std::fs::write(outside.path().join("file"), b"outside").unwrap();

        let rooted = RootedFs::open(root_path.clone()).await.unwrap();
        std::fs::rename(&root_path, &moved_path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root_path).unwrap();

        let mut file = rooted.open_regular_blocking(&relative("file")).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "pinned");

        let mut staged = rooted
            .begin_staged_file_blocking(&relative("file"))
            .unwrap();
        staged.file_mut().write_all(b"committed").unwrap();
        staged.commit().unwrap();
        assert_eq!(
            std::fs::read(moved_path.join("file")).unwrap(),
            b"committed"
        );
        assert_eq!(
            std::fs::read(outside.path().join("file")).unwrap(),
            b"outside"
        );
    }

    #[tokio::test]
    async fn staged_file_replaces_directory_atomically() {
        let root = tempfile::TempDir::new().unwrap();
        let dir_path = root.path().join("target");
        std::fs::create_dir_all(dir_path.join("nested")).unwrap();
        std::fs::write(dir_path.join("nested/child.txt"), b"old child").unwrap();
        std::fs::write(dir_path.join("file.txt"), b"old file").unwrap();

        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("target"))
            .unwrap();
        staged.file_mut().write_all(b"new file content").unwrap();
        staged.commit().unwrap();

        let meta = std::fs::symlink_metadata(&dir_path).unwrap();
        assert!(meta.is_file());
        assert_eq!(std::fs::read(&dir_path).unwrap(), b"new file content");
        assert!(!dir_path.join("nested").exists());
    }

    #[tokio::test]
    async fn symlink_replaces_directory_atomically() {
        let root = tempfile::TempDir::new().unwrap();
        let dir_path = root.path().join("target");
        std::fs::create_dir_all(dir_path.join("sub")).unwrap();
        std::fs::write(dir_path.join("sub/item"), b"content").unwrap();

        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        rooted
            .replace_symlink_blocking(&relative("target"), Path::new("somewhere/else"))
            .unwrap();

        let meta = std::fs::symlink_metadata(&dir_path).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&dir_path).unwrap(),
            PathBuf::from("somewhere/else")
        );
    }

    #[tokio::test]
    async fn remove_dir_tree_recursively_cleans_nested_directory() {
        let root = tempfile::TempDir::new().unwrap();
        let dir_path = root.path().join("tree");
        std::fs::create_dir_all(dir_path.join("a/b/c")).unwrap();
        std::fs::write(dir_path.join("a/b/c/file"), b"deep").unwrap();
        std::fs::write(dir_path.join("a/file2"), b"shallow").unwrap();

        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        rooted.remove_dir_tree_blocking(&relative("tree")).unwrap();

        assert!(!dir_path.exists());
    }
}
