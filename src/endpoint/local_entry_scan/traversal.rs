//! Disk-backed preorder traversal. No inactive directory keeps an FD, name
//! vector, matcher stack, or run reader. The one current path is also the
//! lexical ignore ancestry; following a link must never change that ancestry.
use super::{engine_entry, entry_metadata, LocalScanError};
use crate::engine::domain::Entry;
use crate::engine::ignore_scope::SourceIgnoreScope;
use crate::engine::reconcile::BoxError;
use crate::engine::scan::ScanRequest;
use crate::engine::scan_sort::{read_name, NameSpool, SortBudget};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "traversal_tests.rs"]
mod tests;

type Sender = tokio::sync::mpsc::Sender<Result<Entry, BoxError>>;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity(u64, u64);

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> io::Result<Identity> {
    Ok(Identity(metadata.dev(), metadata.ino()))
}

fn path_identity(path: &Path, metadata: &std::fs::Metadata, follow: bool) -> io::Result<Identity> {
    #[cfg(unix)]
    {
        let _ = (path, follow);
        identity(metadata)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let _ = metadata;
        // No-follow scans identify the reparse point itself, including dangling
        // links. Followed scans must use target IDs for ancestor cycle checks.
        let flags = FILE_FLAG_BACKUP_SEMANTICS
            | if follow {
                0
            } else {
                FILE_FLAG_OPEN_REPARSE_POINT
            };
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(flags)
            .open(path)?;
        windows_identity(&file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, metadata, follow);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "local scan identities unsupported",
        ))
    }
}

#[cfg(windows)]
fn windows_identity(file: &File) -> io::Result<Identity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: file owns a live handle; information fits the Win32 output type.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful GetFileInformationByHandle initialized the output.
    let information = unsafe { information.assume_init() };
    Ok(Identity(
        u64::from(information.dwVolumeSerialNumber),
        u64::from(information.nFileIndexHigh) << 32 | u64::from(information.nFileIndexLow),
    ))
}

#[derive(Clone, Copy)]
struct Frame {
    run: u64,
    offset: u64,
    identity: Identity,
}

