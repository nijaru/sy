use crate::engine::domain::RelativePath;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::ffi::{CString, OsStr};
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

    #[error("rooted filesystem path must contain only normal relative components")]
    InvalidRelativePath,

    #[error("filesystem path contains a NUL byte")]
    PathContainsNul,

    #[error("rooted file is not a regular file: {0}")]
    NotRegularFile(PathBuf),

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
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn remove_blocking(&self, relative: &RelativePath, is_directory: bool) -> Result<()> {
        self.remove_path_blocking(relative.as_path(), is_directory)
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
    fn remove_path_blocking(&self, relative: &Path, is_directory: bool) -> Result<()> {
        let (parent, leaf) = self.open_parent_blocking(relative)?;
        match unlink_at(parent.as_raw_fd(), &leaf, is_directory) {
            Ok(()) => Ok(()),
            Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[cfg(not(unix))]
    fn remove_path_blocking(&self, _relative: &Path, _is_directory: bool) -> Result<()> {
        Err(RootedFsError::UnsupportedPlatform)
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

    fn relative(path: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(path)).unwrap()
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
            .remove_blocking(&relative("escape/keep"), false)
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

        rooted.remove_blocking(&relative("file"), false).unwrap();
        rooted.remove_blocking(&relative("dir"), true).unwrap();
        assert!(!root.path().join("file").exists());
        assert!(!root.path().join("dir").exists());
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
