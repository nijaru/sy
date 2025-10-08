use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::scanner::FileEntry;
use crate::transport::{Transport, TransferResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Transferrer<'a, T: Transport> {
    transport: &'a T,
    dry_run: bool,
    symlink_mode: SymlinkMode,
    preserve_xattrs: bool,
    preserve_hardlinks: bool,
    hardlink_map: Arc<Mutex<HashMap<u64, PathBuf>>>, // inode -> first destination path
}

impl<'a, T: Transport> Transferrer<'a, T> {
    pub fn new(
        transport: &'a T,
        dry_run: bool,
        symlink_mode: SymlinkMode,
        preserve_xattrs: bool,
        preserve_hardlinks: bool,
        hardlink_map: Arc<Mutex<HashMap<u64, PathBuf>>>,
    ) -> Self {
        Self {
            transport,
            dry_run,
            symlink_mode,
            preserve_xattrs,
            preserve_hardlinks,
            hardlink_map,
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
            // Check if this is a hardlink we should preserve
            if self.preserve_hardlinks && source.nlink > 1 {
                if let Some(inode) = source.inode {
                    // Check if we've seen this inode before
                    let first_path_opt = {
                        let map = self.hardlink_map.lock().unwrap();
                        map.get(&inode).cloned()
                    }; // Lock is dropped here

                    if let Some(first_path) = first_path_opt {
                        // Create hardlink to the first occurrence
                        tracing::debug!(
                            "Creating hardlink: {} -> {} (inode: {})",
                            dest_path.display(),
                            first_path.display(),
                            inode
                        );
                        self.transport.create_hardlink(&first_path, dest_path).await?;

                        // Return a result indicating no bytes were transferred (hardlink created)
                        return Ok(Some(TransferResult {
                            bytes_written: 0,
                            compression_used: false,
                            transferred_bytes: Some(0),
                            delta_operations: None,
                            literal_bytes: None,
                        }));
                    } else {
                        // First time seeing this inode - copy file and record it
                        let result = self.copy_file(&source.path, dest_path).await?;

                        // Record this as the first path for this inode
                        {
                            let mut map = self.hardlink_map.lock().unwrap();
                            map.insert(inode, dest_path.to_path_buf());
                        } // Lock is dropped here

                        // Write extended attributes if present
                        self.write_xattrs(source, dest_path).await?;

                        return Ok(Some(result));
                    }
                }
            }

            // Not a hardlink or not preserving hardlinks - normal copy
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, true, SymlinkMode::Preserve, false, false, hardlink_map); // dry_run = true
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Follow, false, false, hardlink_map);
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Skip, false, false, hardlink_map);
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, true, false, hardlink_map); // preserve_xattrs = true
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
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map); // preserve_xattrs = false
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        assert!(dest_path.exists());

        // Verify xattrs were NOT preserved
        let xattr = xattr::get(&dest_path, "user.test").unwrap();
        assert!(xattr.is_none(), "Xattr should not be preserved when flag is false");
    }

    #[tokio::test]
    #[cfg(unix)]  // Hardlinks work differently on Windows
    async fn test_hardlink_preservation() {
        use std::os::unix::fs::MetadataExt;

        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create original file
        let original_file = source_dir.path().join("original.txt");
        fs::write(&original_file, "content").unwrap();

        // Create hardlink in source
        let link_file = source_dir.path().join("link.txt");
        fs::hard_link(&original_file, &link_file).unwrap();

        // Get inode
        let original_meta = fs::metadata(&original_file).unwrap();
        let inode = original_meta.ino();

        // Create FileEntries for both
        let original_entry = FileEntry {
            path: original_file.clone(),
            relative_path: PathBuf::from("original.txt"),
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(inode),
            nlink: 2,
                acls: None,
        };

        let link_entry = FileEntry {
            path: link_file.clone(),
            relative_path: PathBuf::from("link.txt"),
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(inode),
            nlink: 2,
                acls: None,
        };

        // Transfer with preserve_hardlinks = true
        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, true, Arc::clone(&hardlink_map));

        // Transfer original first
        let dest_original = dest_dir.path().join("original.txt");
        transferrer.create(&original_entry, &dest_original).await.unwrap();

        // Transfer link second - should create hardlink
        let dest_link = dest_dir.path().join("link.txt");
        transferrer.create(&link_entry, &dest_link).await.unwrap();

        // Both files should exist
        assert!(dest_original.exists());
        assert!(dest_link.exists());

        // They should be hardlinks (same inode)
        let dest_original_meta = fs::metadata(&dest_original).unwrap();
        let dest_link_meta = fs::metadata(&dest_link).unwrap();

        assert_eq!(
            dest_original_meta.ino(),
            dest_link_meta.ino(),
            "Destination files should be hardlinks (same inode)"
        );

        // Both should have nlink = 2
        assert_eq!(dest_original_meta.nlink(), 2);
        assert_eq!(dest_link_meta.nlink(), 2);

        // Verify hardlink_map was updated
        let map = hardlink_map.lock().unwrap();
        assert!(map.contains_key(&inode), "Inode should be in hardlink map");
        assert_eq!(map.get(&inode).unwrap(), &dest_original);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_hardlink_not_preserved_without_flag() {
        use std::os::unix::fs::MetadataExt;

        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create original file
        let original_file = source_dir.path().join("original.txt");
        fs::write(&original_file, "content").unwrap();

        // Create hardlink in source
        let link_file = source_dir.path().join("link.txt");
        fs::hard_link(&original_file, &link_file).unwrap();

        // Get inode
        let original_meta = fs::metadata(&original_file).unwrap();
        let inode = original_meta.ino();

        // Create FileEntries
        let original_entry = FileEntry {
            path: original_file.clone(),
            relative_path: PathBuf::from("original.txt"),
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(inode),
            nlink: 2,
                acls: None,
        };

        let link_entry = FileEntry {
            path: link_file.clone(),
            relative_path: PathBuf::from("link.txt"),
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(inode),
            nlink: 2,
                acls: None,
        };

        // Transfer with preserve_hardlinks = false
        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);

        // Transfer both files
        let dest_original = dest_dir.path().join("original.txt");
        transferrer.create(&original_entry, &dest_original).await.unwrap();

        let dest_link = dest_dir.path().join("link.txt");
        transferrer.create(&link_entry, &dest_link).await.unwrap();

        // Both files should exist
        assert!(dest_original.exists());
        assert!(dest_link.exists());

        // They should NOT be hardlinks (different inodes)
        let dest_original_meta = fs::metadata(&dest_original).unwrap();
        let dest_link_meta = fs::metadata(&dest_link).unwrap();

        assert_ne!(
            dest_original_meta.ino(),
            dest_link_meta.ino(),
            "Destination files should NOT be hardlinks (different inodes) when flag is false"
        );

        // Both should have nlink = 1 (no links)
        assert_eq!(dest_original_meta.nlink(), 1);
        assert_eq!(dest_link_meta.nlink(), 1);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_hardlink_three_files() {
        use std::os::unix::fs::MetadataExt;

        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create original file
        let file1 = source_dir.path().join("file1.txt");
        fs::write(&file1, "content").unwrap();

        // Create two hardlinks
        let file2 = source_dir.path().join("file2.txt");
        let file3 = source_dir.path().join("file3.txt");
        fs::hard_link(&file1, &file2).unwrap();
        fs::hard_link(&file1, &file3).unwrap();

        // Get inode
        let inode = fs::metadata(&file1).unwrap().ino();

        // Create FileEntries
        let entry1 = FileEntry {
            path: file1,
            relative_path: PathBuf::from("file1.txt"),
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(inode),
            nlink: 3,
                acls: None,
        };

        let entry2 = FileEntry {
            path: file2,
            relative_path: PathBuf::from("file2.txt"),
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(inode),
            nlink: 3,
                acls: None,
        };

        let entry3 = FileEntry {
            path: file3,
            relative_path: PathBuf::from("file3.txt"),
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(inode),
            nlink: 3,
                acls: None,
        };

        // Transfer with preserve_hardlinks = true
        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, true, hardlink_map);

        // Transfer all three
        let dest1 = dest_dir.path().join("file1.txt");
        let dest2 = dest_dir.path().join("file2.txt");
        let dest3 = dest_dir.path().join("file3.txt");

        transferrer.create(&entry1, &dest1).await.unwrap();
        transferrer.create(&entry2, &dest2).await.unwrap();
        transferrer.create(&entry3, &dest3).await.unwrap();

        // All should exist
        assert!(dest1.exists());
        assert!(dest2.exists());
        assert!(dest3.exists());

        // All should be hardlinks (same inode)
        let meta1 = fs::metadata(&dest1).unwrap();
        let meta2 = fs::metadata(&dest2).unwrap();
        let meta3 = fs::metadata(&dest3).unwrap();

        assert_eq!(meta1.ino(), meta2.ino());
        assert_eq!(meta1.ino(), meta3.ino());

        // All should have nlink = 3
        assert_eq!(meta1.nlink(), 3);
        assert_eq!(meta2.nlink(), 3);
        assert_eq!(meta3.nlink(), 3);
    }

    // === Error Handling Tests ===

    #[tokio::test]
    async fn test_create_file_nonexistent_source() {
        let temp = tempfile::tempdir().unwrap();
        let nonexistent = temp.path().join("nonexistent.txt");
        let dest = temp.path().join("dest.txt");

        let entry = FileEntry {
            path: nonexistent.clone(),
            relative_path: PathBuf::from("nonexistent.txt"),
            size: 100,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 100,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);

        let result = transferrer.create(&entry, &dest).await;
        assert!(result.is_err(), "Should fail when source file doesn't exist");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_create_file_permission_denied_dest() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        fs::write(&source, b"test").unwrap();

        // Create read-only destination directory
        let dest_dir = temp.path().join("readonly");
        fs::create_dir(&dest_dir).unwrap();
        let mut perms = fs::metadata(&dest_dir).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&dest_dir, perms).unwrap();

        let dest = dest_dir.join("dest.txt");

        let entry = FileEntry {
            path: source.clone(),
            relative_path: PathBuf::from("source.txt"),
            size: 4,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 4,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);

        let result = transferrer.create(&entry, &dest).await;

        // Restore permissions for cleanup
        let mut perms = fs::metadata(&dest_dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest_dir, perms).unwrap();

        assert!(result.is_err(), "Should fail when destination directory is read-only");
    }

    #[tokio::test]
    async fn test_delete_nonexistent_file() {
        let temp = tempfile::tempdir().unwrap();
        let nonexistent = temp.path().join("nonexistent.txt");

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);

        let result = transferrer.delete(&nonexistent, false).await;
        assert!(result.is_err(), "Should fail when trying to delete nonexistent file");
    }

    #[tokio::test]
    async fn test_symlink_preserve_mode() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let link = temp.path().join("link.txt");
        let dest = temp.path().join("dest.txt");

        fs::write(&source, b"test").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&source, &link).unwrap();

        let entry = FileEntry {
            path: link.clone(),
            relative_path: PathBuf::from("link.txt"),
            size: 4,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: true,
            symlink_target: Some(source.clone()),
            is_sparse: false,
            allocated_size: 4,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Preserve, false, false, hardlink_map);

        transferrer.create(&entry, &dest).await.unwrap();

        // Verify symlink was preserved
        let meta = fs::symlink_metadata(&dest).unwrap();
        assert!(meta.is_symlink(), "Destination should be a symlink");
    }

    #[tokio::test]
    async fn test_symlink_follow_mode() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let link = temp.path().join("link.txt");
        let dest = temp.path().join("dest.txt");

        fs::write(&source, b"test content").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&source, &link).unwrap();

        let entry = FileEntry {
            path: link.clone(),
            relative_path: PathBuf::from("link.txt"),
            size: 12,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: true,
            symlink_target: Some(source.clone()),
            is_sparse: false,
            allocated_size: 12,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, false, SymlinkMode::Follow, false, false, hardlink_map);

        transferrer.create(&entry, &dest).await.unwrap();

        // Verify regular file was created (not symlink)
        let meta = fs::symlink_metadata(&dest).unwrap();
        assert!(!meta.is_symlink(), "Destination should be a regular file");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "test content");
    }

    #[tokio::test]
    async fn test_dry_run_no_changes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let dest = temp.path().join("dest.txt");

        fs::write(&source, b"test").unwrap();

        let entry = FileEntry {
            path: source.clone(),
            relative_path: PathBuf::from("source.txt"),
            size: 4,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 4,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let transport = LocalTransport::new();
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let transferrer = Transferrer::new(&transport, true, SymlinkMode::Preserve, false, false, hardlink_map);

        let result = transferrer.create(&entry, &dest).await.unwrap();
        assert!(result.is_none(), "Dry run should return None");
        assert!(!dest.exists(), "Dry run should not create files");
    }
}
