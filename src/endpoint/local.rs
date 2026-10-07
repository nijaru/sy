use crate::endpoint::{
    BoxReader, Capabilities, Endpoint, EndpointType, ExpectedDestination, FileMetadata,
    StagedWriter,
};
use crate::error::{Result, SyncError};
use async_trait::async_trait;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Local filesystem endpoint.
pub struct LocalEndpoint {
    root: PathBuf,
    capabilities: Capabilities,
    rooted: std::sync::Arc<tokio::sync::OnceCell<std::sync::Arc<sy::rooted_fs::RootedFs>>>,
}

impl LocalEndpoint {
    pub fn new(root: PathBuf) -> Self {
        let root = if root.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            root
        };
        Self {
            root,
            capabilities: Capabilities::local(),
            rooted: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    async fn rooted_fs(
        &self,
        create_root: bool,
    ) -> Result<std::sync::Arc<sy::rooted_fs::RootedFs>> {
        let root = self.root.clone();
        let rooted = self
            .rooted
            .get_or_try_init(|| async move {
                if create_root {
                    tokio::fs::create_dir_all(&root).await?;
                }
                let rooted = sy::rooted_fs::RootedFs::open(root)
                    .await
                    .map_err(map_rooted_fs_error)?;
                Ok::<_, SyncError>(std::sync::Arc::new(rooted))
            })
            .await?;
        Ok(std::sync::Arc::clone(rooted))
    }

    fn resolve(&self, relative: &Path) -> PathBuf {
        if relative.is_absolute() {
            relative.to_path_buf()
        } else {
            self.root.join(relative)
        }
    }
}

fn file_metadata_from_fs(meta: &fs::Metadata) -> FileMetadata {
    FileMetadata {
        size: meta.len(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        is_dir: meta.is_dir(),
        is_symlink: meta.is_symlink(),
        #[cfg(unix)]
        mode: meta.mode(),
    }
}

fn rooted_expected_destination(expected: ExpectedDestination) -> sy::endpoint::ExpectedDestination {
    match expected {
        ExpectedDestination::Absent => sy::endpoint::ExpectedDestination::Absent,
        ExpectedDestination::Unchanged(identity) => sy::endpoint::ExpectedDestination::Unchanged(
            sy::engine::domain::EntryIdentity::from_bytes(*identity.as_bytes()),
        ),
        ExpectedDestination::SnapshotAtOpen => sy::endpoint::ExpectedDestination::SnapshotAtOpen,
        ExpectedDestination::Unverified => sy::endpoint::ExpectedDestination::Unverified,
    }
}

fn map_rooted_fs_error(error: sy::rooted_fs::RootedFsError) -> SyncError {
    match error {
        sy::rooted_fs::RootedFsError::Io(error) => SyncError::Io(error),
        sy::rooted_fs::RootedFsError::DestinationChanged(path) => {
            SyncError::DestinationChanged { path }
        }
        sy::rooted_fs::RootedFsError::CommittedCleanupPending { path, reason } => {
            SyncError::CommittedCleanupPending { path, reason }
        }
        sy::rooted_fs::RootedFsError::CommittedParentChanged { path, reason } => {
            SyncError::CommittedParentChanged { path, reason }
        }
        sy::rooted_fs::RootedFsError::RootChanged(path) => SyncError::DestinationChanged { path },
        error => SyncError::Io(std::io::Error::other(error)),
    }
}

#[derive(Debug, Clone, Copy)]
enum DestinationObservation {
    Absent,
    Identified(sy::engine::domain::EntryIdentity),
    Unidentified,
}

async fn observe_destination(path: &Path) -> Result<DestinationObservation> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(
            match crate::endpoint::local_identity::identity_for_metadata(&metadata) {
                Some(identity) => DestinationObservation::Identified(identity),
                None => DestinationObservation::Unidentified,
            },
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(DestinationObservation::Absent)
        }
        Err(error) => Err(SyncError::Io(error)),
    }
}

fn expected_destination_at_open(
    path: &Path,
    expectation: ExpectedDestination,
    observed: DestinationObservation,
) -> Result<ExpectedDestination> {
    match (expectation, observed) {
        (ExpectedDestination::Absent, DestinationObservation::Absent) => {
            Ok(ExpectedDestination::Absent)
        }
        (ExpectedDestination::Absent, _) => Err(SyncError::DestinationChanged {
            path: path.to_path_buf(),
        }),
        (ExpectedDestination::Unchanged(expected), DestinationObservation::Identified(actual))
            if expected == actual =>
        {
            Ok(ExpectedDestination::Unchanged(expected))
        }
        (ExpectedDestination::Unchanged(_), _) => Err(SyncError::DestinationChanged {
            path: path.to_path_buf(),
        }),
        (ExpectedDestination::SnapshotAtOpen, DestinationObservation::Absent) => {
            Ok(ExpectedDestination::Absent)
        }
        (ExpectedDestination::SnapshotAtOpen, DestinationObservation::Identified(identity)) => {
            Ok(ExpectedDestination::Unchanged(identity))
        }
        (ExpectedDestination::SnapshotAtOpen, DestinationObservation::Unidentified)
        | (ExpectedDestination::Unverified, _) => Ok(ExpectedDestination::Unverified),
    }
}

pub(crate) async fn capture_destination_expectation(
    path: &Path,
    expectation: ExpectedDestination,
) -> Result<ExpectedDestination> {
    let observed = observe_destination(path).await?;
    expected_destination_at_open(path, expectation, observed)
}

pub(crate) async fn verify_destination_expectation(
    path: &Path,
    expectation: ExpectedDestination,
) -> Result<()> {
    let observed = observe_destination(path).await?;
    match (expectation, observed) {
        (ExpectedDestination::Absent, DestinationObservation::Absent)
        | (ExpectedDestination::Unverified, _) => Ok(()),
        (ExpectedDestination::Unchanged(expected), DestinationObservation::Identified(actual))
            if expected == actual =>
        {
            Ok(())
        }
        _ => Err(SyncError::DestinationChanged {
            path: path.to_path_buf(),
        }),
    }
}

/// Local staged writes share the rooted endpoint transaction owner.
struct LocalStagedWriter {
    rooted: std::sync::Arc<sy::rooted_fs::RootedFs>,
    root_path: PathBuf,
    destination_path: PathBuf,
    staged: Option<sy::rooted_fs::RootedStagedFile>,
    file: Option<tokio::fs::File>,
    #[cfg(test)]
    commit_queued: Option<tokio::sync::oneshot::Sender<()>>,
}

