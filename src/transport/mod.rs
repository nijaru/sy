pub mod dual;
pub mod local;
pub mod router;
#[cfg(feature = "s3")]
pub mod s3;
pub mod ssh;

use crate::error::Result;
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use std::path::Path;
use std::time::SystemTime;

/// Transport-agnostic file information
///
/// Unlike std::fs::Metadata, this works for both local and remote files
#[derive(Debug, Clone, Copy)]
pub struct FileInfo {
    pub size: u64,
    pub modified: SystemTime,
}

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

    /// Get file information (size and mtime) in a transport-agnostic way
    ///
    /// This works for both local and remote files, unlike metadata() which returns
    /// std::fs::Metadata that can't be constructed for remote files.
    async fn file_info(&self, path: &Path) -> Result<FileInfo> {
        // Default implementation uses metadata()
        let meta = self.metadata(path).await?;
        let modified = meta.modified().map_err(|e| {
            crate::error::SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to get mtime for {}: {}", path.display(), e),
            ))
        })?;
        Ok(FileInfo {
            size: meta.len(),
            modified,
        })
    }

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

    /// Create a symbolic link
    ///
    /// Creates a symbolic link at `dest` pointing to `target`.
    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()>;

    /// Read file contents into a vector
    ///
    /// This is used for cross-transport operations (e.g., remote→local).
    /// Default implementation reads from local filesystem.
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        tokio::fs::read(path).await.map_err(|e| {
            crate::error::SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read file {}: {}", path.display(), e),
            ))
        })
    }

    /// Write file contents from a vector
    ///
    /// This is used for cross-transport operations (e.g., remote→local).
    /// Default implementation writes to local filesystem.
    async fn write_file(
        &self,
        path: &Path,
        data: &[u8],
        mtime: std::time::SystemTime,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        // Create parent directories
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Write file
        let mut file = tokio::fs::File::create(path).await?;
        file.write_all(data).await?;
        file.flush().await?;
        drop(file);

        // Set mtime
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(mtime))?;

        Ok(())
    }

    /// Get modification time for a file
    ///
    /// This is used for cross-transport operations where metadata() doesn't work.
    /// Default implementation uses local filesystem.
    async fn get_mtime(&self, path: &Path) -> Result<std::time::SystemTime> {
        let metadata = tokio::fs::metadata(path).await?;
        metadata.modified().map_err(|e| {
            crate::error::SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to get mtime for {}: {}", path.display(), e),
            ))
        })
    }

    /// Copy file using streaming (for large files)
    ///
    /// Reads and writes in chunks to avoid loading entire file into memory.
    /// Calls progress_callback with (bytes_transferred, total_bytes) after each chunk.
    /// Returns total bytes transferred.
    async fn copy_file_streaming(
        &self,
        source: &Path,
        dest: &Path,
        progress_callback: Option<std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>>,
    ) -> Result<TransferResult> {
        // Default implementation: fall back to read_file/write_file for simplicity
        // Implementations can override for true streaming
        let data = self.read_file(source).await?;
        let total_size = data.len() as u64;
        let mtime = self.get_mtime(source).await?;

        if let Some(callback) = &progress_callback {
            callback(0, total_size);
        }
        self.write_file(dest, &data, mtime).await?;
        if let Some(callback) = &progress_callback {
            callback(total_size, total_size);
        }

        Ok(TransferResult::new(total_size))
    }

    /// Check available disk space at the destination
    ///
    /// Verifies that at least `bytes_needed` (plus 10% buffer) is available.
    /// For remote transports, this may involve executing commands via SSH/SFTP.
    async fn check_disk_space(&self, path: &Path, bytes_needed: u64) -> Result<()> {
        // Default implementation: use local disk space check
        crate::resource::check_disk_space(path, bytes_needed)
    }

    /// Set extended attributes on a file
    ///
    /// For remote transports, executes platform-specific commands via SSH.
    /// On Linux: uses setfattr
    /// On macOS: uses xattr -w
    async fn set_xattrs(&self, path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<()> {
        // Default implementation: use local xattr crate
        #[cfg(unix)]
        {
            let path = path.to_path_buf();
            let xattrs = xattrs.to_vec();

            tokio::task::spawn_blocking(move || {
                for (name, value) in xattrs {
                    if let Err(e) = xattr::set(&path, &name, &value) {
                        tracing::warn!("Failed to set xattr {} on {}: {}", name, path.display(), e);
                    }
                }
            })
            .await
            .map_err(|e| crate::error::SyncError::Io(std::io::Error::other(e.to_string())))?;
        }
        #[cfg(not(unix))]
        {
            let _ = (path, xattrs);
        }
        Ok(())
    }

    /// Set POSIX ACLs on a file
    ///
    /// For remote transports, executes setfacl via SSH.
    async fn set_acls(&self, path: &Path, acls_text: &str) -> Result<()> {
        // Default implementation: use local exacl crate
        #[cfg(unix)]
        {
            use exacl::{setfacl, AclEntry};
            use std::str::FromStr;

            let path = path.to_path_buf();
            let acls_text = acls_text.to_string();

            tokio::task::spawn_blocking(move || {
                let mut acl_entries = Vec::new();
                for line in acls_text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match AclEntry::from_str(line) {
                        Ok(entry) => acl_entries.push(entry),
                        Err(e) => {
                            tracing::warn!("Failed to parse ACL entry '{}' for {}: {}", line, path.display(), e);
                            continue;
                        }
                    }
                }

                if !acl_entries.is_empty() {
                    if let Err(e) = setfacl(&[&path], &acl_entries, None) {
                        tracing::warn!("Failed to set ACLs on {}: {}", path.display(), e);
                    }
                }
            })
            .await
            .map_err(|e| crate::error::SyncError::Io(std::io::Error::other(e.to_string())))?;
        }
        #[cfg(not(unix))]
        {
            let _ = (path, acls_text);
        }
        Ok(())
    }

    /// Set BSD file flags (macOS only)
    ///
    /// For remote transports, executes chflags via SSH.
    async fn set_bsd_flags(&self, path: &Path, flags: u32) -> Result<()> {
        // Default implementation: use local libc chflags
        #[cfg(target_os = "macos")]
        {
            use std::ffi::CString;
            let path = path.to_path_buf();

            tokio::task::spawn_blocking(move || {
                let c_path = match CString::new(path.to_str().unwrap_or("")) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("Failed to create C string for {}: {}", path.display(), e);
                        return;
                    }
                };

                let result = unsafe { libc::chflags(c_path.as_ptr(), flags as _) };
                if result != 0 {
                    tracing::warn!("Failed to set BSD flags on {}: {}", path.display(), std::io::Error::last_os_error());
                }
            })
            .await
            .map_err(|e| crate::error::SyncError::Io(std::io::Error::other(e.to_string())))?;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (path, flags);
        }
        Ok(())
    }
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

    async fn file_info(&self, path: &Path) -> Result<FileInfo> {
        (**self).file_info(path).await
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

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        (**self).create_symlink(target, dest).await
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        (**self).read_file(path).await
    }

    async fn write_file(
        &self,
        path: &Path,
        data: &[u8],
        mtime: std::time::SystemTime,
    ) -> Result<()> {
        (**self).write_file(path, data, mtime).await
    }

    async fn get_mtime(&self, path: &Path) -> Result<std::time::SystemTime> {
        (**self).get_mtime(path).await
    }

    async fn copy_file_streaming(
        &self,
        source: &Path,
        dest: &Path,
        progress_callback: Option<std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>>,
    ) -> Result<TransferResult> {
        (**self)
            .copy_file_streaming(source, dest, progress_callback)
            .await
    }
}