const FRAME_BYTES: u64 = 32;
struct Stack(tempfile::NamedTempFile);
impl Stack {
    #[cfg(test)]
    fn new() -> io::Result<Self> {
        Self::new_in(&tempfile::env::temp_dir())
    }
    fn new_in(parent: &Path) -> io::Result<Self> {
        Ok(Self(
            tempfile::Builder::new()
                .prefix("sy-local-stack-")
                .tempfile_in(parent)?,
        ))
    }
    fn position(depth: usize) -> io::Result<u64> {
        u64::try_from(depth)
            .ok()
            .and_then(|depth| depth.checked_mul(FRAME_BYTES))
            .ok_or_else(|| io::Error::other("local scan continuation overflow"))
    }
    fn write(&mut self, depth: usize, frame: Frame) -> io::Result<()> {
        self.0.seek(SeekFrom::Start(Self::position(depth)?))?;
        for value in [frame.run, frame.offset, frame.identity.0, frame.identity.1] {
            self.0.write_all(&value.to_le_bytes())?;
        }
        Ok(())
    }
    fn read(&mut self, depth: usize) -> io::Result<Frame> {
        self.0.seek(SeekFrom::Start(Self::position(depth)?))?;
        let mut values = [0; 4];
        for value in &mut values {
            let mut bytes = [0; 8];
            self.0.read_exact(&mut bytes)?;
            *value = u64::from_le_bytes(bytes);
        }
        Ok(Frame {
            run: values[0],
            offset: values[1],
            identity: Identity(values[2], values[3]),
        })
    }
    fn truncate(&mut self, frames: usize) -> io::Result<()> {
        self.0.as_file_mut().set_len(Self::position(frames)?)
    }
    fn contains(&mut self, depth: usize, candidate: Identity) -> io::Result<bool> {
        // Exact loop detection needs only this on-disk ancestor list, never a
        // per-tree visited set (which would also incorrectly reject aliases).
        for index in 0..=depth {
            if self.read(index)?.identity == candidate {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

struct Directory {
    file: File,
    #[cfg(not(unix))]
    path: PathBuf,
}
impl Directory {
    fn held(rooted: &crate::rooted_fs::RootedFs) -> io::Result<Self> {
        Ok(Self {
            file: rooted.clone_root_directory_blocking()?,
            #[cfg(not(unix))]
            path: rooted.root_path().to_path_buf(),
        })
    }

    fn root(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            Ok(Self {
                file: std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
                    .open(path)?,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            Ok(Self {
                // FILE_FLAG_BACKUP_SEMANTICS permits opening directories.
                file: std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(0x02000000)
                    .open(path)?,
                path: path.to_path_buf(),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local scan identities unsupported on this platform",
            ))
        }
    }
    fn scratch_parent(&self) -> io::Result<PathBuf> {
        let parent = tempfile::env::temp_dir();
        #[cfg(unix)]
        crate::engine::scan_sort::validate_scratch_root(&self.file, &parent)?;
        #[cfg(windows)]
        {
            let root_identity = self.identity()?;
            let mut ancestor = std::fs::canonicalize(&parent)?;
            loop {
                if Directory::root(&ancestor)?.identity()? == root_identity {
                    return Err(crate::engine::scan_sort::unsafe_scratch());
                }
                if !ancestor.pop() {
                    break;
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "scratch validation unsupported",
        ));
        Ok(parent)
    }
    fn child(&self, name: &OsStr, follow: bool) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let name = std::ffi::CString::new(name.as_bytes()).map_err(io::Error::other)?;
            let flags = libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_CLOEXEC
                | if follow { 0 } else { libc::O_NOFOLLOW };
            // SAFETY: self owns a live directory FD; name is one NUL-terminated
            // component. Success returns a fresh FD exclusively owned below.
            let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful openat returned a fresh owned descriptor.
            Ok(Self {
                file: unsafe { File::from_raw_fd(fd) },
            })
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            if !follow && std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(io::Error::other("local scan directory replaced by a link"));
            }
            Self::root(&path)
        }
    }
    fn identity(&self) -> io::Result<Identity> {
        #[cfg(unix)]
        {
            identity(&self.file.metadata()?)
        }
        #[cfg(windows)]
        {
            windows_identity(&self.file)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local scan identities unsupported on this platform",
            ))
        }
    }
    fn names(&self) -> io::Result<impl Iterator<Item = io::Result<OsString>>> {
        #[cfg(unix)]
        {
            DirectoryStream::open(self.file.as_raw_fd())
        }
        #[cfg(not(unix))]
        {
            Ok(std::fs::read_dir(&self.path)?.map(|entry| entry.map(|entry| entry.file_name())))
        }
    }
}

fn reopen(
    root: &Path,
    relative: &Path,
    follow: bool,
    stack: &mut Stack,
) -> Result<Directory, LocalScanError> {
    let mut directory = Directory::root(root)?;
    if directory.identity()? != stack.read(0)?.identity {
        return Err(LocalScanError::DirectoryChanged(root.to_path_buf()));
    }
    for (index, component) in relative.components().enumerate() {
        directory = directory.child(component.as_os_str(), follow)?;
        // Check EVERY ancestor: moving the original leaf under a substituted
        // parent must not allow resuming an old continuation in a new tree.
        if directory.identity()? != stack.read(index + 1)?.identity {
            return Err(LocalScanError::DirectoryChanged(root.join(relative)));
        }
    }
    Ok(directory)
}

fn prepare(directory: &Directory, spool: &mut NameSpool, sender: &Sender) -> io::Result<Frame> {
    let identity = directory.identity()?;
    let run = spool.sort(directory.names()?, || sender.is_closed())?;
    Ok(Frame {
        run,
        offset: 0,
        identity,
    })
}

struct Scratch {
    path: PathBuf,
    identity: Identity,
}
impl Scratch {
    fn new(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            identity: path_identity(path, &std::fs::metadata(path)?, false)?,
        })
    }
    fn matches(&self, path: &Path, metadata: &std::fs::Metadata, follow: bool) -> io::Result<bool> {
        // Identity also excludes followed aliases of our scratch directory.
        // Path excludes the owned name even if cleanup/removal races inspection.
        Ok(path == self.path || path_identity(path, metadata, follow)? == self.identity)
    }
}

pub(super) fn validate_scratch_location(root: &Path) -> io::Result<()> {
    Directory::root(root)?.scratch_parent().map(|_| ())
}

/// A selected observation never enumerates the operational parent. Keep the
/// parent handle alive and validate its pathname binding around stat/readlink,
/// just as tree traversal validates reopened ancestors.
pub(super) fn selected_leaf(
    root: &Path,
    relative: &crate::engine::domain::RelativePath,
    request: ScanRequest,
    missing_allowed: bool,
    sender: &Sender,
) -> Result<(), LocalScanError> {
    selected_leaf_with_root(
        root,
        relative,
        request,
        missing_allowed,
        sender,
        SourceIgnoreScope::new(root, request.respect_gitignore),
        || Directory::root(root),
    )
}

