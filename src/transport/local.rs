use super::{Transport, TransferResult};
use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, Scanner};
use async_trait::async_trait;
use std::fs;
use std::path::Path;

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
        tokio::fs::create_dir_all(path)
            .await
            .map_err(SyncError::Io)
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
            const CHUNK_SIZE: usize = 128 * 1024; // 128KB chunks
            let mut buffer = vec![0u8; CHUNK_SIZE];
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            let mut bytes_written = 0u64;

            loop {
                let bytes_read = source_file.read(&mut buffer).map_err(|e| SyncError::CopyError {
                    path: source.clone(),
                    source: e,
                })?;

                if bytes_read == 0 {
                    break;
                }

                hasher.update(&buffer[..bytes_read]);
                dest_file.write_all(&buffer[..bytes_read]).map_err(|e| SyncError::CopyError {
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
            if let Ok(source_meta) = fs::metadata(&source) {
                if let Ok(mtime) = source_meta.modified() {
                    let _ = filetime::set_file_mtime(
                        &dest,
                        filetime::FileTime::from_system_time(mtime),
                    );
                }
            }

            Ok(bytes_written)
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))
        .and_then(|r| r)
        .map(TransferResult::new)
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // For local-to-local operations, delta sync overhead exceeds benefit
        // Even with O(1) rolling hash, the overhead of:
        // - Computing checksums for destination file
        // - Generating delta operations
        // - Applying delta (random seeks + writes)
        // exceeds the cost of a simple sequential copy for local files.
        //
        // Delta sync is beneficial for:
        // - Remote transfers (network bandwidth limited)
        // - Very large files (>1GB) with small changes
        //
        // TODO: Add size-based heuristic (e.g., enable for files >1GB)
        // TODO: Benchmark on SSDs vs HDDs
        tracing::debug!("Local transport: using full copy (delta sync disabled for local-to-local)");
        return self.copy_file(source, dest).await;

        // Original delta sync code (disabled for performance reasons)
        /*
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

        // Skip delta if destination is empty or very small (full copy is faster)
        if dest_size < 4096 {
            tracing::debug!("Destination too small for delta sync, using full copy");
            return self.copy_file(source, dest).await;
        }

        // Run delta sync in blocking task
        let source = source.to_path_buf();
        let dest = dest.to_path_buf();

        tokio::task::spawn_blocking(move || {
            // Calculate block size
            let block_size = calculate_block_size(dest_size);

            // Compute checksums of destination file
            let dest_checksums = compute_checksums(&dest, block_size)
                .map_err(|e| SyncError::CopyError {
                    path: dest.clone(),
                    source: e,
                })?;

            // Generate delta
            let delta = generate_delta(&source, &dest_checksums, block_size)
                .map_err(|e| SyncError::CopyError {
                    path: source.clone(),
                    source: e,
                })?;

            // Calculate compression ratio
            let literal_bytes: u64 = delta.ops.iter()
                .filter_map(|op| {
                    if let DeltaOp::Data(data) = op {
                        Some(data.len() as u64)
                    } else {
                        None
                    }
                })
                .sum();

            let compression_ratio = if source_size > 0 {
                (literal_bytes as f64 / source_size as f64) * 100.0
            } else {
                0.0
            };

            // Apply delta to create temporary file
            let temp_dest = dest.with_extension("sy.tmp");
            apply_delta(&dest, &delta, &temp_dest)
                .map_err(|e| SyncError::CopyError {
                    path: temp_dest.clone(),
                    source: e,
                })?;

            // Atomic rename
            fs::rename(&temp_dest, &dest).map_err(|e| SyncError::CopyError {
                path: dest.clone(),
                source: e,
            })?;

            tracing::info!(
                "Delta sync: {} ops, {:.1}% literal data",
                delta.ops.len(),
                compression_ratio
            );

            Ok::<(), SyncError>(())
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
        */
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
        assert!(!transport.exists(&root.join("not_exists.txt")).await.unwrap());
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
}
