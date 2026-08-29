use super::RootedFs;
use crate::endpoint::local_identity::metadata_identity;
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::engine::reconcile::{BoxError, EntryStream};
use crate::engine::scan::ScanRequest;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const CHANNEL_CAPACITY: usize = 256;
const MAX_SYMLINK_TARGET_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RootedScanError {
    #[error("gitignore-aware scanning is not yet supported by the descriptor-rooted scanner")]
    GitignoreUnsupported,

    #[error("failed to enumerate descriptor-rooted directory: {0}")]
    ReadDirectory(#[source] io::Error),

    #[error("failed to inspect descriptor-rooted entry {path}: {source}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to read descriptor-rooted symlink {path}: {source}")]
    SymlinkTarget {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("unsupported special filesystem entry: {0}")]
    UnsupportedFileType(PathBuf),

    #[error("descriptor-rooted scan observed an unstable symlink: {0}")]
    UnstableSymlink(PathBuf),

    #[error("descriptor-rooted scan path is invalid: {0}")]
    InvalidRelativePath(PathBuf),

    #[error("descriptor-rooted entry size is negative: {0}")]
    NegativeSize(PathBuf),

    #[error("descriptor-rooted timestamp is invalid for {0}")]
    InvalidTimestamp(PathBuf),

    #[error("descriptor-rooted symlink metadata is unsupported on this Unix platform")]
    UnsupportedPlatform,
}

impl RootedFs {
    /// Produce a strictly ordered metadata stream rooted at the directory inode
    /// pinned by this `RootedFs`. The root pathname is never consulted.
    pub(crate) fn entry_stream(&self, request: ScanRequest) -> EntryStream {
        let rooted = self.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let join_sender = sender.clone();

        tokio::spawn(async move {
            let scan =
                tokio::task::spawn_blocking(move || scan_worker(rooted, request, sender)).await;
            if let Err(error) = scan {
                let _ = join_sender.send(Err(Box::new(error) as BoxError)).await;
            }
        });

        Box::pin(futures::stream::unfold(
            receiver,
            |mut receiver| async move { receiver.recv().await.map(|entry| (entry, receiver)) },
        ))
    }
}

fn scan_worker(
    rooted: RootedFs,
    request: ScanRequest,
    sender: tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
) {
    if request.respect_gitignore {
        send_error(&sender, RootedScanError::GitignoreUnsupported);
        return;
    }
    if request.max_depth == Some(0) {
        return;
    }

    let root = match rooted.root_fd.try_clone() {
        Ok(root) => root,
        Err(error) => {
            send_error(&sender, RootedScanError::ReadDirectory(error));
            return;
        }
    };
    if let Err(error) = walk_directory(&root, Path::new(""), 0, request, &sender) {
        send_error(&sender, error);
    }
}

fn send_error(
    sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
    error: RootedScanError,
) {
    let _ = sender.blocking_send(Err(Box::new(error) as BoxError));
}

fn walk_directory(
    directory: &OwnedFd,
    relative_dir: &Path,
    depth: usize,
    request: ScanRequest,
    sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
) -> Result<(), RootedScanError> {
    let mut names = directory_names(directory.as_raw_fd())?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

    for name in names {
        if !request.include_git_dir && name.as_bytes() == b".git" {
            continue;
        }

        let relative_path = if relative_dir.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            relative_dir.join(&name)
        };
        let relative = RelativePath::new(relative_path.clone())
            .map_err(|_| RootedScanError::InvalidRelativePath(relative_path.clone()))?;
        let inspected = inspect_entry(directory.as_raw_fd(), &name, &relative, request)?;
        let descend = inspected.directory;
        if sender.blocking_send(Ok(inspected.entry)).is_err() {
            return Ok(());
        }

        let may_descend = request
            .max_depth
            .is_none_or(|max_depth| depth.saturating_add(1) < max_depth);
        if may_descend {
            if let Some(child) = descend {
                walk_directory(&child, relative.as_path(), depth + 1, request, sender)?;
            }
        }
    }
    Ok(())
}

struct InspectedEntry {
    entry: Entry,
    directory: Option<OwnedFd>,
}

