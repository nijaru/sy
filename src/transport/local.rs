use super::{TransferResult, Transport};
use crate::error::{format_bytes, Result, SyncError};
use crate::fs_util::{has_hard_links, same_filesystem, supports_cow_reflinks};
use crate::integrity::{ChecksumType, IntegrityVerifier};
use crate::sync::scanner::{FileEntry, Scanner};
use crate::temp_file::TempFileGuard;
use async_trait::async_trait;
use std::fs::{self, File};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Check if a file is sparse by comparing allocated blocks to file size
#[cfg(unix)]
fn is_file_sparse(metadata: &std::fs::Metadata) -> bool {
    let blocks = metadata.blocks();
    let file_size = metadata.len();
    let allocated_size = blocks * 512;

    // File is sparse if allocated size is significantly less than file size
    let threshold = 4096;
    file_size > threshold && allocated_size < file_size.saturating_sub(threshold)
}

#[cfg(not(unix))]
fn is_file_sparse(_metadata: &std::fs::Metadata) -> bool {
    false // Non-Unix platforms don't support sparse detection
}

/// Copy a sparse file while preserving holes
///
/// Tries to use SEEK_HOLE/SEEK_DATA for efficiency, falls back to block-based
/// zero detection if not supported.
#[cfg(unix)]
fn copy_sparse_file(source: &Path, dest: &Path) -> std::io::Result<u64> {
    // Try SEEK_HOLE/SEEK_DATA first (most efficient)
    match copy_sparse_file_seek(source, dest) {
        Ok(size) => Ok(size),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            // SEEK_DATA not supported, fall back to block-based approach
            copy_sparse_file_blocks(source, dest)
        }
        Err(e) => Err(e),
    }
}

/// Copy sparse file using SEEK_HOLE/SEEK_DATA (fast path)
#[cfg(unix)]
fn copy_sparse_file_seek(source: &Path, dest: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;

    const SEEK_DATA: i32 = 3; // Find next data region
    const SEEK_HOLE: i32 = 4; // Find next hole

    let mut src_file = File::open(source)?;
    let src_meta = src_file.metadata()?;
    let file_size = src_meta.len();

    if dest.exists() {
        fs::remove_file(dest)?;
    }
    let mut dst_file = File::create(dest)?;

    let mut pos: i64 = 0;
    let file_size_i64 = file_size as i64;
    let src_fd = src_file.as_raw_fd();

    // Try SEEK_DATA first to check if supported
    let first_data = unsafe { libc::lseek(src_fd, 0, SEEK_DATA) };
    if first_data < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINVAL) {
            return Err(err); // Not supported, caller will fall back
        }
        // ENXIO means file is all holes - just set size and return
        dst_file.set_len(file_size)?;
        return Ok(file_size);
    }

    // Seek back to start
    unsafe { libc::lseek(src_fd, 0, libc::SEEK_SET) };
    src_file.seek(SeekFrom::Start(0))?;

    while pos < file_size_i64 {
        let data_start = unsafe { libc::lseek(src_fd, pos, SEEK_DATA) };
        if data_start < 0 {
            break; // No more data (ENXIO)
        }
        if data_start >= file_size_i64 {
            break;
        }

        let hole_start = unsafe { libc::lseek(src_fd, data_start, SEEK_HOLE) };
        let data_end = if hole_start < 0 || hole_start > file_size_i64 {
            file_size_i64
        } else {
            hole_start
        };

        let data_len = (data_end - data_start) as usize;
        src_file.seek(SeekFrom::Start(data_start as u64))?;
        dst_file.seek(SeekFrom::Start(data_start as u64))?;

        let mut remaining = data_len;
        let mut buffer = vec![0u8; 1024 * 1024];

        while remaining > 0 {
            let chunk_size = remaining.min(buffer.len());
            let read = src_file.read(&mut buffer[..chunk_size])?;
            if read == 0 {
                break;
            }
            dst_file.write_all(&buffer[..read])?;
            remaining = remaining.saturating_sub(read);
        }

        pos = data_end;
    }

    dst_file.set_len(file_size)?;
    dst_file.sync_all()?;
    Ok(file_size)
}

/// Copy sparse file by reading blocks and detecting zeros (slow path, portable)
#[cfg(unix)]
fn copy_sparse_file_blocks(source: &Path, dest: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut src_file = File::open(source)?;
    let src_meta = src_file.metadata()?;
    let file_size = src_meta.len();

    if dest.exists() {
        fs::remove_file(dest)?;
    }
    let mut dst_file = File::create(dest)?;

    // Set file size FIRST using ftruncate
    // This creates a sparse file with the correct size
    dst_file.set_len(file_size)?;

    const BLOCK_SIZE: usize = 4096; // Typical filesystem block size
    let mut buffer = vec![0u8; BLOCK_SIZE];
    let mut pos = 0u64;

    while pos < file_size {
        let to_read = ((file_size - pos) as usize).min(BLOCK_SIZE);
        let read = src_file.read(&mut buffer[..to_read])?;
        if read == 0 {
            break;
        }

        // Check if block is all zeros (sparse hole)
        if buffer[..read].iter().all(|&b| b == 0) {
            // Skip this block (hole is already there from ftruncate)
            pos += read as u64;
        } else {
            // Write non-zero data
            dst_file.seek(SeekFrom::Start(pos))?;
            dst_file.write_all(&buffer[..read])?;
            pos += read as u64;
        }
    }

    dst_file.sync_all()?;
    Ok(file_size)
}