pub(super) fn selected_leaf_rooted(
    rooted: &crate::rooted_fs::RootedFs,
    relative: &crate::engine::domain::RelativePath,
    request: ScanRequest,
    missing_allowed: bool,
    sender: &Sender,
) -> Result<(), LocalScanError> {
    selected_leaf_with_root(
        rooted.root_path(),
        relative,
        request,
        missing_allowed,
        sender,
        SourceIgnoreScope::with_rooted_authority(rooted.clone(), request.respect_gitignore),
        || Directory::held(rooted),
    )
}

fn selected_leaf_with_root(
    root: &Path,
    relative: &crate::engine::domain::RelativePath,
    request: ScanRequest,
    missing_allowed: bool,
    sender: &Sender,
    mut scope: SourceIgnoreScope,
    open_root: impl FnOnce() -> io::Result<Directory>,
) -> Result<(), LocalScanError> {
    if relative.as_path().components().count() != 1 {
        return Err(LocalScanError::OutsideRoot {
            path: relative.as_path().to_path_buf(),
        });
    }
    let directory = match open_root() {
        Ok(directory) => directory,
        Err(error) if missing_allowed && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let expected = directory.identity()?;
    let absolute = root.join(relative.as_path());
    let metadata = match entry_metadata(&absolute, request) {
        Ok(metadata) => Some(metadata),
        Err(LocalScanError::Metadata { source, .. })
            if missing_allowed && source.kind() == io::ErrorKind::NotFound =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let entry = match metadata {
        Some(metadata) => {
            if (!request.include_git_dir && relative.as_path() == Path::new(".git"))
                || (!missing_allowed
                    && scope.source_match(relative.as_path(), metadata.is_dir())?)
            {
                None
            } else {
                Some(engine_entry(root, &absolute, &metadata, request)?)
            }
        }
        None => None,
    };
    if Directory::root(root)?.identity()? != expected {
        return Err(LocalScanError::DirectoryChanged(root.to_path_buf()));
    }
    if let Some(entry) = entry {
        // A directory requires a complete descendant view for transitions;
        // selected leaves must not silently authorize recursive replacement.
        if entry.is_directory() && !missing_allowed {
            return Err(LocalScanError::Walk(io::Error::other(
                "selected leaf is a directory; a tree scope is required",
            )));
        }
        let _ = sender.blocking_send(Ok(entry));
    }
    Ok(())
}

pub(super) fn walk_tree(
    root: &Path,
    request: ScanRequest,
    sender: &Sender,
    budget: SortBudget,
) -> Result<(), LocalScanError> {
    walk_tree_with_root(
        root,
        request,
        sender,
        budget,
        Directory::root(root)?,
        SourceIgnoreScope::new(root, request.respect_gitignore),
    )
}

pub(super) fn walk_tree_rooted(
    rooted: &crate::rooted_fs::RootedFs,
    request: ScanRequest,
    sender: &Sender,
    budget: SortBudget,
) -> Result<(), LocalScanError> {
    walk_tree_with_root(
        rooted.root_path(),
        request,
        sender,
        budget,
        Directory::held(rooted)?,
        SourceIgnoreScope::with_rooted_authority(rooted.clone(), request.respect_gitignore),
    )
}

fn walk_tree_with_root(
    root: &Path,
    request: ScanRequest,
    sender: &Sender,
    budget: SortBudget,
    mut directory: Directory,
    mut scope: SourceIgnoreScope,
) -> Result<(), LocalScanError> {
    scope.prepare_source()?;
    if request.max_depth == Some(0) {
        return Ok(());
    }
    let scratch_parent = directory.scratch_parent()?;
    let mut spool = NameSpool::new_in(budget, &scratch_parent)?;
    let mut stack = Stack::new_in(&scratch_parent)?;
    let scratch = [
        Scratch::new(spool.scratch_path())?,
        Scratch::new(stack.0.path())?,
    ];
    let mut path = PathBuf::new();
    let mut depth = 0;
    let mut frame = prepare(&directory, &mut spool, sender)?;
    stack.write(0, frame)?;
    let mut names = BufReader::new(spool.open(frame.run, 0)?);
    let result = (|| {
        while !sender.is_closed() {
            // Entry conversion uses lexical paths (including copy-links).
            // Revalidate every ancestor before resolving those paths, and on
            // EOF so an empty/replaced root is not a successful source scan.
            drop(reopen(root, &path, request.follow_symlinks, &mut stack)?);
            let Some(name) = read_name(&mut names)? else {
                drop(names);
                spool.remove(frame.run)?;
                if depth == 0 {
                    return Ok(());
                }
                depth -= 1;
                path.pop();
                stack.truncate(depth + 1)?;
                drop(directory);
                directory = reopen(root, &path, request.follow_symlinks, &mut stack)?;
                frame = stack.read(depth)?;
                names = BufReader::new(spool.open(frame.run, frame.offset)?);
                continue;
            };
            let relative = path.join(&name);
            let absolute = root.join(&relative);
            let metadata = entry_metadata(&absolute, request)?;
            if scratch[0].matches(&absolute, &metadata, request.follow_symlinks)?
                || scratch[1].matches(&absolute, &metadata, request.follow_symlinks)?
            {
                continue;
            }
            // WalkBuilder detects followed loops before ignore filtering and
            // even at max-depth. Ignoring a loop cannot turn a failed source
            // observation into a successful scan that authorizes deletion.
            if request.follow_symlinks
                && metadata.is_dir()
                && stack.contains(depth, path_identity(&absolute, &metadata, true)?)?
            {
                return Err(LocalScanError::SymlinkLoop(absolute));
            }
            if !request.include_git_dir && name == ".git" {
                continue;
            }
            // Match against lexical ancestry BEFORE descending. Re-rooting a
            // shallow walker here would canonicalize external link ancestry.
            if scope.source_match(&relative, metadata.is_dir())? {
                continue;
            }
            let entry = engine_entry(root, &absolute, &metadata, request)?;
            let child = if metadata.is_dir()
                && request.max_depth.is_none_or(|limit| depth + 1 < limit)
            {
                let child = directory.child(&name, request.follow_symlinks)?;
                let child_identity = child.identity()?;
                if child_identity != path_identity(&absolute, &metadata, request.follow_symlinks)? {
                    return Err(LocalScanError::DirectoryChanged(absolute));
                }
                Some(child)
            } else {
                None
            };
            if sender.blocking_send(Ok(entry)).is_err() {
                return Ok(());
            }
            if let Some(child) = child {
                frame.offset = names.stream_position()?;
                stack.write(depth, frame)?;
                drop(names);
                directory = child;
                path = relative;
                depth += 1;
                frame = prepare(&directory, &mut spool, sender)?;
                stack.write(depth, frame)?;
                names = BufReader::new(spool.open(frame.run, 0)?);
            }
        }
        Ok(())
    })();
    // Cleanup is explicit and fallible on successful completion AND errors /
    // cancellation. RAII remains the last resort if the process panics.
    let stack_cleanup = stack.0.close();
    let spool_cleanup = spool.close();
    let cleanup = match (stack_cleanup, spool_cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(stack), Err(spool)) => Err(io::Error::other(format!("stack: {stack}; runs: {spool}"))),
    };
    if let Err(source) = cleanup {
        return Err(LocalScanError::Cleanup {
            operation: result.err().map(Box::new),
            source,
        });
    }
    result
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);
#[cfg(unix)]
impl DirectoryStream {
    fn open(fd: std::os::fd::RawFd) -> io::Result<Self> {
        // SAFETY: fd is live; reopening '.' gives an independent directory
        // cursor rather than dup's shared open-file-description offset.
        let scan_fd = unsafe {
            libc::openat(
                fd,
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if scan_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: scan_fd is fresh; fdopendir owns it on success only.
        let dir = unsafe { libc::fdopendir(scan_fd) };
        if dir.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so scan_fd still belongs to us.
            unsafe {
                libc::close(scan_fd);
            }
            return Err(error);
        }
        Ok(Self(dir))
    }
}
#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns a live DIR pointer.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
#[cfg(unix)]
impl Iterator for DirectoryStream {
    type Item = io::Result<OsString>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            errno::set_errno(errno::Errno(0));
            // SAFETY: self exclusively owns this live DIR pointer.
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let errno = errno::errno().0;
                return (errno != 0).then(|| Err(io::Error::from_raw_os_error(errno)));
            }
            // SAFETY: successful readdir returns NUL-terminated d_name valid
            // until next readdir; copy the name before advancing.
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                use std::os::unix::ffi::OsStringExt;
                return Some(Ok(OsString::from_vec(name.to_vec())));
            }
        }
    }
}
