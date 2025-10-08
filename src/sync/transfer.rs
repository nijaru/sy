use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::scanner::FileEntry;
use crate::transport::{Transport, TransferResult};
use std::path::Path;

pub struct Transferrer<'a, T: Transport> {
    transport: &'a T,
    dry_run: bool,
    symlink_mode: SymlinkMode,
    preserve_xattrs: bool,
}

impl<'a, T: Transport> Transferrer<'a, T> {
    pub fn new(transport: &'a T, dry_run: bool, symlink_mode: SymlinkMode, preserve_xattrs: bool) -> Self {
        Self {
            transport,
            dry_run,
            symlink_mode,
            preserve_xattrs,
        }
    }

    /// Create a new file or directory
    /// Returns Some(TransferResult) for files, None for directories
    pub async fn create(&self, source: &FileEntry, dest_path: &Path) -> Result<Option<TransferResult>> {
        if self.dry_run {
            tracing::info!("Would create: {}", dest_path.display());
            return Ok(None);
        }

        // Handle symlinks based on mode
        if source.is_symlink {
            return self.handle_symlink(source, dest_path).await;
        }

        if source.is_dir {
            self.create_directory(dest_path).await?;
            Ok(None)
        } else {
            let result = self.copy_file(&source.path, dest_path).await?;

            // Write extended attributes if present
            self.write_xattrs(source, dest_path).await?;

            Ok(Some(result))
        }
    }

    /// Update an existing file
    /// Returns Some(TransferResult) for files, None for directories
    pub async fn update(&self, source: &FileEntry, dest_path: &Path) -> Result<Option<TransferResult>> {
        if self.dry_run {
            tracing::info!("Would update: {}", dest_path.display());
            return Ok(None);
        }

        if !source.is_dir {
            // Use delta sync for updates
            let result = self.transport.sync_file_with_delta(&source.path, dest_path).await?;

            // Write extended attributes if present
            self.write_xattrs(source, dest_path).await?;

            tracing::info!("Updated: {} -> {}", source.path.display(), dest_path.display());
            Ok(Some(result))
        } else {
            Ok(None)
        }
    }

    /// Delete a file or directory
    pub async fn delete(&self, dest_path: &Path, is_dir: bool) -> Result<()> {
        if self.dry_run {
            tracing::info!("Would delete: {}", dest_path.display());
            return Ok(());
        }

        self.transport.remove(dest_path, is_dir).await?;
        tracing::info!("Deleted: {}", dest_path.display());
        Ok(())
    }

    async fn create_directory(&self, path: &Path) -> Result<()> {
        self.transport.create_dir_all(path).await?;
        tracing::debug!("Created directory: {}", path.display());
        Ok(())
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            self.transport.create_dir_all(parent).await?;
        }

        // Copy file using transport
        let result = self.transport.copy_file(source, dest).await?;

