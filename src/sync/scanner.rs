use crate::error::{Result, SyncError};
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<PathBuf>,
    pub is_sparse: bool,
    pub allocated_size: u64, // Actual bytes allocated on disk
    pub xattrs: Option<HashMap<String, Vec<u8>>>, // Extended attributes (if enabled)
}

/// Detect if a file is sparse and get its allocated size
/// Returns (is_sparse, allocated_size)
#[cfg(unix)]
fn detect_sparse_file(_path: &Path, metadata: &std::fs::Metadata) -> (bool, u64) {
    // Get the number of 512-byte blocks allocated
    let blocks = metadata.blocks();
    let file_size = metadata.len();

    // Calculate actual allocated bytes (blocks are always 512 bytes on Unix)
    let allocated_size = blocks * 512;

    // A file is sparse if it uses significantly fewer blocks than its size would suggest
    // We use a threshold of 4KB (8 blocks) to account for filesystem overhead
    let threshold = 4096;
    let is_sparse = file_size > threshold && allocated_size < file_size.saturating_sub(threshold);

    (is_sparse, allocated_size)
}

/// Non-Unix platforms don't support sparse file detection
#[cfg(not(unix))]
fn detect_sparse_file(_path: &Path, metadata: &std::fs::Metadata) -> (bool, u64) {
    // On non-Unix platforms, assume not sparse and allocated size equals file size
    let file_size = metadata.len();
    (false, file_size)
}

/// Read extended attributes from a file
/// Returns None if xattrs are not supported or if reading fails
fn read_xattrs(path: &Path) -> Option<HashMap<String, Vec<u8>>> {
    let mut xattrs = HashMap::new();

    // List all xattr names
    let names = match xattr::list(path) {
        Ok(names) => names,
        Err(_) => return None, // No xattrs or not supported
    };

    for name in names {
        if let Ok(Some(value)) = xattr::get(path, &name) {
            if let Some(name_str) = name.to_str() {
                xattrs.insert(name_str.to_string(), value);
            }
        }
    }

    if xattrs.is_empty() {
        None
    } else {
        Some(xattrs)
    }
}

pub struct Scanner {
    root: PathBuf,
    preserve_xattrs: bool,
}