struct CommitCancellationGuard {
    state: Option<std::sync::Arc<std::sync::Mutex<bool>>>,
}

impl CommitCancellationGuard {
    fn new(state: std::sync::Arc<std::sync::Mutex<bool>>) -> Self {
        Self { state: Some(state) }
    }

    fn disarm(&mut self) {
        self.state = None;
    }
}

impl Drop for CommitCancellationGuard {
    fn drop(&mut self) {
        if let Some(state) = &self.state {
            *state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        }
    }
}

impl LocalStagedWriter {
    async fn new(
        rooted: std::sync::Arc<sy::rooted_fs::RootedFs>,
        relative: sy::engine::domain::RelativePath,
        expectation: ExpectedDestination,
        root_path: PathBuf,
        destination_path: PathBuf,
    ) -> Result<Self> {
        let parent = relative.parent();
        let rooted_for_staging = std::sync::Arc::clone(&rooted);
        let (staged, file) = tokio::task::spawn_blocking(move || {
            rooted_for_staging
                .verify_root_path_blocking()
                .map_err(map_rooted_fs_error)?;
            if let Some(parent) = parent {
                rooted_for_staging
                    .create_directories_blocking(&parent)
                    .map_err(map_rooted_fs_error)?;
            }
            let staged = rooted_for_staging
                .begin_staged_file_with_expectation_blocking(
                    &relative,
                    rooted_expected_destination(expectation),
                )
                .map_err(map_rooted_fs_error)?;
            let file = staged.try_clone_file().map_err(map_rooted_fs_error)?;
            Ok::<_, SyncError>((staged, file))
        })
        .await
        .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))??;

        Ok(Self {
            rooted,
            root_path,
            destination_path,
            staged: Some(staged),
            file: Some(tokio::fs::File::from_std(file)),
            #[cfg(test)]
            commit_queued: None,
        })
    }

    async fn with_staged<T, F>(&mut self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut sy::rooted_fs::RootedStagedFile) -> sy::rooted_fs::Result<T>
            + Send
            + 'static,
    {
        let mut staged = self
            .staged
            .take()
            .ok_or_else(|| SyncError::Config("staged writer is already closed".to_string()))?;
        let (staged, result) = tokio::task::spawn_blocking(move || {
            let result = operation(&mut staged);
            (staged, result)
        })
        .await
        .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        self.staged = Some(staged);
        result.map_err(map_rooted_fs_error)
    }

    fn file_mut(&mut self) -> Result<&mut tokio::fs::File> {
        self.file.as_mut().ok_or_else(|| {
            SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "staged writer is already closed",
            ))
        })
    }
}

/// Mirror a preservation set onto a local path, removing stale values.
///
/// Blocking syscall API shared by the endpoint methods, staged-writer
/// preservation, and the native transfer strategies so every commit path
/// applies identical semantics.
#[cfg(unix)]
pub(crate) fn write_xattrs_blocking(
    full_path: &Path,
    xattrs: &[(OsString, Vec<u8>)],
) -> Result<()> {
    let existing: Vec<OsString> = xattr::list(full_path)?.collect();

    for (name, value) in xattrs {
        xattr::set(full_path, name, value)?;
    }

    for name in existing {
        if !xattrs.iter().any(|(desired, _)| desired == &name) {
            match xattr::remove(full_path, &name) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        || error.raw_os_error() == Some(libc::EPERM)
                        || error.raw_os_error() == Some(libc::EACCES) => {}
                Err(error) => return Err(SyncError::Io(error)),
            }
        }
    }
    Ok(())
}

/// Replace a local path's ACL with `acl`'s exacl-unified text. Empty text
/// restores the mode-derived base entries.
#[cfg(all(unix, feature = "acl"))]
pub(crate) fn write_acl_blocking(full_path: &Path, acl: &str) -> Result<()> {
    use std::str::FromStr;

    let entries = if acl.is_empty() {
        #[cfg(target_os = "macos")]
        {
            Vec::new()
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(full_path)?.permissions().mode();
            exacl::from_mode(mode & 0o777)
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
        {
            Vec::new()
        }
    } else {
        acl.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                exacl::AclEntry::from_str(line).map_err(|error| {
                    SyncError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error.to_string(),
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?
    };

    exacl::setfacl(&[&full_path], &entries, None)?;
    Ok(())
}

/// Copy from a held source descriptor into the still-private staging inode.
/// macOS `fcopyfile` keeps the operating system's native copy path available
/// without reopening a user-visible destination pathname.
fn copy_native_file_blocking(
    source: &mut std::fs::File,
    destination: &mut std::fs::File,
) -> std::io::Result<u64> {
    use std::io::{Seek, SeekFrom};

    source.seek(SeekFrom::Start(0))?;
    destination.set_len(0)?;
    destination.seek(SeekFrom::Start(0))?;

    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: both descriptors refer to open regular files held for this
        // call. `state` must be null for the current fcopyfile implementation;
        // COPYFILE_DATA copies bytes only, leaving requested metadata and
        // preservation to the transaction finalizer.
        let result = unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                destination.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_DATA,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        destination.metadata().map(|metadata| metadata.len())
    }

    #[cfg(not(target_os = "macos"))]
    {
        std::io::copy(source, destination)
    }
}

#[cfg(unix)]
const SEEK_DATA: libc::c_int = libc::SEEK_DATA;
#[cfg(unix)]
const SEEK_HOLE: libc::c_int = libc::SEEK_HOLE;

#[cfg(unix)]
#[derive(Clone, Copy)]
enum ExtentSeek {
    Position(u64),
    End,
    Unsupported,
}

#[cfg(unix)]
fn seek_extent(
    fd: std::os::fd::RawFd,
    offset: u64,
    whence: libc::c_int,
) -> std::io::Result<ExtentSeek> {
    let offset = libc::off_t::try_from(offset).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file offset exceeds platform range",
        )
    })?;
    // SAFETY: `fd` is borrowed from the live held source file and `lseek` only
    // reads or updates that descriptor's file position for this operation.
    let result = unsafe { libc::lseek(fd, offset, whence) };
    if result >= 0 {
        return Ok(ExtentSeek::Position(u64::try_from(result).map_err(
            |_| std::io::Error::new(std::io::ErrorKind::InvalidData, "negative sparse extent"),
        )?));
    }

    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENXIO) => Ok(ExtentSeek::End),
        Some(code) if code == libc::EINVAL || code == libc::ENOTSUP || code == libc::EOPNOTSUPP => {
            Ok(ExtentSeek::Unsupported)
        }
        _ => Err(error),
    }
}

