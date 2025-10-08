pub mod dual;
pub mod local;
pub mod router;
pub mod ssh;

use crate::error::Result;
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use std::path::Path;

/// Result of a file transfer operation
#[derive(Debug, Clone, Copy)]
pub struct TransferResult {
    /// Actual bytes written (may differ from file size for delta sync)
    pub bytes_written: u64,
    /// Number of delta operations (None if full file copy)
    pub delta_operations: Option<usize>,
    /// Bytes of literal data transferred via delta (None if full file copy)
    pub literal_bytes: Option<u64>,
    /// Bytes transferred over network (compressed size if compression used)
    pub transferred_bytes: Option<u64>,
    /// Whether compression was used
    pub compression_used: bool,
}

impl TransferResult {
    pub fn new(bytes_written: u64) -> Self {
        Self {
            bytes_written,
            delta_operations: None,
            literal_bytes: None,
            transferred_bytes: None,
            compression_used: false,
        }
    }

    pub fn with_delta(bytes_written: u64, delta_operations: usize, literal_bytes: u64) -> Self {
        Self {
            bytes_written,
            delta_operations: Some(delta_operations),
            literal_bytes: Some(literal_bytes),
            transferred_bytes: None,
            compression_used: false,
        }
    }

    pub fn with_compression(bytes_written: u64, transferred_bytes: u64) -> Self {
        Self {
            bytes_written,
            delta_operations: None,
            literal_bytes: None,
            transferred_bytes: Some(transferred_bytes),
            compression_used: true,
        }
    }

    /// Returns true if this transfer used delta sync
    pub fn used_delta(&self) -> bool {
        self.delta_operations.is_some()
    }

    /// Calculate compression ratio (percentage of file that was literal data)
    /// Returns None if full file copy
    pub fn compression_ratio(&self) -> Option<f64> {
        if let (Some(literal), true) = (self.literal_bytes, self.bytes_written > 0) {
            Some((literal as f64 / self.bytes_written as f64) * 100.0)
        } else {
            None
        }
    }
}

/// Transport abstraction for local and remote file operations
///
/// This trait provides a unified interface for file operations that works
/// across both local filesystems and remote systems (SSH, SFTP, etc.)
#[async_trait]
#[allow(dead_code)] // Methods will be used when we implement SSH transport
pub trait Transport: Send + Sync {
    /// Scan a directory and return all entries
    ///
    /// This recursively scans the directory, respecting .gitignore patterns
    /// and excluding .git directories.
    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>>;

    /// Check if a path exists
    async fn exists(&self, path: &Path) -> Result<bool>;

    /// Get metadata for a path (for comparison during sync)
    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata>;

    /// Create all parent directories for a path
    async fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Copy a file from source to destination
    ///
    /// This preserves modification time and handles parent directory creation.
    /// Returns the number of bytes actually written.
    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult>;

    /// Sync a file using delta sync if destination exists
    ///
    /// This uses the rsync algorithm to transfer only changed blocks when
    /// the destination file already exists. Falls back to full copy if
    /// destination doesn't exist or delta sync isn't beneficial.
    /// Returns the number of bytes actually transferred.
    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Default implementation: fall back to full copy
        self.copy_file(source, dest).await
    }

    /// Remove a file or directory
    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()>;

    /// Create a hard link
    ///
    /// Creates a hard link at `dest` pointing to `source`.
    /// Both paths must be on the same filesystem.
    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()>;
}

// Implement Transport for Arc<T> where T: Transport
// This allows sharing transports across tasks in parallel execution
#[async_trait]
impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>> {
        (**self).scan(path).await
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        (**self).exists(path).await
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        (**self).metadata(path).await
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        (**self).create_dir_all(path).await
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        (**self).copy_file(source, dest).await
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        (**self).sync_file_with_delta(source, dest).await
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        (**self).remove(path, is_dir).await
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        (**self).create_hardlink(source, dest).await
    }
}