impl Scanner {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            preserve_xattrs: false,
        }
    }

    pub fn with_xattrs(mut self, preserve_xattrs: bool) -> Self {
        self.preserve_xattrs = preserve_xattrs;
        self
    }

    pub fn scan(&self) -> Result<Vec<FileEntry>> {
        // Pre-allocate with reasonable capacity to reduce allocations
        let mut entries = Vec::with_capacity(256);

        let mut walker = WalkBuilder::new(&self.root);
        walker
            .hidden(false) // Don't skip hidden files by default
            .git_ignore(true) // Respect .gitignore
            .git_global(true) // Respect global gitignore
            .git_exclude(true) // Respect .git/info/exclude
            .filter_entry(|entry| {
                // Skip .git directories
                entry.file_name() != ".git"
            });

        let walker = walker.build();

        for result in walker {
            let entry = result.map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?;

            let path = entry.path().to_path_buf();
            let metadata = entry.metadata().map_err(|e| SyncError::ReadDirError {
                path: path.clone(),
                source: std::io::Error::other(e.to_string()),
            })?;

            // Skip the root directory itself
            if path == self.root {
                continue;
            }

            let relative_path = path
                .strip_prefix(&self.root)
                .map_err(|_| SyncError::InvalidPath { path: path.clone() })?
                .to_path_buf();

            // Check if this is a symlink
            let is_symlink = metadata.is_symlink();
            let symlink_target = if is_symlink {
                // Read the symlink target
                std::fs::read_link(&path).ok()
            } else {
                None
            };

            // Detect sparse files (only for regular files, not directories or symlinks)
            let (is_sparse, allocated_size) = if !metadata.is_dir() && !is_symlink {
                detect_sparse_file(&path, &metadata)
            } else {
                (false, 0)
            };

            // Read extended attributes if enabled
            let xattrs = if self.preserve_xattrs {
                read_xattrs(&path)
            } else {
                None
            };

            entries.push(FileEntry {
                path: path.clone(),
                relative_path,
                size: metadata.len(),
                modified: metadata.modified().map_err(|e| SyncError::ReadDirError {
                    path: path.clone(),
                    source: e,
                })?,
                is_dir: metadata.is_dir(),
                is_symlink,
                symlink_target,
                is_sparse,
                allocated_size,
                xattrs,
            });
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_scanner_basic() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create test structure
        fs::create_dir(root.join("dir1")).unwrap();
        fs::write(root.join("file1.txt"), "content").unwrap();
        fs::write(root.join("dir1/file2.txt"), "content").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        assert!(entries.len() >= 3); // dir1, file1.txt, dir1/file2.txt
        assert!(entries
            .iter()
            .any(|e| e.relative_path == PathBuf::from("file1.txt")));
    }

    #[test]
    fn test_scanner_gitignore() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Initialize git repo (required for .gitignore to work)
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        // Create .gitignore
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "should be ignored").unwrap();
        fs::write(root.join("included.txt"), "should be included").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        // ignored.txt should not appear
        assert!(!entries
            .iter()
            .any(|e| e.relative_path.to_str() == Some("ignored.txt")));
        // included.txt should appear
        assert!(entries
            .iter()
            .any(|e| e.relative_path.to_str() == Some("included.txt")));
    }

    #[test]
    #[cfg(unix)]  // Symlinks work differently on Windows
    fn test_scanner_symlinks() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a regular file
        fs::write(root.join("target.txt"), "target content").unwrap();

        // Create a symlink to the file
        std::os::unix::fs::symlink(root.join("target.txt"), root.join("link.txt")).unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        // Find the symlink entry
        let link_entry = entries
            .iter()
            .find(|e| e.relative_path == PathBuf::from("link.txt"))
            .expect("Symlink should be in scan results");

        assert!(link_entry.is_symlink, "Entry should be marked as symlink");
        assert!(link_entry.symlink_target.is_some(), "Symlink should have a target");

        // The target should be the absolute path to target.txt
        let target = link_entry.symlink_target.as_ref().unwrap();
        assert_eq!(target, &root.join("target.txt"));

        // Find the regular file entry
        let file_entry = entries
            .iter()
            .find(|e| e.relative_path == PathBuf::from("target.txt"))
            .expect("Target file should be in scan results");

        assert!(!file_entry.is_symlink, "Regular file should not be marked as symlink");
        assert!(file_entry.symlink_target.is_none(), "Regular file should have no target");
    }

    #[test]
    #[cfg(unix)] // Sparse files work differently on Windows
    fn test_scanner_sparse_files() {
        use std::io::Write;
        use std::process::Command;

        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a sparse file using dd (Unix command)
        // This ensures we get a real sparse file
        let sparse_path = root.join("sparse.dat");

        // Use dd to create a 10MB sparse file
        let output = Command::new("dd")
            .args(&[
                "if=/dev/zero",
                &format!("of={}", sparse_path.display()),
                "bs=1024",
                "count=0",
                "seek=10240" // Seek to 10MB
            ])
            .output()
            .expect("Failed to create sparse file with dd");

        if !output.status.success() {
            panic!("dd command failed: {:?}", String::from_utf8_lossy(&output.stderr));
        }

        // Write 4KB of actual data at the beginning
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&sparse_path)
            .unwrap();
        let data = vec![0x42; 4096];
        file.write_all(&data).unwrap();
        file.flush().unwrap();
        drop(file);

        // The file size should be 10MB, but allocated size should be much smaller (only 4KB data written)
        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        let sparse_entry = entries
            .iter()
            .find(|e| e.relative_path == PathBuf::from("sparse.dat"))
            .expect("Sparse file should be in scan results");

        assert_eq!(sparse_entry.size, 10 * 1024 * 1024, "File size should be 10MB");

        // Note: Some filesystems (like APFS on macOS) may not create truly sparse files
        // in all situations. If the filesystem doesn't support sparse files, skip assertions.
        if sparse_entry.allocated_size < sparse_entry.size {
            // Filesystem supports sparse files - verify detection works
            assert!(sparse_entry.is_sparse, "File should be detected as sparse (size: {}, allocated: {})", sparse_entry.size, sparse_entry.allocated_size);
            assert!(
                sparse_entry.allocated_size < sparse_entry.size / 2,
                "Allocated size ({}) should be much smaller than file size ({})",
                sparse_entry.allocated_size, sparse_entry.size
            );
        } else {
            // Filesystem doesn't support sparse files - just verify no crash and correct detection
            assert!(!sparse_entry.is_sparse, "Non-sparse file should not be detected as sparse");
        }
    }

    #[test]
    fn test_scanner_regular_file_not_sparse() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a regular file with actual data
        let file_path = root.join("regular.txt");
        let data = vec![0x42; 10 * 1024]; // 10KB of actual data
        fs::write(&file_path, &data).unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        let regular_entry = entries
            .iter()
            .find(|e| e.relative_path == PathBuf::from("regular.txt"))
            .expect("Regular file should be in scan results");

        // Regular file should not be marked as sparse
        assert!(!regular_entry.is_sparse, "Regular file should not be detected as sparse");
        assert_eq!(regular_entry.size, 10 * 1024, "File size should be 10KB");
    }
}
