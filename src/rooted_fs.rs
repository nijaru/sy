#[cfg(unix)]
mod scan;

#[cfg(all(target_os = "macos", feature = "acl"))]
mod acl_macos;

use crate::engine::domain::{EntryIdentity, EntryKind, RelativePath, Timestamp};
use std::ffi::OsString;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
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

    #[error("could not allocate a unique staging file after {0} attempts")]
    StagingNameExhausted(usize),

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

/// Same-directory temporary file whose parent directory is held by descriptor.
///
/// Dropping this value before `commit` removes only the temporary leaf through
/// the held parent descriptor. `commit` fsyncs file contents and atomically
/// renames the temporary leaf over the destination without resolving the
/// destination pathname from the process working directory or root pathname.
pub struct RootedStagedFile {
    file: File,
    #[cfg(unix)]
    parent_fd: OwnedFd,
    #[cfg(unix)]
    temp_name: OsString,
    #[cfg(unix)]
    destination_name: OsString,
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

    pub fn commit(mut self) -> Result<()> {
        self.file.sync_all()?;
        self.commit_blocking()?;
        self.committed = true;
        Ok(())
    }

    #[cfg(unix)]
    fn commit_blocking(&mut self) -> Result<()> {
        rename_at(
            self.parent_fd.as_raw_fd(),
            &self.temp_name,
            &self.destination_name,
        )
    }

    #[cfg(not(unix))]
    fn commit_blocking(&mut self) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
    }
}

impl Drop for RootedStagedFile {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        #[cfg(unix)]
        {
            let _ = unlink_at(self.parent_fd.as_raw_fd(), &self.temp_name, false);
        }
    }
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
        self.begin_staged_path_blocking(relative.as_path())
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
    fn begin_staged_path_blocking(&self, relative: &Path) -> Result<RootedStagedFile> {
        let (parent_fd, destination_name) = self.open_parent_blocking(relative)?;

        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let temp_name = next_temp_name();
            match create_staging_file_at(parent_fd.as_raw_fd(), &temp_name) {
                Ok(file) => {
                    return Ok(RootedStagedFile {
                        file,
                        parent_fd,
                        temp_name,
                        destination_name,
                        committed: false,
                    });
                }
                Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::EEXIST) => {}
                Err(error) => return Err(error),
            }
        }

        Err(RootedFsError::StagingNameExhausted(TEMP_CREATE_ATTEMPTS))
    }

    #[cfg(not(unix))]
    fn begin_staged_path_blocking(&self, _relative: &Path) -> Result<RootedStagedFile> {
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

        let mut staged = self.begin_staged_path_blocking(destination)?;
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
                        return Err(std::io::Error::last_os_error().into());
                    }
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

            match rename_at(parent.as_raw_fd(), &temp_name, &destination_name) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let _ = unlink_at(parent.as_raw_fd(), &temp_name, false);
                    return Err(error);
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
        use xattr::FileExt;

        let file = self.open_xattr_entry_blocking(relative, kind)?;
        let mut xattrs = Vec::new();
        let mut total = 0_usize;
        for name in file.list_xattr()? {
            // A value may be large on filesystems that store it out of line
            // (macOS resource forks). The running total is checked before the
            // value is retained so an oversized set fails loudly.
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
    fn read_acl_path_blocking(&self, relative: &Path, kind: EntryKind) -> Result<Option<String>> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        let entries = exacl::getfacl(fd_alias_path(&file), None)?;
        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(exacl::to_string(&entries)?))
    }

    #[cfg(all(target_os = "linux", feature = "acl"))]
    fn write_acl_path_blocking(&self, relative: &Path, kind: EntryKind, acl: &str) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let file = self.open_xattr_entry_blocking(relative, kind)?;
        // Mirror `LocalEndpoint::write_acl`: empty text restores the
        // mode-derived base entries instead of leaving a bare ACL.
        let entries = if acl.is_empty() {
            let mode = file.metadata()?.permissions().mode();
            exacl::from_mode(mode & 0o777)
        } else {
            exacl::from_str(acl)?
        };
        exacl::setfacl(&[fd_alias_path(&file)], &entries, None)?;
        Ok(())
    }

    /// macOS: `/dev/fd/N` does NOT resolve to the open inode for
    /// `acl_get_file` (verified: it returns an empty list), so the fd-based
    /// syscalls in `acl_macos` carry the conversion instead. Same
    /// no-follow held descriptor, same exacl text format.
    #[cfg(all(target_os = "macos", feature = "acl"))]
    fn read_acl_path_blocking(&self, relative: &Path, kind: EntryKind) -> Result<Option<String>> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        let entries = acl_macos::read_fd_entries(file.as_raw_fd())?;
        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(exacl::to_string(&entries)?))
    }

    #[cfg(all(target_os = "macos", feature = "acl"))]
    fn write_acl_path_blocking(&self, relative: &Path, kind: EntryKind, acl: &str) -> Result<()> {
        let file = self.open_xattr_entry_blocking(relative, kind)?;
        let entries = exacl::from_str(acl)?;
        acl_macos::write_fd_entries(file.as_raw_fd(), &entries)?;
        Ok(())
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
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
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
    let from = component_cstring(from)?;
    let to = component_cstring(to)?;
    let result = unsafe {
        // SAFETY: `parent` remains open for the call and both names are live
        // NUL-terminated single components. Both lookups stay inside the held
        // directory descriptor.
        libc::renameat(parent, from.as_ptr(), parent, to.as_ptr())
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
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
            .apply_metadata_blocking(Some(0o640), Some(modified))
            .unwrap();
        staged.commit().unwrap();

        let metadata = std::fs::metadata(root.path().join("file")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o640);
        assert_eq!(metadata.mtime(), modified.seconds());
        assert_eq!(metadata.mtime_nsec(), i64::from(modified.nanoseconds()));
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
}
