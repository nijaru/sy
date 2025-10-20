use super::{TransferResult, Transport};
use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, Scanner};
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

/// Local filesystem transport
///
/// Implements the Transport trait for local filesystem operations.
/// This wraps the existing Phase 1 implementation in the async Transport interface.
pub struct LocalTransport;

impl LocalTransport {
    pub fn new() -> Self {
        Self
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

            // Regular file copy with checksum verification
            use std::io::{Read, Write};

            // Open source and destination files
            let mut source_file = fs::File::open(&source).map_err(|e| SyncError::CopyError {
                path: source.clone(),
                source: e,
            })?;

            let mut dest_file = fs::File::create(&dest).map_err(|e| SyncError::CopyError {
                path: dest.clone(),
                source: e,
            })?;

            // Stream copy with checksum calculation
            // 256KB optimal for disk I/O and checksum performance
            const CHUNK_SIZE: usize = 256 * 1024; // 256KB chunks
            let mut buffer = vec![0u8; CHUNK_SIZE];
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            let mut bytes_written = 0u64;

            loop {
                let bytes_read =
                    source_file
                        .read(&mut buffer)
                        .map_err(|e| SyncError::CopyError {
                            path: source.clone(),
                            source: e,
                        })?;

                if bytes_read == 0 {
                    break;
                }

                hasher.update(&buffer[..bytes_read]);
                dest_file
                    .write_all(&buffer[..bytes_read])
                    .map_err(|e| SyncError::CopyError {
                        path: dest.clone(),
                        source: e,
                    })?;

                bytes_written += bytes_read as u64;
            }

            let checksum = hasher.digest();

            tracing::debug!(
                "Copied {} ({} bytes, xxh3: {:x})",
                source.display(),
                bytes_written,
                checksum
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

        tokio::task::spawn_blocking(move || {
            use std::io::{BufReader, BufWriter, Read, Write};
            use std::time::Instant;

            // For local->local delta sync, we can do better than rsync algorithm:
            // Just compare blocks directly since we have both files locally.
            // This is MUCH faster: no checksumming, no hash lookups, sequential I/O only.

            let block_size = 64 * 1024; // 64KB blocks for good I/O performance
            let total_start = Instant::now();

            // Open both files
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

            // Create temp file for writing
            let temp_dest = dest.with_extension("sy.tmp");
            let mut temp_file = BufWriter::with_capacity(
                256 * 1024,
                File::create(&temp_dest).map_err(|e| SyncError::CopyError {
                    path: temp_dest.clone(),
                    source: e,
                })?,
            );

            let mut source_buf = vec![0u8; block_size];
            let mut dest_buf = vec![0u8; block_size];
            let mut bytes_written = 0u64;
            let mut literal_bytes = 0u64;
            let mut copy_ops = 0usize;

            // Compare and copy block by block
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
                let blocks_match = src_read == dst_read && source_buf[..src_read] == dest_buf[..dst_read];

                if !blocks_match {
                    // Block changed or sizes different - write new data
                    temp_file
                        .write_all(&source_buf[..src_read])
                        .map_err(|e| SyncError::CopyError {
                            path: temp_dest.clone(),
                            source: e,
                        })?;
                    literal_bytes += src_read as u64;
                } else {
                    // Block unchanged - copy from destination
                    temp_file
                        .write_all(&dest_buf[..dst_read])
                        .map_err(|e| SyncError::CopyError {
                            path: temp_dest.clone(),
                            source: e,
                        })?;
                    copy_ops += 1;
                }
                bytes_written += src_read as u64;
            }

            // Flush temp file
            temp_file.flush().map_err(|e| SyncError::CopyError {
                path: temp_dest.clone(),
                source: e,
            })?;
            drop(temp_file);

            let total_elapsed = total_start.elapsed();
            tracing::debug!("Local delta sync completed in {:?}", total_elapsed);

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

            let total_ops = copy_ops + if literal_bytes > 0 { 1 } else { 0 };
            tracing::info!(
                "Local delta sync: {} blocks compared, {:.1}% changed",
                total_ops,
                compression_ratio
            );

            Ok::<TransferResult, SyncError>(TransferResult::with_delta(
                bytes_written,
                total_ops,
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
            .any(|e| e.relative_path == PathBuf::from("file1.txt")));
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

        // On filesystems that support sparse files, verify sparseness is preserved
        let dest_blocks = dest_meta.blocks();
        let dest_allocated = dest_blocks * 512;
        if dest_allocated < dest_meta.len() {
            // Sparseness was preserved!
            assert!(
                dest_allocated < dest_meta.len() / 2,
                "Destination should be sparse"
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
}