/// Quickly identify at least one reported hole without walking a fragmented
/// file's entire extent map. Unsupported or conservative maps select whole copy.
#[cfg(unix)]
fn sparse_extent_map_has_holes(fd: std::os::fd::RawFd, file_len: u64) -> std::io::Result<bool> {
    if file_len == 0 {
        return Ok(false);
    }
    let data = match seek_extent(fd, 0, SEEK_DATA)? {
        ExtentSeek::Position(data) if data < file_len => data,
        ExtentSeek::Position(_) | ExtentSeek::End | ExtentSeek::Unsupported => return Ok(false),
    };
    if data > 0 {
        return Ok(true);
    }
    match seek_extent(fd, data, SEEK_HOLE)? {
        ExtentSeek::Position(hole) => Ok(hole < file_len),
        ExtentSeek::End | ExtentSeek::Unsupported => Ok(false),
    }
}

/// Visit sparse data ranges with constant memory. The callback is invoked in
/// ascending file-offset order; `None` reports that SEEK_DATA/SEEK_HOLE cannot
/// provide a trustworthy map on this filesystem.
#[cfg(unix)]
fn visit_sparse_extents(
    fd: std::os::fd::RawFd,
    file_len: u64,
    mut visit: impl FnMut(u64, u64) -> std::io::Result<()>,
) -> std::io::Result<Option<bool>> {
    if file_len == 0 {
        return Ok(Some(false));
    }
    let mut cursor = 0_u64;
    let mut saw_data = false;
    let mut has_holes = false;
    loop {
        let data = match seek_extent(fd, cursor, SEEK_DATA)? {
            ExtentSeek::Position(data) if data < file_len => data,
            ExtentSeek::Position(_) | ExtentSeek::Unsupported => return Ok(None),
            ExtentSeek::End if !saw_data => return Ok(None),
            ExtentSeek::End => break,
        };
        if data < cursor {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sparse data extents are not ordered",
            ));
        }
        has_holes |= data > cursor;

        let end = match seek_extent(fd, data, SEEK_HOLE)? {
            ExtentSeek::Position(end) => end.min(file_len),
            ExtentSeek::End => file_len,
            ExtentSeek::Unsupported => return Ok(None),
        };
        if end <= data {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sparse extent did not advance",
            ));
        }
        has_holes |= end < file_len;
        visit(data, end)?;
        saw_data = true;
        cursor = end;
        if cursor == file_len {
            break;
        }
    }
    Ok(Some(has_holes))
}

#[cfg(unix)]
fn copy_sparse_native_file_blocking(
    source: &mut std::fs::File,
    destination: &mut std::fs::File,
) -> std::io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    let file_len = source.metadata()?.len();
    let fd = source.as_raw_fd();
    let sparse = visit_sparse_extents(fd, file_len, |_, _| Ok(()))?;
    if sparse != Some(true) {
        return Ok(None);
    }

    destination.set_len(0)?;
    destination.set_len(file_len)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut bytes_written = 0_u64;
    let second_pass = visit_sparse_extents(fd, file_len, |start, end| {
        source.seek(SeekFrom::Start(start))?;
        destination.seek(SeekFrom::Start(start))?;
        let mut remaining = end - start;
        while remaining > 0 {
            let amount = remaining.min(buffer.len() as u64) as usize;
            let read = source.read(&mut buffer[..amount])?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "source changed while copying sparse extent",
                ));
            }
            destination.write_all(&buffer[..read])?;
            remaining -= read as u64;
            bytes_written += read as u64;
        }
        Ok(())
    })?;
    if second_pass != Some(true) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "source sparse extents changed during copy",
        ));
    }
    destination.flush()?;
    Ok(Some(bytes_written))
}

/// Clone the held destination basis into rooted staging, then patch changed
/// ranges from the held source. Linux FICLONE accepts file descriptors, so no
/// visible pathname is reopened by this optimization.
#[cfg(target_os = "linux")]
fn reflink_patch_native_files_blocking(
    source: &mut std::fs::File,
    destination_basis: &mut std::fs::File,
    staged: &mut std::fs::File,
    source_size: u64,
) -> std::io::Result<Option<u64>> {
    use std::io::{Seek, SeekFrom};
    use std::os::fd::AsRawFd;

    // FICLONE is a fixed 32-bit request code; libc::Ioctl differs between
    // glibc and musl, while the kernel compares the low 32 bits.
    const FICLONE: libc::Ioctl = 0x4004_9409_u32 as libc::Ioctl;

    staged.set_len(0)?;
    let result = unsafe {
        // SAFETY: both descriptors are held regular files. `destination_basis`
        // is open for reading and `staged` is a distinct writable private inode,
        // as required by FICLONE.
        libc::ioctl(staged.as_raw_fd(), FICLONE, destination_basis.as_raw_fd())
    };
    if result < 0 {
        // A failed clone must leave staging ready for a different strategy.
        staged.set_len(0)?;
        staged.seek(SeekFrom::Start(0))?;
        return Ok(None);
    }

    patch_native_reflink_ranges_blocking(source, destination_basis, staged, source_size).map(Some)
}

#[cfg(target_os = "linux")]
fn patch_native_reflink_ranges_blocking(
    source: &mut std::fs::File,
    destination_basis: &mut std::fs::File,
    staged: &mut std::fs::File,
    source_size: u64,
) -> std::io::Result<u64> {
    use std::io::{Seek, SeekFrom, Write};

    source.seek(SeekFrom::Start(0))?;
    destination_basis.seek(SeekFrom::Start(0))?;
    let mut source_buffer = vec![0_u8; 1024 * 1024];
    let mut basis_buffer = vec![0_u8; 1024 * 1024];
    let mut offset = 0_u64;
    let mut bytes_written = 0_u64;
    loop {
        let source_read = read_up_to(source, &mut source_buffer)?;
        if source_read == 0 {
            break;
        }
        let basis_read = read_up_to(destination_basis, &mut basis_buffer)?;
        if source_read != basis_read || source_buffer[..source_read] != basis_buffer[..basis_read] {
            staged.seek(SeekFrom::Start(offset))?;
            staged.write_all(&source_buffer[..source_read])?;
            bytes_written = bytes_written
                .checked_add(u64::try_from(source_read).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "reflink patch byte count exceeds u64",
                    )
                })?)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "reflink patch byte count overflow",
                    )
                })?;
        }
        offset = offset
            .checked_add(u64::try_from(source_read).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "reflink source offset exceeds u64",
                )
            })?)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "reflink offset overflow")
            })?;
    }
    if offset != source_size || source.metadata()?.len() != source_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "source changed while applying reflink patch",
        ));
    }

    staged.set_len(source_size)?;
    staged.flush()?;
    Ok(bytes_written)
}