#[cfg(not(unix))]
fn copy_sparse_file(source: &Path, dest: &Path) -> std::io::Result<u64> {
    // On non-Unix platforms, fall back to regular copy
    fs::copy(source, dest)
}

/// Local filesystem transport
///
/// Implements the Transport trait for local filesystem operations.
/// This wraps the existing Phase 1 implementation in the async Transport interface.
pub struct LocalTransport {
    verifier: IntegrityVerifier,
}

impl LocalTransport {
    pub fn new() -> Self {
        // Default: no verification
        Self {
            verifier: IntegrityVerifier::new(ChecksumType::None, false),
        }
    }

    pub fn with_verifier(verifier: IntegrityVerifier) -> Self {
        Self { verifier }
    }
}

impl Default for LocalTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for LocalTransport {
    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>> {
        // Use existing scanner (runs synchronously, wrapped in async)
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let scanner = Scanner::new(&path);
            scanner.scan()
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        Ok(tokio::fs::try_exists(path).await.unwrap_or(false))
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        tokio::fs::metadata(path)
            .await
            .map_err(|e| SyncError::ReadDirError {
                path: path.to_path_buf(),
                source: e,
            })
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        tokio::fs::create_dir_all(path).await.map_err(SyncError::Io)
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            self.create_dir_all(parent).await?;
        }

        // Copy file with checksum verification using spawn_blocking
        let source = source.to_path_buf();
        let dest = dest.to_path_buf();

        tokio::task::spawn_blocking(move || {
            // Check if source is sparse
            let source_meta = fs::metadata(&source).map_err(|e| SyncError::CopyError {
                path: source.clone(),
                source: e,
            })?;

            let is_sparse = is_file_sparse(&source_meta);

            if is_sparse {
                // For sparse files, use std::fs::copy() which preserves sparseness on Unix
                tracing::debug!(
                    "Sparse file detected ({}), using sparse-aware copy",
                    source.display()
                );
                let bytes_written = fs::copy(&source, &dest).map_err(|e| SyncError::CopyError {
                    path: source.clone(),
                    source: e,
                })?;

                // Strip xattrs (fs::copy may preserve them on some platforms)
                #[cfg(unix)]
                {
                    if let Ok(xattr_list) = xattr::list(&dest) {
                        for attr_name in xattr_list {
                            let _ = xattr::remove(&dest, &attr_name);
                        }
                    }
                }

                // Preserve modification time
                if let Ok(mtime) = source_meta.modified() {
                    let _ = filetime::set_file_mtime(
                        &dest,
                        filetime::FileTime::from_system_time(mtime),
                    );
                }

                tracing::debug!(
                    "Sparse copy complete: {} ({} bytes logical size)",
                    source.display(),
                    bytes_written
                );

                return Ok(bytes_written);
            }

            // Use fs::copy() which is optimized per-platform:
            // - macOS: clonefile() for COW reflinks on APFS (100x+ faster)
            // - Linux: copy_file_range() for zero-copy (kernel-side)
            // - Fallback: sendfile() or read/write
            // This is MUCH faster than manual read/write loop
            let bytes_written = fs::copy(&source, &dest).map_err(|e| SyncError::CopyError {
                path: source.clone(),
                source: e,
            })?;

            // fs::copy() may preserve xattrs on some platforms (e.g., macOS).
            // Strip all xattrs so that Transferrer can selectively re-add them
            // based on preserve_xattrs setting.
            #[cfg(unix)]
            {
                if let Ok(xattr_list) = xattr::list(&dest) {
                    for attr_name in xattr_list {
                        let _ = xattr::remove(&dest, &attr_name);
                    }
                }
            }

            tracing::debug!(
                "Copied {} ({} bytes, fast copy)",
                source.display(),
                bytes_written
            );

            // Preserve modification time
            if let Ok(mtime) = source_meta.modified() {
                let _ =
                    filetime::set_file_mtime(&dest, filetime::FileTime::from_system_time(mtime));
            }

            Ok(bytes_written)
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))
        .and_then(|r| r)
        .map(TransferResult::new)
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Check if destination exists
        if !self.exists(dest).await? {
            tracing::debug!("Destination doesn't exist, using full copy");
            return self.copy_file(source, dest).await;
        }

        // Get file sizes
        let source_meta = self.metadata(source).await?;
        let dest_meta = self.metadata(dest).await?;
        let source_size = source_meta.len();
        let dest_size = dest_meta.len();