fn inspect_entry(
    parent: RawFd,
    name: &OsStr,
    relative: &RelativePath,
    request: ScanRequest,
) -> Result<InspectedEntry, RootedScanError> {
    let stat = lstat_at(parent, name, relative.as_path())?;
    let file_type = stat.st_mode & libc::S_IFMT;

    if file_type == libc::S_IFDIR {
        let directory = open_dir_at(parent, name, relative.as_path())?;
        let file =
            File::from(
                directory
                    .try_clone()
                    .map_err(|source| RootedScanError::Metadata {
                        path: relative.as_path().to_path_buf(),
                        source,
                    })?,
            );
        let metadata = file
            .metadata()
            .map_err(|source| RootedScanError::Metadata {
                path: relative.as_path().to_path_buf(),
                source,
            })?;
        let entry =
            entry_from_metadata(relative.clone(), EntryKind::Directory, &metadata, request)?;
        return Ok(InspectedEntry {
            entry,
            directory: Some(directory),
        });
    }

    if file_type == libc::S_IFREG {
        let file = open_regular_at(parent, name, relative.as_path())?;
        let metadata = file
            .metadata()
            .map_err(|source| RootedScanError::Metadata {
                path: relative.as_path().to_path_buf(),
                source,
            })?;
        if !metadata.file_type().is_file() {
            return Err(RootedScanError::UnstableSymlink(
                relative.as_path().to_path_buf(),
            ));
        }
        let entry = entry_from_metadata(relative.clone(), EntryKind::File, &metadata, request)?;
        return Ok(InspectedEntry {
            entry,
            directory: None,
        });
    }

    if file_type == libc::S_IFLNK {
        let before = symlink_snapshot(&stat, relative.as_path())?;
        let target = if request.metadata.symlink_target {
            Some(readlink_at(parent, name, relative.as_path())?)
        } else {
            None
        };
        let after_stat = lstat_at(parent, name, relative.as_path())?;
        let after = symlink_snapshot(&after_stat, relative.as_path())?;
        if before != after {
            return Err(RootedScanError::UnstableSymlink(
                relative.as_path().to_path_buf(),
            ));
        }

        let mut entry = Entry::symlink(
            relative.clone(),
            target.clone().unwrap_or_default(),
            before.modified,
        );
        entry.symlink_target = target;
        if request.metadata.unix_mode {
            entry.unix_mode = Some(before.mode & 0o7777);
        }
        if request.metadata.identity {
            entry.identity = Some(before.identity);
        }
        Ok(InspectedEntry {
            entry,
            directory: None,
        })
    } else {
        Err(RootedScanError::UnsupportedFileType(
            relative.as_path().to_path_buf(),
        ))
    }
}

fn entry_from_metadata(
    relative: RelativePath,
    kind: EntryKind,
    metadata: &std::fs::Metadata,
    request: ScanRequest,
) -> Result<Entry, RootedScanError> {
    let nanoseconds = u32::try_from(metadata.mtime_nsec())
        .map_err(|_| RootedScanError::InvalidTimestamp(relative.as_path().to_path_buf()))?;
    let modified = Timestamp::new(metadata.mtime(), nanoseconds)
        .map_err(|_| RootedScanError::InvalidTimestamp(relative.as_path().to_path_buf()))?;
    let mut entry = match kind {
        EntryKind::File => Entry::file(relative, metadata.len(), modified),
        EntryKind::Directory => Entry::directory(relative, modified),
        EntryKind::Symlink => unreachable!("symlinks use fstatat metadata"),
    };
    if request.metadata.unix_mode {
        entry.unix_mode = Some(metadata.mode() & 0o7777);
    }
    if request.metadata.identity {
        entry.identity = metadata_identity(metadata, kind);
    }
    if request.metadata.hardlink_group && kind == EntryKind::File && metadata.nlink() > 1 {
        entry.hardlink_group = Some(hardlink_group(metadata));
    }
    Ok(entry)
}