/// Read a full buffer unless EOF arrives. Regular files can still return short
/// reads, and patch comparison must keep source and basis offsets aligned.
#[cfg(target_os = "linux")]
fn read_up_to(file: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;

    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Apply a preservation payload to one local staging path before commit.
pub(crate) fn apply_preservation_blocking(
    full_path: &Path,
    preservation: &crate::endpoint::io::Preservation,
) -> Result<()> {
    if let Some(xattrs) = &preservation.xattrs {
        #[cfg(unix)]
        write_xattrs_blocking(full_path, xattrs)?;
        #[cfg(not(unix))]
        {
            let _ = (full_path, xattrs);
            return Err(SyncError::Config(
                "local extended attributes are unsupported on this platform".to_string(),
            ));
        }
    }
    if let Some(acl) = &preservation.acl {
        #[cfg(all(unix, feature = "acl"))]
        write_acl_blocking(full_path, acl)?;
        #[cfg(not(all(unix, feature = "acl")))]
        {
            let _ = (full_path, acl);
            return Err(SyncError::Config(
                "local ACL preservation requires the acl feature on Unix".to_string(),
            ));
        }
    }
    Ok(())
}

#[async_trait]
impl StagedWriter for LocalStagedWriter {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file_mut()?.write_all(data).await?;
        Ok(())
    }

    async fn set_metadata(&mut self, metadata: &FileMetadata) -> Result<()> {
        self.file_mut()?.flush().await?;
        let time = filetime::FileTime::from_system_time(metadata.modified);
        let modified = sy::engine::domain::Timestamp::new(time.seconds(), time.nanoseconds())
            .map_err(|error| SyncError::Config(error.to_string()))?;
        #[cfg(unix)]
        let unix_mode = Some(metadata.mode);
        #[cfg(not(unix))]
        let unix_mode = None;
        self.with_staged(move |staged| staged.apply_metadata_blocking(unix_mode, Some(modified)))
            .await
    }

    async fn staged_hash(&mut self) -> Result<Option<blake3::Hash>> {
        self.file_mut()?.flush().await?;
        self.with_staged(|staged| staged.staged_hash_blocking())
            .await
            .map(Some)
    }

    async fn copy_from_native_file(
        &mut self,
        mut source: std::fs::File,
    ) -> Result<(std::fs::File, u64)> {
        self.file_mut()?.flush().await?;
        self.with_staged(move |staged| {
            let bytes_written = copy_native_file_blocking(&mut source, staged.file_mut())?;
            Ok((source, bytes_written))
        })
        .await
    }

    async fn copy_sparse_from_native_file(
        &mut self,
        mut source: std::fs::File,
    ) -> Result<(std::fs::File, Option<u64>)> {
        #[cfg(unix)]
        {
            self.file_mut()?.flush().await?;
            self.with_staged(move |staged| {
                let bytes_written =
                    copy_sparse_native_file_blocking(&mut source, staged.file_mut())?;
                Ok((source, bytes_written))
            })
            .await
        }
        #[cfg(not(unix))]
        {
            Ok((source, None))
        }
    }

    #[cfg(target_os = "linux")]
    async fn reflink_patch_from_native_files(
        &mut self,
        mut source: std::fs::File,
        mut destination_basis: std::fs::File,
        source_size: u64,
    ) -> Result<(std::fs::File, std::fs::File, Option<u64>)> {
        self.file_mut()?.flush().await?;
        self.with_staged(move |staged| {
            let bytes_written = reflink_patch_native_files_blocking(
                &mut source,
                &mut destination_basis,
                staged.file_mut(),
                source_size,
            )?;
            Ok((source, destination_basis, bytes_written))
        })
        .await
    }

    async fn apply_preservation(
        &mut self,
        preservation: &crate::endpoint::io::Preservation,
        expected_mode: Option<u32>,
    ) -> Result<()> {
        self.file_mut()?.flush().await?;
        let preservation = preservation.clone();
        self.with_staged(move |staged| {
            staged.apply_preservation_blocking(
                preservation.xattrs.as_deref(),
                preservation.acl.as_deref(),
                expected_mode,
            )
        })
        .await
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            if let Err(operation) = file.flush().await {
                drop(file);
                return match self.abort().await {
                    Ok(()) => Err(SyncError::Io(operation)),
                    Err(abort) => Err(SyncError::StagingAbortFailed {
                        operation: operation.to_string(),
                        abort: abort.to_string(),
                    }),
                };
            }
            drop(file);
        }
        let staged = self
            .staged
            .take()
            .ok_or_else(|| SyncError::Config("staged writer is already closed".to_string()))?;
        let cancellation = std::sync::Arc::new(std::sync::Mutex::new(false));
        let worker_cancellation = std::sync::Arc::clone(&cancellation);
        let rooted = std::sync::Arc::clone(&self.rooted);
        let root_path = self.root_path.clone();
        let destination_path = self.destination_path.clone();
        let mut cancellation_guard = CommitCancellationGuard::new(cancellation);
        let worker = tokio::task::spawn_blocking(move || -> Result<()> {
            if let Err(operation) = rooted.verify_root_path_blocking() {
                let operation = map_rooted_fs_error(operation);
                return match staged.abort() {
                    Ok(()) => Err(operation),
                    Err(abort) => Err(SyncError::StagingAbortFailed {
                        operation: operation.to_string(),
                        abort: abort.to_string(),
                    }),
                };
            }
            staged
                .commit_cancellable(worker_cancellation)
                .map_err(map_rooted_fs_error)?;
            match rooted.verify_root_path_blocking() {
                Ok(()) => Ok(()),
                Err(error) => Err(SyncError::CommittedRootChanged {
                    destination: destination_path,
                    root: root_path,
                    reason: error.to_string(),
                }),
            }
        });
        #[cfg(test)]
        if let Some(queued) = self.commit_queued.take() {
            let _ = queued.send(());
        }
        let result = worker
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        cancellation_guard.disarm();
        result
    }

    async fn abort(mut self: Box<Self>) -> Result<()> {
        self.file.take();
        let staged = self.staged.take().ok_or_else(|| {
            SyncError::Io(std::io::Error::other(
                "staged writer lost its transaction before explicit abort",
            ))
        })?;
        tokio::task::spawn_blocking(move || staged.abort())
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
            .map_err(map_rooted_fs_error)
    }
}

