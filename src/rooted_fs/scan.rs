use super::RootedFs;
use crate::endpoint::local_identity::metadata_identity;
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::engine::reconcile::{BoxError, EntryStream};
use crate::engine::scan::ScanRequest;
use crate::engine::scan_sort::{read_name, NameSpool, SortBudget};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
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

    #[error("failed to spool ordered scan: {0}")]
    Scratch(#[source] io::Error),

    #[error("descriptor-rooted scan directory changed: {0}")]
    DirectoryChanged(PathBuf),

    #[error("descriptor-rooted scan path exceeds wire bounds: {0}")]
    PathLimit(PathBuf),

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

    if let Err(error) = walk_tree(&rooted, request, &sender, SortBudget::default()) {
        if !sender.is_closed() {
            send_error(&sender, error);
        }
    }
}

fn send_error(sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>, error: RootedScanError) {
    let _ = sender.blocking_send(Err(Box::new(error) as BoxError));
}

// Fixed-size continuation records keep both ancestor identities and sorted-run
// cursors on disk. No ancestor retains a name vector, run reader or directory FD.
#[derive(Clone, Copy)]
struct Continuation {
    run: u64,
    offset: u64,
    device: u64,
    inode: u64,
}

const CONTINUATION_BYTES: u64 = 32;

struct Continuations(tempfile::NamedTempFile);

impl Continuations {
    fn new() -> io::Result<Self> {
        Ok(Self(
            tempfile::Builder::new()
                .prefix("sy-scan-stack-")
                .tempfile()?,
        ))
    }

    fn write(&mut self, depth: usize, frame: Continuation) -> io::Result<()> {
        self.0.seek(SeekFrom::Start(frame_offset(depth)?))?;
        let mut record = [0; CONTINUATION_BYTES as usize];
        for (field, value) in record.as_chunks_mut::<8>().0.iter_mut().zip([
            frame.run,
            frame.offset,
            frame.device,
            frame.inode,
        ]) {
            *field = value.to_le_bytes();
        }
        self.0.write_all(&record)?;
        Ok(())
    }

    fn read(&mut self, depth: usize) -> io::Result<Continuation> {
        self.0.seek(SeekFrom::Start(frame_offset(depth)?))?;
        let mut record = [0; CONTINUATION_BYTES as usize];
        self.0.read_exact(&mut record)?;
        let mut values = [0; 4];
        for (field, value) in record.as_chunks::<8>().0.iter().zip(&mut values) {
            *value = u64::from_le_bytes(*field);
        }
        Ok(Continuation {
            run: values[0],
            offset: values[1],
            device: values[2],
            inode: values[3],
        })
    }

    fn truncate(&mut self, frames: usize) -> io::Result<()> {
        self.0.as_file_mut().set_len(frame_offset(frames)?)
    }
}

fn frame_offset(depth: usize) -> io::Result<u64> {
    u64::try_from(depth)
        .ok()
        .and_then(|depth| depth.checked_mul(CONTINUATION_BYTES))
        .ok_or_else(|| io::Error::other("scan continuation offset overflow"))
}

fn directory_identity(directory: &OwnedFd, path: &Path) -> Result<(u64, u64), RootedScanError> {
    let metadata =
        File::from(
            directory
                .try_clone()
                .map_err(|source| RootedScanError::Metadata {
                    path: path.to_path_buf(),
                    source,
                })?,
        )
        .metadata()
        .map_err(|source| RootedScanError::Metadata {
            path: path.to_path_buf(),
            source,
        })?;
    Ok((metadata.dev(), metadata.ino()))
}

fn check_directory(
    directory: &OwnedFd,
    path: &Path,
    expected: Continuation,
) -> Result<(), RootedScanError> {
    if directory_identity(directory, path)? != (expected.device, expected.inode) {
        return Err(RootedScanError::DirectoryChanged(path.to_path_buf()));
    }
    Ok(())
}

fn reopen_directory(
    rooted: &RootedFs,
    path: &Path,
    stack: &mut Continuations,
) -> Result<OwnedFd, RootedScanError> {
    let mut directory = rooted
        .root_fd
        .try_clone()
        .map_err(RootedScanError::ReadDirectory)?;
    check_directory(
        &directory,
        Path::new(""),
        stack.read(0).map_err(RootedScanError::Scratch)?,
    )?;
    // Reopen from the held root, one no-follow component at a time. Compare
    // EVERY ancestor, not just the leaf: a renamed directory must not resume
    // beneath a replacement parent even if its original inode was moved there.
    for (index, component) in path.components().enumerate() {
        directory = open_dir_at(directory.as_raw_fd(), component.as_os_str(), path)?;
        check_directory(
            &directory,
            path,
            stack.read(index + 1).map_err(RootedScanError::Scratch)?,
        )?;
    }
    Ok(directory)
}

