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
}

impl TransferResult {
    pub fn new(bytes_written: u64) -> Self {
        Self { bytes_written }
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
}