#[async_trait]
impl Endpoint for LocalEndpoint {
    fn endpoint_type(&self) -> EndpointType {
        EndpointType::Local
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn native_path(&self, path: &Path) -> Option<PathBuf> {
        Some(self.resolve(path))
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match tokio::fs::symlink_metadata(self.resolve(path)).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn metadata(&self, path: &Path) -> Result<FileMetadata> {
        let meta = tokio::fs::symlink_metadata(self.resolve(path)).await?;
        Ok(file_metadata_from_fs(&meta))
    }

    async fn metadata_following(&self, path: &Path) -> Result<FileMetadata> {
        // std::fs::metadata follows links; a dangling target errors loudly.
        let meta = tokio::fs::metadata(self.resolve(path)).await?;
        Ok(file_metadata_from_fs(&meta))
    }

    async fn read_xattrs(&self, path: &Path) -> Result<Vec<(OsString, Vec<u8>)>> {
        #[cfg(unix)]
        {
            let full_path = self.resolve(path);
            return tokio::task::spawn_blocking(move || -> Result<Vec<(OsString, Vec<u8>)>> {
                let mut result = Vec::new();
                for name in xattr::list(&full_path)? {
                    if let Some(value) = xattr::get(&full_path, &name)? {
                        result.push((name, value));
                    }
                }
                Ok(result)
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            Err(SyncError::Config(
                "local extended attributes are unsupported on this platform".to_string(),
            ))
        }
    }

    async fn write_xattrs(&self, path: &Path, xattrs: &[(OsString, Vec<u8>)]) -> Result<()> {
        #[cfg(unix)]
        {
            let full_path = self.resolve(path);
            let xattrs = xattrs.to_vec();
            return tokio::task::spawn_blocking(move || write_xattrs_blocking(&full_path, &xattrs))
                .await
                .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(unix))]
        {
            let _ = (path, xattrs);
            Err(SyncError::Config(
                "local extended attributes are unsupported on this platform".to_string(),
            ))
        }
    }

    async fn read_acl(&self, path: &Path) -> Result<Option<String>> {
        #[cfg(all(unix, feature = "acl"))]
        {
            let full_path = self.resolve(path);
            return tokio::task::spawn_blocking(move || -> Result<Option<String>> {
                let entries = exacl::getfacl(&full_path, None)?;
                if entries.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(
                        entries
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ))
                }
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(all(unix, feature = "acl")))]
        {
            let _ = path;
            Err(SyncError::Config(
                "local ACL preservation requires the acl feature on Unix".to_string(),
            ))
        }
    }

    async fn write_acl(&self, path: &Path, acl: &str) -> Result<()> {
        #[cfg(all(unix, feature = "acl"))]
        {
            let full_path = self.resolve(path);
            let acl = acl.to_string();
            return tokio::task::spawn_blocking(move || write_acl_blocking(&full_path, &acl))
                .await
                .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(all(unix, feature = "acl")))]
        {
            let _ = (path, acl);
            Err(SyncError::Config(
                "local ACL preservation requires the acl feature on Unix".to_string(),
            ))
        }
    }

    async fn read_bsd_flags(&self, path: &Path) -> Result<Option<u32>> {
        #[cfg(target_os = "macos")]
        {
            let full_path = self.resolve(path);
            return tokio::task::spawn_blocking(move || -> Result<Option<u32>> {
                use std::os::macos::fs::MetadataExt;
                Ok(Some(std::fs::symlink_metadata(full_path)?.st_flags()))
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            Err(SyncError::Config(
                "BSD flags are only supported on macOS".to_string(),
            ))
        }
    }

    async fn write_bsd_flags(&self, path: &Path, flags: u32) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;

            let full_path = self.resolve(path);
            return tokio::task::spawn_blocking(move || -> Result<()> {
                let path =
                    std::ffi::CString::new(full_path.as_os_str().as_bytes()).map_err(|_| {
                        SyncError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "NUL in path",
                        ))
                    })?;

                // SAFETY: `path` is a live, NUL-terminated CString for the full
                // local destination path. `chflags` only borrows the pointer for
                // the duration of this call.
                let result = unsafe { libc::chflags(path.as_ptr(), flags as _) };
                if result == 0 {
                    Ok(())
                } else {
                    Err(SyncError::Io(std::io::Error::last_os_error()))
                }
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (path, flags);
            Err(SyncError::Config(
                "BSD flags are only supported on macOS".to_string(),
            ))
        }
    }

    async fn open_reader(&self, path: &Path) -> Result<BoxReader> {
        let file = self.open_native_file(path).await?.ok_or_else(|| {
            SyncError::Config("local endpoint cannot provide a rooted source reader".to_string())
        })?;
        Ok(Box::pin(tokio::fs::File::from_std(file)))
    }

    async fn open_native_file(&self, path: &Path) -> Result<Option<std::fs::File>> {
        #[cfg(unix)]
        {
            let relative = sy::engine::domain::RelativePath::new(path.to_path_buf())
                .map_err(|error| SyncError::Config(error.to_string()))?;
            // Unlike destination preparation, opening a source must not create
            // a missing root as a side effect.
            let rooted = self.rooted_fs(false).await?;
            return tokio::task::spawn_blocking(move || {
                rooted
                    .verify_root_path_blocking()
                    .map_err(map_rooted_fs_error)?;
                let file = rooted
                    .open_regular_blocking(&relative)
                    .map_err(map_rooted_fs_error)?;
                rooted
                    .verify_root_path_blocking()
                    .map_err(map_rooted_fs_error)?;
                Ok(Some(file))
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(None)
        }
    }

    async fn open_native_file_following(&self, path: &Path) -> Result<Option<std::fs::File>> {
        #[cfg(unix)]
        {
            let relative = sy::engine::domain::RelativePath::new(path.to_path_buf())
                .map_err(|error| SyncError::Config(error.to_string()))?;
            let full_path = self.resolve(relative.as_path());
            let rooted = self.rooted_fs(false).await?;
            return tokio::task::spawn_blocking(move || {
                rooted
                    .verify_root_path_blocking()
                    .map_err(map_rooted_fs_error)?;
                // --copy-links explicitly follows the leaf (and intermediate)
                // symlinks. O_NONBLOCK prevents a raced FIFO replacement from
                // pinning this worker before descriptor-type validation.
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
                let file = options.open(&full_path)?;
                if !file.metadata()?.is_file() {
                    return Err(SyncError::Config(
                        "followed native transfer source is not a regular file".to_string(),
                    ));
                }
                use std::os::fd::AsRawFd;
                let fd = file.as_raw_fd();
                // SAFETY: `fd` is a live open descriptor.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags >= 0 {
                    // SAFETY: `fd` is live; clear O_NONBLOCK so subsequent reads use normal blocking I/O.
                    unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
                }
                rooted
                    .verify_root_path_blocking()
                    .map_err(map_rooted_fs_error)?;
                Ok(Some(file))
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(None)
        }
    }

    async fn read_open_file_preservation(
        &self,
        path: &Path,
        file: &std::fs::File,
        request: crate::endpoint::io::PreservationRequest,
    ) -> Result<crate::endpoint::io::Preservation> {
        if !request.xattrs && !request.acl {
            return Ok(crate::endpoint::io::Preservation::default());
        }
        let relative = sy::engine::domain::RelativePath::new(path.to_path_buf())
            .map_err(|error| SyncError::Config(error.to_string()))?;
        let file = file.try_clone()?;
        let rooted = self.rooted_fs(false).await?;
        tokio::task::spawn_blocking(move || {
            rooted
                .verify_root_path_blocking()
                .map_err(map_rooted_fs_error)?;
            let xattrs = request
                .xattrs
                .then(|| rooted.read_open_file_xattrs_blocking(&file, &relative))
                .transpose()
                .map_err(map_rooted_fs_error)?;
            let acl = request
                .acl
                .then(|| rooted.read_open_file_acl_blocking(&file, &relative))
                .transpose()
                .map_err(map_rooted_fs_error)?
                .map(|acl| acl.unwrap_or_default());
            Ok(crate::endpoint::io::Preservation { xattrs, acl })
        })
        .await
        .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
    }

    async fn native_file_has_sparse_holes(&self, file: &std::fs::File) -> Result<bool> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let file = file.try_clone()?;
            let has_holes = tokio::task::spawn_blocking(move || {
                let file_len = file.metadata()?.len();
                sparse_extent_map_has_holes(file.as_raw_fd(), file_len)
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
            .map_err(SyncError::Io)?;
            return Ok(has_holes);
        }

        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(false)
        }
    }

    async fn begin_write(
        &self,
        path: &Path,
        expected_destination: ExpectedDestination,
    ) -> Result<Box<dyn StagedWriter>> {
        let relative = sy::engine::domain::RelativePath::new(path.to_path_buf())
            .map_err(|error| SyncError::Config(error.to_string()))?;
        let rooted = self.rooted_fs(true).await?;
        let destination_path = self.root.join(relative.as_path());
        Ok(Box::new(
            LocalStagedWriter::new(
                rooted,
                relative,
                expected_destination,
                self.root.clone(),
                destination_path,
            )
            .await?,
        ))
    }

    async fn remove(&self, path: &Path, recursive: bool) -> Result<()> {
        let full_path = self.resolve(path);
        let meta = tokio::fs::symlink_metadata(&full_path).await?;
        if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&full_path).await?;
            } else {
                tokio::fs::remove_dir(&full_path).await?;
            }
        } else {
            tokio::fs::remove_file(&full_path).await?;
        }
        Ok(())
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        tokio::fs::create_dir_all(self.resolve(path)).await?;
        Ok(())
    }