        // Size-based heuristic: use delta sync for files >10MB
        // Below this threshold, sequential copy is often faster than the overhead
        // of checksumming + delta generation + random I/O, even with O(1) rolling hash.
        // This threshold is tuned based on benchmarks showing delta sync is beneficial
        // for files as small as 10MB when changes are localized (e.g., 1MB change in 100MB).
        const DELTA_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB

        if dest_size < DELTA_THRESHOLD {
            tracing::debug!(
                "File size ({:.1} MB) below delta threshold ({} MB), using full copy",
                dest_size as f64 / 1024.0 / 1024.0,
                DELTA_THRESHOLD / 1024 / 1024
            );
            return self.copy_file(source, dest).await;
        }

        // Skip delta if destination is very small (full copy is faster)
        if dest_size < 4096 {
            tracing::debug!("Destination too small for delta sync, using full copy");
            return self.copy_file(source, dest).await;
        }

        tracing::info!(
            "File size {:.1} MB, attempting delta sync",
            dest_size as f64 / 1024.0 / 1024.0
        );

        // Run delta sync in blocking task
        let source = source.to_path_buf();
        let dest = dest.to_path_buf();
        let verifier = self.verifier.clone();

        tokio::task::spawn_blocking(move || {
            use crate::delta::estimate_change_ratio;
            use std::io::{BufReader, Read, Seek, SeekFrom, Write};
            use std::time::Instant;

            let block_size = 64 * 1024; // 64KB blocks for good I/O performance
            let total_start = Instant::now();

            // Check if source file is sparse FIRST (before change ratio)
            // Sparse files need special handling to preserve holes
            let source_meta = fs::metadata(&source).map_err(|e| SyncError::CopyError {
                path: source.clone(),
                source: e,
            })?;

            if is_file_sparse(&source_meta) {
                tracing::info!(
                    "Source file is sparse (allocated size < logical size), using sparse-aware copy"
                );

                // Use SEEK_HOLE/SEEK_DATA to preserve sparseness
                let bytes_written = copy_sparse_file(&source, &dest).map_err(|e| SyncError::CopyError {
                    path: source.clone(),
                    source: e,
                })?;

                tracing::debug!(
                    "Sparse file copy complete: {} bytes logical size",
                    bytes_written
                );

                return Ok(TransferResult::new(bytes_written));
            }

            // Sample blocks to estimate change ratio
            // If >75% of file has changed, full copy is faster than delta sync
            let change_ratio_result = estimate_change_ratio(
                &source,
                &dest,
                block_size,
                Some(20), // Sample 20 blocks
                Some(0.75), // 75% threshold
            );

            match change_ratio_result {
                Ok(ratio) => {
                    tracing::info!(
                        "Change ratio: {} ({}/{} blocks changed)",
                        ratio.change_ratio_percent(),
                        ratio.blocks_changed,
                        ratio.blocks_sampled
                    );

                    if !ratio.use_delta {
                        tracing::info!(
                            "Change ratio {} exceeds threshold {:.1}%, using full copy instead of delta sync",
                            ratio.change_ratio_percent(),
                            ratio.threshold * 100.0
                        );

                        // Fallback to full copy (not sparse, so fs::copy is fine)
                        let bytes_written = fs::copy(&source, &dest).map_err(|e| SyncError::CopyError {
                            path: source.clone(),
                            source: e,
                        })?;

                        return Ok(TransferResult::new(bytes_written));
                    }

                    tracing::info!(
                        "Change ratio {} below threshold, proceeding with delta sync",
                        ratio.change_ratio_percent()
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to estimate change ratio: {}. Proceeding with delta sync anyway.",
                        e
                    );
                }
            }

            // Choose delta sync strategy based on filesystem capabilities and file properties
            let supports_cow = supports_cow_reflinks(&dest);
            let same_fs = same_filesystem(&source, &dest);
            let has_hardlinks = has_hard_links(&dest);

            let use_cow_strategy = supports_cow && same_fs && !has_hardlinks;

            // Log strategy selection for debugging
            if use_cow_strategy {
                tracing::info!(
                    "Delta sync strategy: COW (clone + selective writes) - filesystem supports COW reflinks"
                );
            } else {
                let reason = if !supports_cow {
                    "filesystem does not support COW reflinks"
                } else if !same_fs {
                    "source and dest on different filesystems"
                } else {
                    "destination has hard links (preserving link integrity)"
                };

                tracing::info!(
                    "Delta sync strategy: in-place (full file rebuild) - {}",
                    reason
                );
            }

            // Strategy 1: COW clone + selective writes (fast on APFS/BTRFS/XFS)
            // Strategy 2: In-place delta (for ext4, hard links, cross-filesystem)
            let temp_dest = dest.with_extension("sy.tmp");
            let temp_guard = TempFileGuard::new(&temp_dest);

            let (bytes_written, literal_bytes, changed_blocks) = if use_cow_strategy {
                // COW Strategy: Clone file (instant), then selectively overwrite changed blocks
                fs::copy(&dest, &temp_dest).map_err(|e| SyncError::DeltaSyncError {
                    path: temp_dest.clone(),
                    strategy: "COW (clone + selective writes)".to_string(),
                    source: e,
                    hint: "COW file cloning failed. This may happen if:\n  \
                           - Filesystem doesn't support reflinks (needs APFS, BTRFS, or XFS)\n  \
                           - Cross-filesystem operation detected\n  \
                           - Insufficient disk space\n  \
                           Falling back to in-place strategy may help.".to_string(),
                })?;

                // Strip xattrs from clone (fs::copy may preserve them)
                #[cfg(unix)]
                {
                    if let Ok(xattr_list) = xattr::list(&temp_dest) {
                        for attr_name in xattr_list {
                            let _ = xattr::remove(&temp_dest, &attr_name);
                        }
                    }
                }

                // Open source and original dest for reading (sequential)
                let mut source_file = BufReader::with_capacity(
                    256 * 1024,
                    File::open(&source).map_err(|e| SyncError::CopyError {
                        path: source.clone(),
                        source: e,
                    })?,
                );
                let mut dest_file = BufReader::with_capacity(
                    256 * 1024,
                    File::open(&dest).map_err(|e| SyncError::CopyError {
                        path: dest.clone(),
                        source: e,
                    })?,
                );

                // Open temp file for writing (selective, seek-based)
                let mut temp_file = File::options()
                    .write(true)
                    .open(&temp_dest)
                    .map_err(|e| SyncError::CopyError {
                        path: temp_dest.clone(),
                        source: e,
                    })?;

                let mut source_buf = vec![0u8; block_size];
                let mut dest_buf = vec![0u8; block_size];
                let mut offset = 0u64;
                let mut bytes_written = 0u64;
                let mut literal_bytes = 0u64;
                let mut changed_blocks = 0usize;

                // Compare blocks and only write changed ones
                loop {
                    let src_read = source_file.read(&mut source_buf).map_err(|e| {
                        SyncError::CopyError {
                            path: source.clone(),
                            source: e,
                        }
                    })?;
                    if src_read == 0 {
                        break; // EOF
                    }

                    let dst_read = dest_file.read(&mut dest_buf).map_err(|e| SyncError::CopyError {
                        path: dest.clone(),
                        source: e,
                    })?;

                    // Compare blocks
                    let blocks_match = src_read == dst_read
                        && source_buf[..src_read] == dest_buf[..dst_read];

                    if !blocks_match {
                        // Block changed - seek and write to temp file
                        temp_file
                            .seek(SeekFrom::Start(offset))
                            .map_err(|e| SyncError::CopyError {
                                path: temp_dest.clone(),
                                source: e,
                            })?;
                        temp_file
                            .write_all(&source_buf[..src_read])
                            .map_err(|e| SyncError::CopyError {
                                path: temp_dest.clone(),
                                source: e,
                            })?;

                        // Verify block if paranoid mode enabled
                        if verifier.verify_on_write() {
                            let mut verify_buf = vec![0u8; src_read];
                            temp_file
                                .seek(SeekFrom::Start(offset))
                                .map_err(|e| SyncError::CopyError {
                                    path: temp_dest.clone(),
                                    source: e,
                                })?;
                            temp_file
                                .read_exact(&mut verify_buf)
                                .map_err(|e| SyncError::CopyError {
                                    path: temp_dest.clone(),
                                    source: e,
                                })?;

                            if !verifier.verify_block(&source_buf[..src_read], &verify_buf)? {
                                let expected = verifier.compute_data_checksum(&source_buf[..src_read])?;
                                let actual = verifier.compute_data_checksum(&verify_buf)?;
                                return Err(SyncError::BlockCorruption {
                                    path: temp_dest.clone(),
                                    block_number: (offset / block_size as u64) as usize,
                                    expected_checksum: expected.to_hex(),
                                    actual_checksum: actual.to_hex(),
                                });
                            }
                        }

                        literal_bytes += src_read as u64;
                        changed_blocks += 1;
                    }
                    // If blocks match, we don't write anything! Clone already has the data.

                    bytes_written += src_read as u64;
                    offset += src_read as u64;
                }

                // Truncate temp file to source size (if source < dest)
                temp_file.set_len(bytes_written).map_err(|e| SyncError::CopyError {
                    path: temp_dest.clone(),
                    source: e,
                })?;

                // Flush and sync temp file
                temp_file.flush().map_err(|e| SyncError::CopyError {
                    path: temp_dest.clone(),
                    source: e,
                })?;
                drop(temp_file);

                (bytes_written, literal_bytes, changed_blocks)
            } else {
                // In-place Strategy: Create temp file, copy only changed blocks
                // This avoids slow fs::copy() on non-COW filesystems like ext4

                // Create empty temp file and allocate space
                let temp_file = File::create(&temp_dest).map_err(|e| SyncError::DeltaSyncError {
                    path: temp_dest.clone(),
                    strategy: "in-place (full file rebuild)".to_string(),
                    source: e,
                    hint: "Failed to create temporary file for delta sync.\n  \
                           Check write permissions and disk space on destination.".to_string(),
                })?;
                temp_file.set_len(source_size).map_err(|e| SyncError::DeltaSyncError {
                    path: temp_dest.clone(),
                    strategy: "in-place (full file rebuild)".to_string(),
                    source: e,
                    hint: format!("Failed to allocate {} for temporary file.\n  \
                                  Check available disk space on destination.",
                                  format_bytes(source_size)),
                })?;
                drop(temp_file);

                // Open source and dest for reading
                let mut source_file = BufReader::with_capacity(
                    256 * 1024,
                    File::open(&source).map_err(|e| SyncError::CopyError {
                        path: source.clone(),
                        source: e,
                    })?,
                );
                let mut dest_file = BufReader::with_capacity(
                    256 * 1024,
                    File::open(&dest).map_err(|e| SyncError::CopyError {
                        path: dest.clone(),
                        source: e,
                    })?,
                );

                // Open temp for random writes
                let mut temp_file = File::options()
                    .write(true)
                    .open(&temp_dest)
                    .map_err(|e| SyncError::CopyError {
                        path: temp_dest.clone(),
                        source: e,
                    })?;

                let mut source_buf = vec![0u8; block_size];
                let mut dest_buf = vec![0u8; block_size];
                let mut offset = 0u64;
                let mut bytes_written = 0u64;
                let mut literal_bytes = 0u64;
                let mut changed_blocks = 0usize;

                // Compare blocks and write ALL blocks (changed + unchanged)
                // to build the complete new file
                loop {
                    let src_read = source_file.read(&mut source_buf).map_err(|e| {
                        SyncError::CopyError {
                            path: source.clone(),
                            source: e,
                        }
                    })?;
                    if src_read == 0 {
                        break; // EOF
                    }

                    let dst_read = dest_file.read(&mut dest_buf).map_err(|e| SyncError::CopyError {
                        path: dest.clone(),
                        source: e,
                    })?;

                    // Compare blocks
                    let blocks_match = src_read == dst_read
                        && source_buf[..src_read] == dest_buf[..dst_read];

                    // Always seek and write (building complete file)
                    temp_file
                        .seek(SeekFrom::Start(offset))
                        .map_err(|e| SyncError::CopyError {
                            path: temp_dest.clone(),
                            source: e,
                        })?;
                    temp_file
                        .write_all(&source_buf[..src_read])
                        .map_err(|e| SyncError::CopyError {
                            path: temp_dest.clone(),
                            source: e,
                        })?;

                    // Verify block if paranoid mode enabled
                    if verifier.verify_on_write() {
                        let mut verify_buf = vec![0u8; src_read];
                        temp_file
                            .seek(SeekFrom::Start(offset))
                            .map_err(|e| SyncError::CopyError {
                                path: temp_dest.clone(),
                                source: e,
                            })?;
                        temp_file
                            .read_exact(&mut verify_buf)
                            .map_err(|e| SyncError::CopyError {
                                path: temp_dest.clone(),
                                source: e,
                            })?;

                        if !verifier.verify_block(&source_buf[..src_read], &verify_buf)? {
                            let expected = verifier.compute_data_checksum(&source_buf[..src_read])?;
                            let actual = verifier.compute_data_checksum(&verify_buf)?;
                            return Err(SyncError::BlockCorruption {
                                path: temp_dest.clone(),
                                block_number: (offset / block_size as u64) as usize,
                                expected_checksum: expected.to_hex(),
                                actual_checksum: actual.to_hex(),
                            });
                        }
                    }

                    if !blocks_match {
                        literal_bytes += src_read as u64;
                        changed_blocks += 1;
                    }

                    bytes_written += src_read as u64;
                    offset += src_read as u64;
                }

                // Flush and sync temp file
                temp_file.flush().map_err(|e| SyncError::CopyError {
                    path: temp_dest.clone(),
                    source: e,
                })?;
                drop(temp_file);

                (bytes_written, literal_bytes, changed_blocks)
            };

            let total_elapsed = total_start.elapsed();
            tracing::debug!(
                "Local delta sync completed in {:?} ({} changed blocks)",
                total_elapsed,
                changed_blocks
            );

            let compression_ratio = if source_size > 0 {
                (literal_bytes as f64 / source_size as f64) * 100.0
            } else {
                0.0
            };

            // Atomic rename
            fs::rename(&temp_dest, &dest).map_err(|e| SyncError::CopyError {
                path: dest.clone(),
                source: e,
            })?;

            // Defuse temp file guard - file successfully renamed
            temp_guard.defuse();

            let total_blocks = bytes_written.div_ceil(block_size as u64) as usize;
            tracing::info!(
                "Local delta sync: {} blocks compared, {} changed ({:.1}%)",
                total_blocks,
                changed_blocks,
                compression_ratio
            );

            Ok::<TransferResult, SyncError>(TransferResult::with_delta(
                bytes_written,
                changed_blocks,
                literal_bytes,
            ))
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        if is_dir {
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(SyncError::Io)?;
        } else {
            tokio::fs::remove_file(path).await.map_err(SyncError::Io)?;
        }
        tracing::info!("Removed: {}", path.display());
        Ok(())
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(SyncError::Io)?;
        }

        // Create the hard link
        tokio::fs::hard_link(source, dest)
            .await
            .map_err(SyncError::Io)?;

        tracing::debug!(
            "Created hardlink: {} -> {}",
            dest.display(),
            source.display()
        );
        Ok(())
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(SyncError::Io)?;
        }