fn prepare_directory(
    directory: &OwnedFd,
    path: &Path,
    spool: &mut NameSpool,
    sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
) -> Result<Continuation, RootedScanError> {
    let (device, inode) = directory_identity(directory, path)?;
    let names = DirectoryStream::open(directory.as_raw_fd())?;
    let run = spool
        .sort(names, || sender.is_closed())
        .map_err(RootedScanError::Scratch)?;
    Ok(Continuation {
        run,
        offset: 0,
        device,
        inode,
    })
}

fn validate_path_bound(path: &Path) -> Result<(), RootedScanError> {
    let mut bytes = 2;
    let mut count = 0;
    for component in path.components() {
        bytes += 2 + component.as_os_str().as_bytes().len();
        count += 1;
        if bytes > crate::protocol::MAX_WIRE_PATH_BYTES
            || count > crate::protocol::MAX_WIRE_COMPONENTS
        {
            return Err(RootedScanError::PathLimit(path.to_path_buf()));
        }
    }
    Ok(())
}

// Scratch may lie beneath the scanned root (e.g. scanning /tmp). Do not emit
// our own bookkeeping as source data or recursively scan the run directory.
// Match native name AND inode so an unrelated same-named entry is not excluded.
struct ScratchEntry {
    name: OsString,
    device: libc::dev_t,
    inode: libc::ino_t,
}

impl ScratchEntry {
    fn new(path: &Path, file: &File) -> Result<Self, RootedScanError> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let result = unsafe {
            // SAFETY: file is live and the output buffer fits one stat.
            libc::fstat(file.as_raw_fd(), stat.as_mut_ptr())
        };
        if result < 0 {
            return Err(RootedScanError::Scratch(io::Error::last_os_error()));
        }
        let stat = unsafe {
            // SAFETY: successful fstat initialized the output buffer.
            stat.assume_init()
        };
        let name = path
            .file_name()
            .ok_or_else(|| RootedScanError::Scratch(io::Error::other("scratch path has no name")))?
            .to_owned();
        Ok(Self {
            name,
            device: stat.st_dev,
            inode: stat.st_ino,
        })
    }

    fn matches(&self, parent: RawFd, name: &OsStr, path: &Path) -> Result<bool, RootedScanError> {
        if name != self.name {
            return Ok(false);
        }
        let stat = lstat_at(parent, name, path)?;
        Ok(stat.st_dev == self.device && stat.st_ino == self.inode)
    }
}