    async fn replace_symlink(
        &self,
        target: &Path,
        dest: &Path,
        expected: ExpectedDestination,
        modified: Option<crate::engine::domain::Timestamp>,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            let rel = sy::engine::domain::RelativePath::new(dest.to_path_buf())
                .map_err(|error| SyncError::Config(error.to_string()))?;

            let rooted = self.rooted_fs(true).await?;
            let target = target.to_path_buf();
            tokio::task::spawn_blocking(move || {
                if let Some(parent) = rel.parent() {
                    rooted
                        .create_directories_blocking(&parent)
                        .map_err(map_rooted_fs_error)?;
                }
                rooted
                    .replace_symlink_blocking(
                        &rel,
                        &target,
                        rooted_expected_destination(expected),
                        modified,
                    )
                    .map_err(map_rooted_fs_error)
            })
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
        }
        #[cfg(not(unix))]
        {
            let _ = (target, dest, expected, modified);
            Err(SyncError::Config(
                "symlink creation is not implemented for this platform".to_string(),
            ))
        }
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        let full_source = self.resolve(source);
        let full_dest = self.resolve(dest);
        if let Some(parent) = full_dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let temp = crate::temp_file::TempFileGuard::temp_path_for(&full_dest);
        let guard = crate::temp_file::TempFileGuard::new(&temp);
        tokio::fs::hard_link(&full_source, &temp).await?;
        tokio::fs::rename(&temp, &full_dest).await?;
        guard.defuse();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_meta() -> FileMetadata {
        FileMetadata {
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            #[cfg(unix)]
            mode: 0o644,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reflink_clone_uses_held_files_or_leaves_staging_empty() {
        const BUFFER: usize = 1024 * 1024;

        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source");
        let basis_path = dir.path().join("basis");
        let staged_path = dir.path().join("staged");
        let mut source = vec![b'a'; BUFFER + 1];
        source[BUFFER] = b'b';
        let basis = vec![b'a'; source.len()];
        fs::write(&source_path, &source).unwrap();
        fs::write(&basis_path, &basis).unwrap();
        fs::write(&staged_path, b"stale staging bytes").unwrap();

        let mut source_file = fs::File::open(source_path).unwrap();
        let mut basis_file = fs::File::open(basis_path).unwrap();
        let mut staged_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staged_path)
            .unwrap();
        let result = reflink_patch_native_files_blocking(
            &mut source_file,
            &mut basis_file,
            &mut staged_file,
            source.len() as u64,
        )
        .unwrap();
        drop(staged_file);

        match result {
            Some(bytes_written) => {
                assert_eq!(bytes_written, 1);
                assert_eq!(fs::read(staged_path).unwrap(), source);
            }
            None => assert_eq!(fs::metadata(staged_path).unwrap().len(), 0),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn held_reflink_patch_matches_source_across_changed_and_equal_blocks() {
        const BUFFER: usize = 1024 * 1024;

        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source");
        let basis_path = dir.path().join("basis");
        let staged_path = dir.path().join("staged");
        let mut source = vec![b'a'; 2 * BUFFER + 11];
        source[7] = b'b';
        source[2 * BUFFER + 10] = b'c';
        let basis = vec![b'a'; source.len()];
        fs::write(&source_path, &source).unwrap();
        fs::write(&basis_path, &basis).unwrap();
        fs::write(&staged_path, &basis).unwrap();

        let mut source_file = fs::File::open(source_path).unwrap();
        let mut basis_file = fs::File::open(basis_path).unwrap();
        let mut staged_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staged_path)
            .unwrap();
        let bytes_written = patch_native_reflink_ranges_blocking(
            &mut source_file,
            &mut basis_file,
            &mut staged_file,
            source.len() as u64,
        )
        .unwrap();
        drop(staged_file);

        assert_eq!(bytes_written, (BUFFER + 11) as u64);
        assert_eq!(fs::read(staged_path).unwrap(), source);
    }

    #[test]
    fn rooted_io_error_keeps_its_kind_when_mapped() {
        let operation = sy::rooted_fs::RootedFsError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "source changed during sparse copy",
        ));
        let SyncError::Io(error) = map_rooted_fs_error(operation) else {
            panic!("rooted I/O error was mapped to the wrong error kind");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_source_open_is_rooted_and_does_not_create_roots() {
        let dir = TempDir::new().unwrap();
        let missing_root = dir.path().join("missing");
        let missing_endpoint = LocalEndpoint::new(missing_root.clone());
        assert!(missing_endpoint
            .open_native_file(Path::new("file"))
            .await
            .is_err());
        assert!(missing_endpoint
            .open_reader(Path::new("file"))
            .await
            .is_err());
        assert!(!missing_root.exists());

        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let outside_dir = dir.path().join("outside-dir");
        fs::create_dir(&outside_dir).unwrap();
        fs::write(outside_dir.join("file"), b"outside").unwrap();
        std::os::unix::fs::symlink(&outside_dir, root.join("ancestor")).unwrap();
        let endpoint = LocalEndpoint::new(root);
        for path in ["link", "ancestor/file"] {
            assert!(endpoint.open_native_file(Path::new(path)).await.is_err());
            assert!(endpoint.open_reader(Path::new(path)).await.is_err());
        }
    }

    #[tokio::test]
    async fn staged_write_creates_missing_root_and_parent_directories_confined() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("new-root");
        let endpoint = LocalEndpoint::new(root.clone());
        let mut writer = endpoint
            .begin_write(Path::new("nested/dir/file"), ExpectedDestination::Absent)
            .await
            .unwrap();
        writer.write(b"content").await.unwrap();
        writer.commit().await.unwrap();

        assert_eq!(fs::read(root.join("nested/dir/file")).unwrap(), b"content");
    }

    #[test]
    fn cancelling_queued_commit_preserves_destination() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"old").unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
            let rooted = endpoint.rooted_fs(true).await.unwrap();
            let relative = sy::engine::domain::RelativePath::new(PathBuf::from("file")).unwrap();
            let mut writer = LocalStagedWriter::new(
                rooted,
                relative,
                ExpectedDestination::SnapshotAtOpen,
                dir.path().to_path_buf(),
                dir.path().join("file"),
            )
            .await
            .unwrap();
            writer.write(b"new").await.unwrap();
            let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
            writer.commit_queued = Some(queued_tx);

            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();