        // Create the symbolic link
        #[cfg(unix)]
        {
            tokio::fs::symlink(target, dest)
                .await
                .map_err(SyncError::Io)?;
        }

        #[cfg(windows)]
        {
            // Windows requires different symlink APIs for files vs directories
            if tokio::fs::metadata(target)
                .await
                .ok()
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                tokio::fs::symlink_dir(target, dest)
                    .await
                    .map_err(SyncError::Io)?;
            } else {
                tokio::fs::symlink_file(target, dest)
                    .await
                    .map_err(SyncError::Io)?;
            }
        }

        tracing::debug!(
            "Created symlink: {} -> {}",
            dest.display(),
            target.display()
        );
        Ok(())
    }

    async fn copy_file_streaming(
        &self,
        source: &Path,
        dest: &Path,
        progress_callback: Option<std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>>,
    ) -> Result<TransferResult> {
        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            self.create_dir_all(parent).await?;
        }

        let source = source.to_path_buf();
        let dest = dest.to_path_buf();

        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};

            // Get source metadata
            let source_meta = fs::metadata(&source).map_err(|e| SyncError::CopyError {
                path: source.clone(),
                source: e,
            })?;

            let total_size = source_meta.len();

            // Open source for reading
            let mut src_file = File::open(&source).map_err(|e| SyncError::CopyError {
                path: source.clone(),
                source: e,
            })?;

            // Create destination for writing
            let mut dst_file = File::create(&dest).map_err(|e| SyncError::CopyError {
                path: dest.clone(),
                source: e,
            })?;

            // Streaming copy with progress updates
            const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
            let mut buffer = vec![0u8; CHUNK_SIZE];
            let mut bytes_transferred = 0u64;

            // Initial progress callback
            if let Some(callback) = &progress_callback {
                callback(0, total_size);
            }

            loop {
                let bytes_read = src_file.read(&mut buffer).map_err(|e| SyncError::CopyError {
                    path: source.clone(),
                    source: e,
                })?;

                if bytes_read == 0 {
                    break; // EOF
                }

                dst_file
                    .write_all(&buffer[..bytes_read])
                    .map_err(|e| SyncError::CopyError {
                        path: dest.clone(),
                        source: e,
                    })?;

                bytes_transferred += bytes_read as u64;

                // Update progress after each chunk
                if let Some(callback) = &progress_callback {
                    callback(bytes_transferred, total_size);
                }
            }

            // Flush and sync
            dst_file.flush().map_err(|e| SyncError::CopyError {
                path: dest.clone(),
                source: e,
            })?;
            dst_file.sync_all().map_err(|e| SyncError::CopyError {
                path: dest.clone(),
                source: e,
            })?;
            drop(dst_file);

            // Strip xattrs (to match copy_file behavior)
            #[cfg(unix)]
            {
                if let Ok(xattr_list) = xattr::list(&dest) {
                    for attr_name in xattr_list {
                        let _ = xattr::remove(&dest, &attr_name);
                    }
                }
            }

            // Preserve modification time
            if let Ok(mtime) = source_meta.modified() {
                let _ =
                    filetime::set_file_mtime(&dest, filetime::FileTime::from_system_time(mtime));
            }

            tracing::debug!(
                "Streaming copy complete: {} ({} bytes)",
                source.display(),
                bytes_transferred
            );

            Ok(TransferResult::new(bytes_transferred))
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))
        .and_then(|r| r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_local_transport_scan() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create test structure
        fs::create_dir(root.join("dir1")).unwrap();
        fs::write(root.join("file1.txt"), "content").unwrap();
        fs::write(root.join("dir1/file2.txt"), "content").unwrap();

        let transport = LocalTransport::new();
        let entries = transport.scan(root).await.unwrap();

        assert!(entries.len() >= 3);
        assert!(entries
            .iter()
            .any(|e| **e.relative_path == PathBuf::from("file1.txt")));
    }

    #[tokio::test]
    async fn test_local_transport_exists() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        fs::write(root.join("exists.txt"), "content").unwrap();

        let transport = LocalTransport::new();
        assert!(transport.exists(&root.join("exists.txt")).await.unwrap());
        assert!(!transport
            .exists(&root.join("not_exists.txt"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_local_transport_copy_file() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        let transport = LocalTransport::new();
        let dest_file = dest_dir.path().join("test.txt");
        transport.copy_file(&source_file, &dest_file).await.unwrap();

        assert!(dest_file.exists());
        assert_eq!(fs::read_to_string(&dest_file).unwrap(), "test content");
    }

    #[tokio::test]
    async fn test_local_transport_create_dir_all() {
        let temp = TempDir::new().unwrap();
        let nested_path = temp.path().join("a/b/c");

        let transport = LocalTransport::new();
        transport.create_dir_all(&nested_path).await.unwrap();

        assert!(nested_path.exists());
        assert!(nested_path.is_dir());
    }

    #[tokio::test]
    async fn test_local_transport_remove_file() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("remove.txt");
        fs::write(&file, "content").unwrap();

        let transport = LocalTransport::new();
        transport.remove(&file, false).await.unwrap();

        assert!(!file.exists());
    }

    #[tokio::test]
    async fn test_local_transport_remove_dir() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("remove_dir");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file.txt"), "content").unwrap();

        let transport = LocalTransport::new();
        transport.remove(&dir, true).await.unwrap();

        assert!(!dir.exists());
    }

    #[tokio::test]
    #[cfg(unix)] // Sparse files work differently on Windows
    async fn test_local_transport_sparse_file_copy() {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;

        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create a sparse file using dd
        let source_file = source_dir.path().join("sparse.dat");
        let output = std::process::Command::new("dd")
            .args([
                "if=/dev/zero",
                &format!("of={}", source_file.display()),
                "bs=1024",
                "count=0",
                "seek=10240", // 10MB sparse file
            ])
            .output()
            .expect("Failed to create sparse file");

        if !output.status.success() {
            panic!("dd command failed");
        }

        // Write some actual data
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&source_file)
            .unwrap();
        file.write_all(&[0x42; 4096]).unwrap();
        drop(file);

        // Copy the file
        let transport = LocalTransport::new();
        let dest_file = dest_dir.path().join("sparse.dat");
        let result = transport.copy_file(&source_file, &dest_file).await.unwrap();

        // Verify copy succeeded
        assert!(dest_file.exists());
        assert_eq!(result.bytes_written, 10 * 1024 * 1024);

        // Verify destination is also sparse (or at least has same size)
        let dest_meta = fs::metadata(&dest_file).unwrap();
        assert_eq!(dest_meta.len(), 10 * 1024 * 1024);

        // Check if sparseness was preserved (depends on filesystem)
        let dest_blocks = dest_meta.blocks();
        let dest_allocated = dest_blocks * 512;
        if dest_allocated < dest_meta.len() {
            // Sparseness was preserved!
            eprintln!(
                "✓ Sparse file copy preserved sparseness: {} allocated vs {} logical",
                dest_allocated,
                dest_meta.len()
            );
        } else {
            eprintln!(
                "⚠ Sparseness not preserved: {} allocated vs {} logical (filesystem dependent)",
                dest_allocated,
                dest_meta.len()
            );
        }
    }

    // === Error Handling Tests ===

    #[tokio::test]
    async fn test_copy_file_nonexistent_source() {
        let dest_dir = TempDir::new().unwrap();
        let transport = LocalTransport::new();

        let nonexistent = PathBuf::from("/nonexistent/file.txt");
        let dest = dest_dir.path().join("test.txt");

        let result = transport.copy_file(&nonexistent, &dest).await;
        assert!(result.is_err(), "Should fail when source doesn't exist");
    }

    #[tokio::test]
    #[cfg(unix)] // Permission tests work differently on Windows
    async fn test_copy_file_permission_denied_destination() {
        use std::os::unix::fs::PermissionsExt;

        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create source file
        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        // Make destination directory read-only
        let mut perms = fs::metadata(dest_dir.path()).unwrap().permissions();
        perms.set_mode(0o444); // Read-only
        fs::set_permissions(dest_dir.path(), perms).unwrap();

        let transport = LocalTransport::new();
        let dest_file = dest_dir.path().join("test.txt");

        let result = transport.copy_file(&source_file, &dest_file).await;

        // Restore permissions for cleanup
        let mut perms = fs::metadata(dest_dir.path()).unwrap().permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(dest_dir.path(), perms);

        assert!(result.is_err(), "Should fail when destination is read-only");
    }

    #[tokio::test]
    async fn test_create_dir_all_nested() {
        let temp = TempDir::new().unwrap();
        let transport = LocalTransport::new();

        let nested_path = temp.path().join("a/b/c/d/e/f");
        transport.create_dir_all(&nested_path).await.unwrap();

        assert!(nested_path.exists());
        assert!(nested_path.is_dir());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_dir_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("parent");
        fs::create_dir(&parent).unwrap();

        // Make parent read-only
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&parent, perms).unwrap();

        let transport = LocalTransport::new();
        let child = parent.join("child");

        let result = transport.create_dir_all(&child).await;

        // Restore permissions for cleanup
        let mut perms = fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(&parent, perms);

        assert!(result.is_err(), "Should fail when parent is read-only");
    }

    #[tokio::test]
    async fn test_remove_nonexistent_file() {
        let temp = TempDir::new().unwrap();
        let transport = LocalTransport::new();

        let nonexistent = temp.path().join("nonexistent.txt");
        let result = transport.remove(&nonexistent, false).await;

        // Should error on nonexistent file
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_metadata_nonexistent_file() {
        let temp = TempDir::new().unwrap();
        let transport = LocalTransport::new();

        let nonexistent = temp.path().join("nonexistent.txt");
        let result = transport.metadata(&nonexistent).await;

        assert!(result.is_err(), "Should fail for nonexistent file");
    }

    #[tokio::test]
    async fn test_scan_nonexistent_directory() {
        let transport = LocalTransport::new();
        let nonexistent = PathBuf::from("/nonexistent/directory");

        let result = transport.scan(&nonexistent).await;
        assert!(result.is_err(), "Should fail when directory doesn't exist");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_scan_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let protected_dir = temp.path().join("protected");
        fs::create_dir(&protected_dir).unwrap();
        fs::write(protected_dir.join("file.txt"), "content").unwrap();

        // Make directory unreadable
        let mut perms = fs::metadata(&protected_dir).unwrap().permissions();
        perms.set_mode(0o000); // No permissions
        fs::set_permissions(&protected_dir, perms).unwrap();

        let transport = LocalTransport::new();
        let result = transport.scan(&protected_dir).await;

        // Restore permissions for cleanup
        let mut perms = fs::metadata(&protected_dir).unwrap().permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(&protected_dir, perms);

        assert!(
            result.is_err(),
            "Should fail when directory is not readable"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_hardlink_across_filesystems() {
        // This test attempts to create a hardlink across filesystems
        // It should fail gracefully
        let source_dir = TempDir::new().unwrap();
        let source_file = source_dir.path().join("source.txt");
        fs::write(&source_file, "content").unwrap();

        // Try to link to /tmp (likely different filesystem on many systems)
        let dest = PathBuf::from("/tmp/sy_test_hardlink_cross_fs.txt");

        let transport = LocalTransport::new();
        let result = transport.create_hardlink(&source_file, &dest).await;

        // Clean up if it somehow succeeded
        let _ = fs::remove_file(&dest);

        // On most systems this should fail (cross-device link)
        // But if both are on same filesystem, it might succeed
        // Either way, we're testing that it doesn't crash
        // Both outcomes are acceptable - we just verify no panic
        let _ = result;
    }

    #[tokio::test]
    async fn test_copy_file_streaming_with_progress() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create a 5MB test file
        let source_file = source_dir.path().join("large.dat");
        let data = vec![0x42u8; 5 * 1024 * 1024]; // 5MB
        fs::write(&source_file, &data).unwrap();

        // Track progress updates
        let progress_updates = Arc::new(AtomicU64::new(0));
        let progress_updates_clone = progress_updates.clone();
        let last_bytes = Arc::new(AtomicU64::new(0));
        let last_bytes_clone = last_bytes.clone();

        let progress_callback = Arc::new(move |bytes_transferred: u64, total: u64| {
            progress_updates_clone.fetch_add(1, Ordering::SeqCst);
            last_bytes_clone.store(bytes_transferred, Ordering::SeqCst);
            assert!(bytes_transferred <= total, "Transferred bytes should not exceed total");
        });

        // Copy file with streaming and progress
        let transport = LocalTransport::new();
        let dest_file = dest_dir.path().join("large.dat");
        let result = transport
            .copy_file_streaming(&source_file, &dest_file, Some(progress_callback))
            .await
            .unwrap();

        // Verify copy succeeded
        assert_eq!(result.bytes_written, 5 * 1024 * 1024);
        assert!(dest_file.exists());

        // Verify content matches
        let dest_data = fs::read(&dest_file).unwrap();
        assert_eq!(dest_data, data);

        // Verify progress was updated (should be at least 5 updates for 5MB file with 1MB chunks)
        let updates = progress_updates.load(Ordering::SeqCst);
        assert!(updates >= 5, "Expected at least 5 progress updates, got {}", updates);

        // Verify final progress shows complete transfer
        let final_bytes = last_bytes.load(Ordering::SeqCst);
        assert_eq!(final_bytes, 5 * 1024 * 1024, "Final progress should show complete transfer");
    }

    #[tokio::test]
    async fn test_copy_file_streaming_without_progress() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create a test file
        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        // Copy without progress callback
        let transport = LocalTransport::new();
        let dest_file = dest_dir.path().join("test.txt");
        let result = transport
            .copy_file_streaming(&source_file, &dest_file, None)
            .await
            .unwrap();

        // Verify copy succeeded
        assert_eq!(result.bytes_written, 12);
        assert!(dest_file.exists());
        assert_eq!(fs::read_to_string(&dest_file).unwrap(), "test content");
    }
}
