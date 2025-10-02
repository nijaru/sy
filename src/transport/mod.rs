pub mod local;
pub mod ssh;

use crate::error::Result;
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use std::path::Path;

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
    /// This preserves modification time and handles parent directory creation
    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<()>;

    /// Remove a file or directory
    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()>;
}