fn hardlink_group(metadata: &std::fs::Metadata) -> EntryIdentity {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sy-hardlink-group-v1\0");
    hasher.update(&metadata.dev().to_le_bytes());
    hasher.update(&metadata.ino().to_le_bytes());
    EntryIdentity::from_bytes(*hasher.finalize().as_bytes())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SymlinkSnapshot {
    mode: u32,
    modified: Timestamp,
    identity: EntryIdentity,
}

fn symlink_snapshot(stat: &libc::stat, path: &Path) -> Result<SymlinkSnapshot, RootedScanError> {
    let (mtime, mtime_nsec, ctime, ctime_nsec) = stat_times(stat)?;
    let modified = Timestamp::new(mtime, mtime_nsec)
        .map_err(|_| RootedScanError::InvalidTimestamp(path.to_path_buf()))?;
    let size = u64::try_from(stat.st_size)
        .map_err(|_| RootedScanError::NegativeSize(path.to_path_buf()))?;
    let mode = stat.st_mode as u32;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sy-entry-identity-v1\0");
    hasher.update(&(stat.st_dev as u64).to_le_bytes());
    hasher.update(&(stat.st_ino as u64).to_le_bytes());
    hasher.update(&size.to_le_bytes());
    hasher.update(&mode.to_le_bytes());
    hasher.update(&mtime.to_le_bytes());
    hasher.update(&(i64::from(mtime_nsec)).to_le_bytes());
    hasher.update(&ctime.to_le_bytes());
    hasher.update(&(i64::from(ctime_nsec)).to_le_bytes());
    hasher.update(&[3]);
    Ok(SymlinkSnapshot {
        mode,
        modified,
        identity: EntryIdentity::from_bytes(*hasher.finalize().as_bytes()),
    })
}

#[cfg(target_os = "linux")]
fn stat_times(stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    let mtime_nsec =
        u32::try_from(stat.st_mtime_nsec).map_err(|_| RootedScanError::UnsupportedPlatform)?;
    let ctime_nsec =
        u32::try_from(stat.st_ctime_nsec).map_err(|_| RootedScanError::UnsupportedPlatform)?;
    Ok((stat.st_mtime, mtime_nsec, stat.st_ctime, ctime_nsec))
}

#[cfg(target_os = "macos")]
fn stat_times(stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    let mtime_nsec = u32::try_from(stat.st_mtimespec.tv_nsec)
        .map_err(|_| RootedScanError::UnsupportedPlatform)?;
    let ctime_nsec = u32::try_from(stat.st_ctimespec.tv_nsec)
        .map_err(|_| RootedScanError::UnsupportedPlatform)?;
    Ok((
        stat.st_mtimespec.tv_sec,
        mtime_nsec,
        stat.st_ctimespec.tv_sec,
        ctime_nsec,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stat_times(_stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    Err(RootedScanError::UnsupportedPlatform)
}

fn directory_names(fd: RawFd) -> Result<Vec<OsString>, RootedScanError> {
    let duplicate = unsafe {
        // SAFETY: `fd` is a live directory descriptor and dup creates an
        // independent descriptor for fdopendir to own.
        libc::dup(fd)
    };
    if duplicate < 0 {
        return Err(RootedScanError::ReadDirectory(io::Error::last_os_error()));
    }
    let dir = unsafe {
        // SAFETY: `duplicate` is a fresh directory descriptor. fdopendir takes
        // ownership on success.
        libc::fdopendir(duplicate)
    };
    if dir.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            // SAFETY: fdopendir failed, so ownership remained with us.
            libc::close(duplicate);
        }
        return Err(RootedScanError::ReadDirectory(error));
    }
    let mut guard = DirectoryStream(dir);
    let mut names = Vec::new();
    loop {
        set_errno(0);
        let entry = unsafe {
            // SAFETY: guard owns a live DIR pointer for this loop.
            libc::readdir(guard.0)
        };
        if entry.is_null() {
            let errno = get_errno();
            if errno != 0 {
                return Err(RootedScanError::ReadDirectory(
                    io::Error::from_raw_os_error(errno),
                ));
            }
            break;
        }
        let name = unsafe {
            // SAFETY: d_name is NUL-terminated for a successful readdir result
            // and remains valid until the next readdir call.
            CStr::from_ptr((*entry).d_name.as_ptr())
        }
        .to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        names.push(OsString::from_vec(name.to_vec()));
    }
    // Close before returning so directory descriptors remain tightly bounded by
    // recursion depth rather than scan cardinality.
    guard.close();
    Ok(names)
}

struct DirectoryStream(*mut libc::DIR);

impl DirectoryStream {
    fn close(&mut self) {
        if self.0.is_null() {
            return;
        }
        unsafe {
            // SAFETY: self.0 is uniquely owned by this guard.
            libc::closedir(self.0);
        }
        self.0 = std::ptr::null_mut();
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        self.close();
    }
}

fn lstat_at(parent: RawFd, name: &OsStr, path: &Path) -> Result<libc::stat, RootedScanError> {
    let name = component_cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let result = unsafe {
        // SAFETY: parent is live, name is one NUL-terminated component, and the
        // output buffer is valid for one stat structure.
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        return Err(RootedScanError::Metadata {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(unsafe {
        // SAFETY: successful fstatat initialized the output buffer.
        stat.assume_init()
    })
}

fn open_dir_at(parent: RawFd, name: &OsStr, path: &Path) -> Result<OwnedFd, RootedScanError> {
    let name = component_cstring(name)?;
    let fd = unsafe {
        // SAFETY: parent is live and name is one NUL-terminated component.
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(RootedScanError::Metadata {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(unsafe {
        // SAFETY: successful openat returned a fresh owned descriptor.
        OwnedFd::from_raw_fd(fd)
    })
}

fn open_regular_at(parent: RawFd, name: &OsStr, path: &Path) -> Result<File, RootedScanError> {
    let name = component_cstring(name)?;
    let fd = unsafe {
        // SAFETY: parent is live and name is one NUL-terminated component.
        // O_NONBLOCK prevents a raced FIFO/device replacement from blocking the
        // metadata scan; the opened type is verified before use.
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(RootedScanError::Metadata {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    let owned = unsafe {
        // SAFETY: successful openat returned a fresh owned descriptor.
        OwnedFd::from_raw_fd(fd)
    };
    Ok(File::from(owned))
}

fn readlink_at(parent: RawFd, name: &OsStr, path: &Path) -> Result<PathBuf, RootedScanError> {
    let name = component_cstring(name)?;
    let mut capacity = 256_usize;
    loop {
        let mut buffer = vec![0_u8; capacity];
        let read = unsafe {
            // SAFETY: parent is live, name is one NUL-terminated component, and
            // buffer is writable for `capacity` bytes.
            libc::readlinkat(
                parent,
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if read < 0 {
            return Err(RootedScanError::SymlinkTarget {
                path: path.to_path_buf(),
                source: io::Error::last_os_error(),
            });
        }
        let read = usize::try_from(read).map_err(|_| RootedScanError::SymlinkTarget {
            path: path.to_path_buf(),
            source: io::Error::other("negative readlink length"),
        })?;
        if read < buffer.len() {
            buffer.truncate(read);
            return Ok(PathBuf::from(OsString::from_vec(buffer)));
        }
        if capacity >= MAX_SYMLINK_TARGET_BYTES {
            return Err(RootedScanError::SymlinkTarget {
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, "symlink target is too long"),
            });
        }
        capacity = capacity.saturating_mul(2).min(MAX_SYMLINK_TARGET_BYTES);
    }
}

fn component_cstring(name: &OsStr) -> Result<CString, RootedScanError> {
    CString::new(name.as_bytes()).map_err(|_| {
        RootedScanError::ReadDirectory(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory entry contains NUL",
        ))
    })
}

#[cfg(target_os = "linux")]
fn set_errno(value: libc::c_int) {
    unsafe {
        // SAFETY: libc exposes the calling thread's errno slot.
        *libc::__errno_location() = value;
    }
}

#[cfg(target_os = "linux")]
fn get_errno() -> libc::c_int {
    unsafe {
        // SAFETY: libc exposes the calling thread's errno slot.
        *libc::__errno_location()
    }
}

#[cfg(target_os = "macos")]
fn set_errno(value: libc::c_int) {
    unsafe {
        // SAFETY: libc exposes the calling thread's errno slot.
        *libc::__error() = value;
    }
}

#[cfg(target_os = "macos")]
fn get_errno() -> libc::c_int {
    unsafe {
        // SAFETY: libc exposes the calling thread's errno slot.
        *libc::__error()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn set_errno(_value: libc::c_int) {}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn get_errno() -> libc::c_int {
    0
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures::StreamExt;

    async fn collect(rooted: &RootedFs, request: ScanRequest) -> Vec<Entry> {
        rooted
            .entry_stream(request)
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn scan_is_ordered_and_identity_safe() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("b")).unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        std::fs::write(root.path().join("z"), b"z").unwrap();
        std::fs::write(root.path().join("a/file"), b"a").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        let entries = collect(&rooted, ScanRequest::default()).await;
        assert!(entries.windows(2).all(|pair| pair[0].path < pair[1].path));
        assert!(entries.iter().all(|entry| entry.identity.is_some()));
    }

    #[tokio::test]
    async fn zero_depth_scan_emits_no_entries() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut request = ScanRequest::default();
        request.max_depth = Some(0);

        assert!(collect(&rooted, request).await.is_empty());
    }

    #[tokio::test]
    async fn scan_remains_on_pinned_root_after_path_swap() {
        let parent = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let root_path = parent.path().join("root");
        let moved_path = parent.path().join("moved");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(root_path.join("inside"), b"inside").unwrap();
        std::fs::write(outside.path().join("outside"), b"outside").unwrap();
        let rooted = RootedFs::open(root_path.clone()).await.unwrap();

        std::fs::rename(&root_path, &moved_path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root_path).unwrap();

        let entries = collect(&rooted, ScanRequest::default()).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.as_path(), Path::new("inside"));
    }

    #[tokio::test]
    async fn scan_refuses_gitignore_mode_until_it_is_descriptor_safe() {
        let root = tempfile::TempDir::new().unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut request = ScanRequest::default();
        request.respect_gitignore = true;
        let mut entries = rooted.entry_stream(request);
        assert!(entries.next().await.unwrap().is_err());
    }
}
