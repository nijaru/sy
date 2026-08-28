//! Capability-driven file transfer selection for the v0.5 architecture.
//!
//! The common path prefers endpoint-native filesystem primitives when both
//! sides expose native paths. Generic endpoint pairs fall back to bounded
//! staged streaming. Local updates may use a reflink clone + patch when that
//! reduces physical writes; non-COW local files use a normal whole-file copy.

use crate::endpoint::io::copy_file_streaming;
use crate::endpoint::{Endpoint, FileMetadata};
use crate::error::{Result, SyncError};
use crate::temp_file::TempFileGuard;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStrategy {
    /// Native sparse-aware copy preserving holes.
    NativeSparseCopy,
    /// Clone the old destination with COW and patch changed ranges.
    ReflinkPatch,
    /// Use the platform's optimized whole-file copy primitive into staging.
    NativeWholeCopy,
    /// Portable bounded source -> staged destination streaming.
    Streaming,
}

#[derive(Debug, Clone, Copy)]
pub struct TransferOptions {
    pub update: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct TransferResult {
    pub bytes_written: u64,
    pub strategy: TransferStrategy,
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
    let metadata = source.metadata(source_path).await?;
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
        "selecting transfer strategy"
    );

    if let (Some(source_native), Some(dest_native)) =
        (source.native_path(source_path), dest.native_path(dest_path))
    {
        if source_caps.sparse
            && dest_caps.sparse
            && native_file_is_sparse(&source_native).unwrap_or(false)
        {
            if let Some(bytes_written) =
                native_sparse_copy(source_native.clone(), dest_native.clone(), metadata.clone())
                    .await?
            {
                return Ok(TransferResult {
                    bytes_written,
                    strategy: TransferStrategy::NativeSparseCopy,
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
            if let Some(bytes_written) =
                reflink_patch(source_native.clone(), dest_native.clone(), metadata.clone()).await?
            {
                return Ok(TransferResult {
                    bytes_written,
                    strategy: TransferStrategy::ReflinkPatch,
                });
            }
        }

        if dest_caps.atomic_rename {
            let bytes_written =
                native_whole_copy(source_native, dest_native, metadata.clone()).await?;
            return Ok(TransferResult {
                bytes_written,
                strategy: TransferStrategy::NativeWholeCopy,
            });
        }
    }

    if source_caps.streaming_read && dest_caps.staged_write {
        let result = copy_file_streaming(source, source_path, dest, dest_path).await?;
        return Ok(TransferResult {
            bytes_written: result.bytes_written,
            strategy: TransferStrategy::Streaming,
        });
    }

    Err(SyncError::Config(format!(
        "no safe transfer strategy for {:?} -> {:?}",
        source.endpoint_type(),
        dest.endpoint_type()
    )))
}

async fn native_whole_copy(source: PathBuf, dest: PathBuf, metadata: FileMetadata) -> Result<u64> {
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let temp = TempFileGuard::temp_path_for(&dest);
        let guard = TempFileGuard::new(&temp);
        let bytes_written = std::fs::copy(&source, &temp)?;
        strip_xattrs(&temp);
        apply_metadata(&temp, &metadata)?;
        std::fs::rename(&temp, &dest)?;
        guard.defuse();
        Ok(bytes_written)
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
}

async fn reflink_patch(
    source: PathBuf,
    dest: PathBuf,
    metadata: FileMetadata,
) -> Result<Option<u64>> {
    tokio::task::spawn_blocking(move || {
        if !dest.exists() {
            return Ok(None);
        }

        // Reflink patching trades an extra destination read for fewer physical
        // writes. Keep it conservative until the benchmark suite tunes this.
        let ratio = match crate::delta::estimate_change_ratio(
            &source,
            &dest,
            1024 * 1024,
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

        strip_xattrs(&temp);
        patch_changed_blocks(&source, &dest, &temp, metadata.size)?;
        apply_metadata(&temp, &metadata)?;
        std::fs::rename(&temp, &dest)?;
        guard.defuse();
        Ok(Some(metadata.size))
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
}

fn patch_changed_blocks(
    source: &Path,
    old_dest: &Path,
    staged: &Path,
    source_size: u64,
) -> std::io::Result<()> {
    use std::fs::{File, OpenOptions};
    use std::io::{BufReader, Read, Seek, SeekFrom, Write};

    const BLOCK_SIZE: usize = 1024 * 1024;
    let mut source_file = BufReader::with_capacity(BLOCK_SIZE, File::open(source)?);
    let mut dest_file = BufReader::with_capacity(BLOCK_SIZE, File::open(old_dest)?);
    let mut staged_file = OpenOptions::new().write(true).open(staged)?;
    let mut source_buf = vec![0_u8; BLOCK_SIZE];
    let mut dest_buf = vec![0_u8; BLOCK_SIZE];
    let mut offset = 0_u64;

    loop {
        let source_read = source_file.read(&mut source_buf)?;
        if source_read == 0 {
            break;
        }
        let dest_read = dest_file.read(&mut dest_buf)?;

        if source_read != dest_read || source_buf[..source_read] != dest_buf[..dest_read] {
            staged_file.seek(SeekFrom::Start(offset))?;
            staged_file.write_all(&source_buf[..source_read])?;
        }

        offset += source_read as u64;
    }

    staged_file.set_len(source_size)?;
    staged_file.flush()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn reflink_clone(source: &Path, dest: &Path) -> std::io::Result<()> {
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;

    // Linux FICLONE from <linux/fs.h>.
    const FICLONE: libc::c_ulong = 0x4004_9409;

    let source_file = File::open(source)?;
    let dest_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dest)?;

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
) -> Result<Option<u64>> {
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
        let mut buffer = vec![0_u8; 1024 * 1024];

        for region in regions {
            source_file.seek(SeekFrom::Start(region.offset))?;
            staged.seek(SeekFrom::Start(region.offset))?;
            let mut remaining = region.length;
            while remaining > 0 {
                let chunk =
                    usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
                let read = source_file.read(&mut buffer[..chunk])?;
                if read == 0 {
                    break;
                }
                staged.write_all(&buffer[..read])?;
                remaining -= read as u64;
            }
        }

        staged.flush()?;
        drop(staged);
        apply_metadata(&temp, &metadata)?;
        std::fs::rename(&temp, &dest)?;
        guard.defuse();
        Ok(Some(metadata.size))
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
}

#[cfg(not(unix))]
async fn native_sparse_copy(
    _source: PathBuf,
    _dest: PathBuf,
    _metadata: FileMetadata,
) -> Result<Option<u64>> {
    Ok(None)
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
fn strip_xattrs(path: &Path) {
    if let Ok(attributes) = xattr::list(path) {
        for attribute in attributes {
            let _ = xattr::remove(path, &attribute);
        }
    }
}

#[cfg(not(unix))]
fn strip_xattrs(_path: &Path) {}