fn walk_tree(
    rooted: &RootedFs,
    request: ScanRequest,
    sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
    budget: SortBudget,
) -> Result<(), RootedScanError> {
    let mut spool = NameSpool::new(budget).map_err(RootedScanError::Scratch)?;
    let mut stack = Continuations::new().map_err(RootedScanError::Scratch)?;
    let scratch = [
        ScratchEntry::new(
            spool.scratch_path(),
            &File::open(spool.scratch_path()).map_err(RootedScanError::Scratch)?,
        )?,
        ScratchEntry::new(stack.0.path(), stack.0.as_file())?,
    ];
    let mut directory = rooted
        .root_fd
        .try_clone()
        .map_err(RootedScanError::ReadDirectory)?;
    let mut path = PathBuf::new();
    let mut depth = 0;
    let mut frame = prepare_directory(&directory, &path, &mut spool, sender)?;
    stack
        .write(depth, frame)
        .map_err(RootedScanError::Scratch)?;
    let mut names = BufReader::new(
        spool
            .open(frame.run, frame.offset)
            .map_err(RootedScanError::Scratch)?,
    );

    while !sender.is_closed() {
        let Some(name) = read_name(&mut names).map_err(RootedScanError::Scratch)? else {
            drop(names);
            spool.remove(frame.run).map_err(RootedScanError::Scratch)?;
            if depth == 0 {
                stack.0.close().map_err(RootedScanError::Scratch)?;
                spool.close().map_err(RootedScanError::Scratch)?;
                return Ok(());
            }
            depth -= 1;
            path.pop();
            stack
                .truncate(depth + 1)
                .map_err(RootedScanError::Scratch)?;
            // Close the completed directory before reopening the parent chain.
            drop(directory);
            directory = reopen_directory(rooted, &path, &mut stack)?;
            frame = stack.read(depth).map_err(RootedScanError::Scratch)?;
            names = BufReader::new(
                spool
                    .open(frame.run, frame.offset)
                    .map_err(RootedScanError::Scratch)?,
            );
            continue;
        };
        if !request.include_git_dir && name.as_bytes() == b".git" {
            continue;
        }
        let relative_path = path.join(&name);
        if scratch[0].matches(directory.as_raw_fd(), &name, &relative_path)?
            || scratch[1].matches(directory.as_raw_fd(), &name, &relative_path)?
        {
            continue;
        }
        validate_path_bound(&relative_path)?;
        let relative = RelativePath::new(relative_path.clone())
            .map_err(|_| RootedScanError::InvalidRelativePath(relative_path))?;
        let inspected = inspect_entry(directory.as_raw_fd(), &name, &relative, request)?;
        if sender.blocking_send(Ok(inspected.entry)).is_err() {
            return Ok(());
        }
        if request.max_depth.is_none_or(|limit| depth + 1 < limit) {
            if let Some(child) = inspected.directory {
                frame.offset = names.stream_position().map_err(RootedScanError::Scratch)?;
                stack
                    .write(depth, frame)
                    .map_err(RootedScanError::Scratch)?;
                drop(names);
                directory = child;
                path = relative.into_path_buf();
                depth += 1;
                frame = prepare_directory(&directory, &path, &mut spool, sender)?;
                stack
                    .write(depth, frame)
                    .map_err(RootedScanError::Scratch)?;
                names = BufReader::new(spool.open(frame.run, 0).map_err(RootedScanError::Scratch)?);
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
        EntryKind::Symlink => Entry::symlink(relative, PathBuf::new(), modified),
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

#[cfg(target_os = "linux")]
const fn stat_mode_u32(stat: &libc::stat) -> u32 {
    stat.st_mode as u32
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn stat_mode_u32(stat: &libc::stat) -> u32 {
    stat.st_mode as u32
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn stat_mode_u32(stat: &libc::stat) -> u32 {
    stat.st_mode as u32
}

fn symlink_snapshot(stat: &libc::stat, path: &Path) -> Result<SymlinkSnapshot, RootedScanError> {
    if stat.st_size < 0 {
        return Err(RootedScanError::NegativeSize(path.to_path_buf()));
    }
    let (mtime, mtime_nsec, _ctime, _ctime_nsec) = stat_times(stat)?;
    let modified = Timestamp::new(mtime, mtime_nsec)
        .map_err(|_| RootedScanError::InvalidTimestamp(path.to_path_buf()))?;
    let mode = stat_mode_u32(stat);
    let identity = crate::endpoint::local_identity::stat_identity(stat, EntryKind::Symlink)
        .ok_or_else(|| RootedScanError::InvalidTimestamp(path.to_path_buf()))?;
    Ok(SymlinkSnapshot {
        mode,
        modified,
        identity,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stat_times(_stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    Err(RootedScanError::UnsupportedPlatform)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stat_times(_stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    Err(RootedScanError::UnsupportedPlatform)
}

// The mutation helper still uses this collection API. Ordered scanning uses
// DirectoryStream directly and never materializes all siblings.
pub(super) fn directory_names(fd: RawFd) -> Result<Vec<OsString>, RootedScanError> {
    DirectoryStream::open(fd)?
        .collect::<io::Result<Vec<_>>>()
        .map_err(RootedScanError::ReadDirectory)
}

/// Bounded namespace check: stop at the first non-dot entry without collecting names.
pub(super) fn directory_is_empty(fd: RawFd) -> Result<bool, RootedScanError> {
    let scan_fd = unsafe {
        // SAFETY: fd is a held directory; reopening dot gives an independent
        // offset and cannot follow a peer-controlled component.
        libc::openat(
            fd,
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if scan_fd < 0 {
        return Err(RootedScanError::ReadDirectory(io::Error::last_os_error()));
    }
    let dir = unsafe {
        // SAFETY: scan_fd is fresh; fdopendir takes ownership only on success.
        libc::fdopendir(scan_fd)
    };
    if dir.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            // SAFETY: failed fdopendir left scan_fd owned here.
            libc::close(scan_fd);
        }
        return Err(RootedScanError::ReadDirectory(error));
    }
    let guard = DirectoryStream(dir);
    loop {
        set_errno(0);
        let entry = unsafe {
            // SAFETY: guard owns a live DIR for this loop.
            libc::readdir(guard.0)
        };
        if entry.is_null() {
            let errno = get_errno();
            return if errno == 0 {
                Ok(true)
            } else {
                Err(RootedScanError::ReadDirectory(
                    io::Error::from_raw_os_error(errno),
                ))
            };
        }
        let name = unsafe {
            // SAFETY: successful readdir supplies a NUL-terminated name valid
            // until the next readdir; no reference escapes this iteration.
            CStr::from_ptr((*entry).d_name.as_ptr())
        }
        .to_bytes();
        if name != b"." && name != b".." {
            return Ok(false);
        }
    }
}
struct DirectoryStream(*mut libc::DIR);

impl DirectoryStream {
    fn open(fd: RawFd) -> Result<Self, RootedScanError> {
        let scan_fd = unsafe {
            // SAFETY: `fd` is a live directory descriptor and `.` is a fixed native
            // component. Reopening it creates a distinct open file description, so
            // concurrent scans do not share the directory offset as they would with
            // dup(2).
            libc::openat(
                fd,
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if scan_fd < 0 {
            return Err(RootedScanError::ReadDirectory(io::Error::last_os_error()));
        }
        let dir = unsafe {
            // SAFETY: `scan_fd` is a fresh directory descriptor. fdopendir takes
            // ownership on success.
            libc::fdopendir(scan_fd)
        };
        if dir.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                // SAFETY: fdopendir failed, so ownership remained with us.
                libc::close(scan_fd);
            }
            return Err(RootedScanError::ReadDirectory(error));
        }
        Ok(Self(dir))
    }

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

impl Iterator for DirectoryStream {
    type Item = io::Result<OsString>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.0.is_null() {
            set_errno(0);
            let entry = unsafe {
                // SAFETY: self owns a live DIR pointer until close below.
                libc::readdir(self.0)
            };
            if entry.is_null() {
                let errno = get_errno();
                self.close();
                return (errno != 0).then(|| Err(io::Error::from_raw_os_error(errno)));
            }
            let name = unsafe {
                // SAFETY: successful readdir returns a NUL-terminated d_name
                // valid until the next call; copy it before advancing.
                CStr::from_ptr((*entry).d_name.as_ptr())
            }
            .to_bytes();
            if name != b"." && name != b".." {
                return Some(Ok(OsString::from_vec(name.to_vec())));
            }
        }
        None
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
fn set_errno(_value: libc::c_int) {}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn get_errno() -> libc::c_int {
    0
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn set_errno(_value: libc::c_int) {}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn get_errno() -> libc::c_int {
    0
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

    type TestScan = (
        tokio::sync::mpsc::Receiver<Result<Entry, BoxError>>,
        tokio::task::JoinHandle<Result<(), RootedScanError>>,
    );

    fn tiny_stream(rooted: RootedFs) -> TestScan {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let worker = tokio::task::spawn_blocking(move || {
            walk_tree(
                &rooted,
                ScanRequest::default(),
                &sender,
                SortBudget {
                    bytes: 90,
                    records: 3,
                    fan_in: 2,
                },
            )
        });
        (receiver, worker)
    }

    #[tokio::test]
    async fn tiny_budget_wide_and_deep_scans_are_incremental_and_ordered() {
        if let Some(path) = std::env::var_os("SY_TEST_SCAN_FD_ROOT") {
            let rooted = RootedFs::open(PathBuf::from(path)).await.unwrap();
            let (mut receiver, worker) = tiny_stream(rooted);
            let mut count = 0;
            while let Some(entry) = receiver.recv().await {
                entry.unwrap();
                count += 1;
            }
            worker.await.unwrap().unwrap();
            assert_eq!(count, 180 * 2 + 511);
            return;
        }
        let root = tempfile::TempDir::new().unwrap();
        // A sibling at each level forces a closed parent run to be resumed.
        let mut path = root.path().to_path_buf();
        for _ in 0..180 {
            std::fs::create_dir(path.join("a")).unwrap();
            std::fs::write(path.join("z"), b"sibling").unwrap();
            path.push("a");
        }
        for index in (0..511).rev() {
            std::fs::write(path.join(format!("n{index:04}")), b"leaf").unwrap();
        }
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (mut receiver, worker) = tiny_stream(rooted.clone());
        let mut previous = None;
        let mut count = 0;
        while let Some(entry) = receiver.recv().await {
            let entry = entry.unwrap();
            if let Some(previous) = &previous {
                assert!(previous < &entry.path)
            }
            previous = Some(entry.path);
            count += 1;
        }
        worker.await.unwrap().unwrap();
        assert_eq!(count, 180 * 2 + 511);

        let (mut receiver, worker) = tiny_stream(rooted);
        assert!(receiver.recv().await.unwrap().is_ok());
        drop(receiver);
        // Cancellation must end traversal even if it is currently building or
        // merging a run, not only when the output channel is full.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        match result {
            Ok(()) => {}
            Err(RootedScanError::Scratch(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            other => panic!("unexpected cancellation result: {other:?}"),
        }

        // Enforce a real descriptor ceiling in an isolated process; changing
        // this test process's limit would race unrelated parallel tests. Depth
        // exceeds the ceiling, so retaining even one FD per ancestor fails.
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "rooted_fs::scan::tests::tiny_budget_wide_and_deep_scans_are_incremental_and_ordered"])
            .env("SY_TEST_SCAN_FD_ROOT", root.path())
            .env("TMPDIR", root.path());
        unsafe {
            // SAFETY: the pre-exec hook only invokes async-signal-safe resource
            // limit syscalls, with valid local buffers; no locks/allocations.
            child.pre_exec(|| {
                let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
                if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut limit = limit.assume_init();
                limit.rlim_cur = limit.rlim_cur.min(64);
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "descriptor-limited scan failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn resumed_directory_checks_every_ancestor_and_never_follows_links() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("parent/child")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let mut stack = Continuations::new().unwrap();
        let mut directory = rooted.root_fd.try_clone().unwrap();
        for (depth, name) in [None, Some("parent"), Some("child")]
            .into_iter()
            .enumerate()
        {
            if let Some(name) = name {
                directory =
                    open_dir_at(directory.as_raw_fd(), OsStr::new(name), Path::new(name)).unwrap();
            }
            let (device, inode) = directory_identity(&directory, Path::new("")).unwrap();
            stack
                .write(
                    depth,
                    Continuation {
                        run: 0,
                        offset: 0,
                        device,
                        inode,
                    },
                )
                .unwrap();
        }
        drop(directory);
        // Keep the leaf inode unchanged, but substitute its ancestor.
        std::fs::rename(root.path().join("parent"), root.path().join("old")).unwrap();
        std::fs::create_dir(root.path().join("parent")).unwrap();
        std::fs::rename(
            root.path().join("old/child"),
            root.path().join("parent/child"),
        )
        .unwrap();
        assert!(matches!(
            reopen_directory(&rooted, Path::new("parent/child"), &mut stack),
            Err(RootedScanError::DirectoryChanged(_))
        ));
        std::fs::remove_dir_all(root.path().join("parent")).unwrap();
        std::os::unix::fs::symlink(root.path().join("old"), root.path().join("parent")).unwrap();
        assert!(reopen_directory(&rooted, Path::new("parent/child"), &mut stack).is_err());
    }

    #[tokio::test]
    async fn scan_is_ordered_and_identity_safe() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("b")).unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        std::fs::write(root.path().join("z"), b"z").unwrap();
        std::fs::write(root.path().join("a/file"), b"a").unwrap();
        std::fs::write(root.path().join("a.ext"), b"prefix sibling").unwrap();
        // APFS rejects non-UTF-8 filesystem names; the spool round-trip test
        // exercises native bytes on all Unix hosts without that limitation.
        #[cfg(target_os = "linux")]
        std::fs::write(root.path().join(OsString::from_vec(vec![0xff])), b"native").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        let entries = collect(&rooted, ScanRequest::default()).await;
        assert!(entries.windows(2).all(|pair| pair[0].path < pair[1].path));
        assert!(entries.iter().all(|entry| entry.identity.is_some()));
    }

    #[tokio::test]
    async fn concurrent_scans_have_independent_directory_cursors() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("a"), b"a").unwrap();
        std::fs::write(root.path().join("dir/b"), b"b").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        let (left, right) = tokio::join!(
            collect(&rooted, ScanRequest::default()),
            collect(&rooted, ScanRequest::default()),
        );
        let expected = [Path::new("a"), Path::new("dir"), Path::new("dir/b")];
        for entries in [&left, &right] {
            assert_eq!(entries.len(), expected.len());
            assert!(entries
                .iter()
                .zip(expected)
                .all(|(entry, path)| entry.path.as_path() == path));
        }
    }

    #[tokio::test]
    async fn zero_depth_scan_emits_no_entries() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let request = ScanRequest {
            max_depth: Some(0),
            ..ScanRequest::default()
        };

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
        let request = ScanRequest {
            respect_gitignore: true,
            ..ScanRequest::default()
        };
        let mut entries = rooted.entry_stream(request);
        assert!(entries.next().await.unwrap().is_err());
    }
}