            let commit = tokio::spawn(async move { Box::new(writer).commit().await });
            queued_rx.await.unwrap();
            commit.abort();
            assert!(commit.await.unwrap_err().is_cancelled());
            release_tx.send(()).unwrap();
            blocker.await.unwrap();

            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if fs::read_dir(dir.path()).unwrap().count() == 1 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"old");
        });
    }

    #[tokio::test]
    async fn rooted_local_writer_refuses_root_replacement_during_transaction() {
        let parent = TempDir::new().unwrap();
        let root = parent.path().join("root");
        let moved = parent.path().join("moved-root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"old").unwrap();
        let endpoint = LocalEndpoint::new(root.clone());
        let mut writer = endpoint
            .begin_write(Path::new("file"), ExpectedDestination::SnapshotAtOpen)
            .await
            .unwrap();
        writer.write(b"new").await.unwrap();

        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        assert!(matches!(
            writer.commit().await,
            Err(SyncError::DestinationChanged { .. })
        ));
        assert_eq!(fs::read(moved.join("file")).unwrap(), b"old");
        assert!(!root.join("file").exists());
        assert_eq!(fs::read_dir(&moved).unwrap().count(), 1);
        assert!(matches!(
            endpoint
                .begin_write(Path::new("another"), ExpectedDestination::Absent)
                .await,
            Err(SyncError::DestinationChanged { .. })
        ));
    }

    #[tokio::test]
    async fn staged_abort_preserves_destination() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"old").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut writer = endpoint
            .begin_write(Path::new("file"), ExpectedDestination::SnapshotAtOpen)
            .await
            .unwrap();
        writer.write(b"new").await.unwrap();
        writer.abort().await.unwrap();
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn staged_hash_reads_uncommitted_bytes() {
        let dir = TempDir::new().unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut writer = endpoint
            .begin_write(Path::new("file"), ExpectedDestination::Absent)
            .await
            .unwrap();
        writer.write(b"content").await.unwrap();
        let hash = writer.staged_hash().await.unwrap().unwrap();
        assert_eq!(hash, blake3::hash(b"content"));
        writer.abort().await.unwrap();
        assert!(!dir.path().join("file").exists());
    }

    #[tokio::test]
    async fn common_finalizer_verifies_staging_before_publication() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing"), b"old").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let metadata = make_meta();
        let preservation = crate::endpoint::io::Preservation::default();

        let mut writer = endpoint
            .begin_write(Path::new("existing"), ExpectedDestination::SnapshotAtOpen)
            .await
            .unwrap();
        writer.write(b"content").await.unwrap();
        let mismatch = crate::endpoint::io::finalize_staged_writer(
            writer,
            &metadata,
            &preservation,
            Some(blake3::hash(b"different")),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            mismatch,
            crate::endpoint::io::VerificationStatus::Failed { .. }
        ));
        assert_eq!(fs::read(dir.path().join("existing")).unwrap(), b"old");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        let mut writer = endpoint
            .begin_write(Path::new("created"), ExpectedDestination::Absent)
            .await
            .unwrap();
        writer.write(b"content").await.unwrap();
        let verified = crate::endpoint::io::finalize_staged_writer(
            writer,
            &metadata,
            &preservation,
            Some(blake3::hash(b"content")),
            None,
        )
        .await
        .unwrap();
        assert_eq!(verified, crate::endpoint::io::VerificationStatus::Verified);
        assert_eq!(fs::read(dir.path().join("created")).unwrap(), b"content");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finalizer_hashes_staging_with_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut writer = endpoint
            .begin_write(Path::new("private"), ExpectedDestination::Absent)
            .await
            .unwrap();
        writer.write(b"private bytes").await.unwrap();
        let mut metadata = make_meta();
        metadata.mode = 0;

        let result = crate::endpoint::io::finalize_staged_writer(
            writer,
            &metadata,
            &crate::endpoint::io::Preservation::default(),
            Some(blake3::hash(b"private bytes")),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result, crate::endpoint::io::VerificationStatus::Verified);
        let path = dir.path().join("private");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"private bytes");
    }

    #[tokio::test]
    async fn staged_commit_replaces_destination() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"old").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut writer = endpoint
            .begin_write(Path::new("file"), ExpectedDestination::SnapshotAtOpen)
            .await
            .unwrap();
        writer.write(b"content").await.unwrap();
        writer.set_metadata(&make_meta()).await.unwrap();
        writer.commit().await.unwrap();
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"content");
    }

    #[tokio::test]
    async fn staged_commit_preserves_a_concurrent_destination_replacement() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"scanned").unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let identity = crate::endpoint::local_identity::identity_for_metadata(&metadata).unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut writer = endpoint
            .begin_write(Path::new("file"), ExpectedDestination::Unchanged(identity))
            .await
            .unwrap();
        writer.write(b"replacement").await.unwrap();

        fs::remove_file(&path).unwrap();
        fs::write(&path, b"concurrent edit").unwrap();
        let error = match writer.commit().await {
            Ok(()) => panic!("concurrent destination replacement must abort"),
            Err(error) => error,
        };

        assert!(matches!(error, SyncError::DestinationChanged { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"concurrent edit");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn staged_create_refuses_a_destination_appearing_before_commit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file");
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut writer = endpoint
            .begin_write(Path::new("file"), ExpectedDestination::Absent)
            .await
            .unwrap();
        writer.write(b"staged").await.unwrap();

        fs::write(&path, b"concurrent create").unwrap();
        let error = match writer.commit().await {
            Ok(()) => panic!("a path appearing after scan must abort"),
            Err(error) => error,
        };

        assert!(matches!(error, SyncError::DestinationChanged { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"concurrent create");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn followed_native_open_rejects_a_fifo_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        use std::time::Duration;

        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is NUL-terminated and points to a valid temporary
        // pathname; mkfifo only reads it for the duration of this call.
        let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(result, 0);
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            endpoint.open_native_file_following(Path::new("fifo")),
        )
        .await
        .expect("opening a FIFO must not block")
        .unwrap_err();
        assert!(matches!(result, SyncError::Config(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_file_preservation_ignores_a_replacement_at_the_source_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"original").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        endpoint
            .write_xattrs(
                Path::new("file"),
                &[(OsString::from("user.sy-bound"), b"original".to_vec())],
            )
            .await
            .unwrap();
        let opened = endpoint
            .open_native_file(Path::new("file"))
            .await
            .unwrap()
            .unwrap();

        fs::rename(&path, dir.path().join("original-file")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        endpoint
            .write_xattrs(
                Path::new("file"),
                &[(OsString::from("user.sy-bound"), b"replacement".to_vec())],
            )
            .await
            .unwrap();

        let preservation = endpoint
            .read_open_file_preservation(
                Path::new("file"),
                &opened,
                crate::endpoint::io::PreservationRequest {
                    xattrs: true,
                    acl: false,
                },
            )
            .await
            .unwrap();
        let user_xattrs = preservation
            .xattrs
            .unwrap()
            .into_iter()
            .filter(|(name, _)| name.to_string_lossy().starts_with("user."))
            .collect::<Vec<_>>();
        assert_eq!(
            user_xattrs,
            vec![(OsString::from("user.sy-bound"), b"original".to_vec())]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_xattrs_round_trip_and_remove_stale_values() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"content").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());

        endpoint
            .write_xattrs(
                Path::new("file"),
                &[(OsString::from("user.sy-test"), b"first".to_vec())],
            )
            .await
            .unwrap();
        let user_xattrs = |attrs: Vec<(OsString, Vec<u8>)>| {
            attrs
                .into_iter()
                .filter(|(name, _)| name.to_string_lossy().starts_with("user."))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            user_xattrs(endpoint.read_xattrs(Path::new("file")).await.unwrap()),
            vec![(OsString::from("user.sy-test"), b"first".to_vec())]
        );

        endpoint.write_xattrs(Path::new("file"), &[]).await.unwrap();
        assert!(user_xattrs(endpoint.read_xattrs(Path::new("file")).await.unwrap()).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_update_is_atomic_at_endpoint_boundary() {
        let dir = TempDir::new().unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        endpoint
            .create_symlink(Path::new("first"), Path::new("link"))
            .await
            .unwrap();
        let identity = crate::endpoint::local_identity::metadata_identity(
            &fs::symlink_metadata(dir.path().join("link")).unwrap(),
            crate::engine::domain::EntryKind::Symlink,
        )
        .unwrap();
        endpoint
            .replace_symlink(
                Path::new("second"),
                Path::new("link"),
                ExpectedDestination::Unchanged(identity),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            fs::read_link(dir.path().join("link")).unwrap(),
            Path::new("second")
        );
    }

    #[tokio::test]
    async fn hardlink_update_is_atomic_at_endpoint_boundary() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("first"), b"one").unwrap();
        fs::write(dir.path().join("second"), b"two").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        endpoint
            .create_hardlink(Path::new("first"), Path::new("link"))
            .await
            .unwrap();
        endpoint
            .create_hardlink(Path::new("second"), Path::new("link"))
            .await
            .unwrap();
        assert_eq!(fs::read(dir.path().join("link")).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exists_sees_dangling_symlink() {
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink("missing", dir.path().join("link")).unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        assert!(endpoint.exists(Path::new("link")).await.unwrap());
        assert!(
            endpoint
                .metadata(Path::new("link"))
                .await
                .unwrap()
                .is_symlink
        );
    }

    #[test]
    fn empty_endpoint_root_means_current_directory() {
        let endpoint = LocalEndpoint::new(PathBuf::new());
        assert_eq!(endpoint.root(), Path::new("."));
        assert_eq!(
            endpoint.native_path(Path::new("dest")),
            Some(PathBuf::from("./dest"))
        );
    }

    #[test]
    fn exposes_native_path() {
        let endpoint = LocalEndpoint::new(PathBuf::from("/tmp/root"));
        assert_eq!(
            endpoint.native_path(Path::new("file")),
            Some(PathBuf::from("/tmp/root/file"))
        );
    }
}