        tracing::debug!("Copied: {} -> {}", source.display(), dest.display());
        Ok(result)
    }

    /// Write extended attributes to a file
    async fn write_xattrs(&self, file_entry: &FileEntry, dest_path: &Path) -> Result<()> {
        if !self.preserve_xattrs {
            return Ok(());
        }

        if let Some(ref xattrs) = file_entry.xattrs {
            if xattrs.is_empty() {
                return Ok(());
            }

            let dest_path = dest_path.to_path_buf();
            let xattrs_clone = xattrs.clone();

            tokio::task::spawn_blocking(move || {
                for (name, value) in xattrs_clone {
                    if let Err(e) = xattr::set(&dest_path, &name, &value) {
                        tracing::warn!("Failed to set xattr {} on {}: {}", name, dest_path.display(), e);
                    } else {
                        tracing::debug!("Set xattr {} on {}", name, dest_path.display());
                    }
                }
            })
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?;
        }
        Ok(())
    }

    async fn handle_symlink(&self, source: &FileEntry, dest_path: &Path) -> Result<Option<TransferResult>> {
        match self.symlink_mode {
            SymlinkMode::Skip => {
                tracing::debug!("Skipping symlink: {}", source.path.display());
                Ok(None)
            }
            SymlinkMode::Follow => {
                // Follow the symlink and copy the target
                if let Some(ref target) = source.symlink_target {
                    // Check if target exists
                    if !target.exists() {
                        tracing::warn!(
                            "Symlink target does not exist: {} -> {}",
                            source.path.display(),
                            target.display()
                        );
                        return Ok(None);
                    }

                    // Copy the target file/directory
                    if target.is_dir() {
                        tracing::warn!(
                            "Skipping symlink to directory (not supported in follow mode): {}",
                            source.path.display()
                        );
                        Ok(None)
                    } else {
                        let result = self.copy_file(target, dest_path).await?;
                        tracing::debug!(
                            "Followed symlink and copied target: {} -> {}",
                            target.display(),
                            dest_path.display()
                        );
                        Ok(Some(result))
                    }
                } else {
                    tracing::warn!("Symlink has no target: {}", source.path.display());
                    Ok(None)
                }
            }
            SymlinkMode::Preserve => {
                // Preserve the symlink as a symlink
                if let Some(ref target) = source.symlink_target {
                    // Ensure parent directory exists
                    if let Some(parent) = dest_path.parent() {
                        self.transport.create_dir_all(parent).await?;
                    }

                    // Create symlink (only works for local transport currently)
                    #[cfg(unix)]
                    {
                        std::os::unix::fs::symlink(target, dest_path)?;
                        tracing::debug!(
                            "Created symlink: {} -> {}",
                            dest_path.display(),
                            target.display()
                        );
                    }
                    #[cfg(not(unix))]
                    {
                        tracing::warn!("Symlink preservation not supported on this platform");
                    }

                    Ok(None)
                } else {
                    tracing::warn!("Symlink has no target: {}", source.path.display());
                    Ok(None)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::local::LocalTransport;
    use std::fs;
    use std::path::PathBuf;
    use std::time::SystemTime;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_copy_file() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        let file_entry = FileEntry {
            path: source_file.clone(),
            relative_path: PathBuf::from("test.txt"),
            size: 12,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 12,
            xattrs: None,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false);
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        assert!(dest_path.exists());
        assert_eq!(fs::read_to_string(&dest_path).unwrap(), "test content");
    }

    #[tokio::test]
    async fn test_dry_run() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        let file_entry = FileEntry {
            path: source_file.clone(),
            relative_path: PathBuf::from("test.txt"),
            size: 12,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 12,
            xattrs: None,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, true, SymlinkMode::Preserve, false); // dry_run = true
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        // File should NOT exist in dry-run mode
        assert!(!dest_path.exists());
    }

    #[tokio::test]
    async fn test_create_directory() {
        let dest_dir = TempDir::new().unwrap();

        let dir_entry = FileEntry {
            path: PathBuf::from("/source/subdir"),
            relative_path: PathBuf::from("subdir"),
            size: 0,
            modified: SystemTime::now(),
            is_dir: true,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 0,
            xattrs: None,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false);
        let dest_path = dest_dir.path().join("subdir");
        transferrer.create(&dir_entry, &dest_path).await.unwrap();

        assert!(dest_path.exists());
        assert!(dest_path.is_dir());
    }

    #[tokio::test]
    #[cfg(unix)]  // Symlinks work differently on Windows
    async fn test_symlink_preserve() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create a target file
        let target_file = source_dir.path().join("target.txt");
        fs::write(&target_file, "target content").unwrap();

        // Create a symlink
        let link_file = source_dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target_file, &link_file).unwrap();

        // Read link to get target
        let link_target = std::fs::read_link(&link_file).unwrap();

        let file_entry = FileEntry {
            path: link_file.clone(),
            relative_path: PathBuf::from("link.txt"),
            size: 0,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: true,
            symlink_target: Some(link_target.clone()),
            is_sparse: false,
            allocated_size: 0,
            xattrs: None,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false);
        let dest_path = dest_dir.path().join("link.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        // Destination should be a symlink
        assert!(dest_path.exists());
        assert!(dest_path.is_symlink());

        // Symlink target should match
        let dest_target = std::fs::read_link(&dest_path).unwrap();
        assert_eq!(dest_target, link_target);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_symlink_follow() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create a target file
        let target_file = source_dir.path().join("target.txt");
        fs::write(&target_file, "target content").unwrap();

        // Create a symlink
        let link_file = source_dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target_file, &link_file).unwrap();

        let file_entry = FileEntry {
            path: link_file.clone(),
            relative_path: PathBuf::from("link.txt"),
            size: 0,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: true,
            symlink_target: Some(target_file.clone()),
            is_sparse: false,
            allocated_size: 0,
            xattrs: None,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Follow, false);
        let dest_path = dest_dir.path().join("link.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        // Destination should be a regular file (not a symlink)
        assert!(dest_path.exists());
        assert!(!dest_path.is_symlink());

        // Content should match the target
        let content = fs::read_to_string(&dest_path).unwrap();
        assert_eq!(content, "target content");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_symlink_skip() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create a target file
        let target_file = source_dir.path().join("target.txt");
        fs::write(&target_file, "target content").unwrap();

        // Create a symlink
        let link_file = source_dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target_file, &link_file).unwrap();

        let file_entry = FileEntry {
            path: link_file.clone(),
            relative_path: PathBuf::from("link.txt"),
            size: 0,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: true,
            symlink_target: Some(target_file),
            is_sparse: false,
            allocated_size: 0,
            xattrs: None,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Skip, false);
        let dest_path = dest_dir.path().join("link.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        // Destination should NOT exist
        assert!(!dest_path.exists());
    }

    #[tokio::test]
    #[cfg(unix)] // xattrs work differently on different platforms
    async fn test_xattr_preservation() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create a file and set xattrs
        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        // Set some xattrs
        xattr::set(&source_file, "user.test", b"value1").unwrap();
        xattr::set(&source_file, "user.another", b"value2").unwrap();

        let file_entry = FileEntry {
            path: source_file.clone(),
            relative_path: PathBuf::from("test.txt"),
            size: 12,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 12,
            xattrs: Some([
                ("user.test".to_string(), b"value1".to_vec()),
                ("user.another".to_string(), b"value2".to_vec()),
            ].iter().cloned().collect()),
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, true); // preserve_xattrs = true
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        // Verify file exists
        assert!(dest_path.exists());

        // Verify xattrs were preserved
        let xattr1 = xattr::get(&dest_path, "user.test").unwrap().unwrap();
        assert_eq!(xattr1, b"value1");

        let xattr2 = xattr::get(&dest_path, "user.another").unwrap().unwrap();
        assert_eq!(xattr2, b"value2");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_xattr_not_preserved_without_flag() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        xattr::set(&source_file, "user.test", b"value1").unwrap();

        let file_entry = FileEntry {
            path: source_file.clone(),
            relative_path: PathBuf::from("test.txt"),
            size: 12,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 12,
            xattrs: Some([("user.test".to_string(), b"value1".to_vec())].iter().cloned().collect()),
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false); // preserve_xattrs = false
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        assert!(dest_path.exists());

        // Verify xattrs were NOT preserved
        let xattr = xattr::get(&dest_path, "user.test").unwrap();
        assert!(xattr.is_none(), "Xattr should not be preserved when flag is false");
    }
}
