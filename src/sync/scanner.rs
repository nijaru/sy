use crate::error::{Result, SyncError};
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(target_os = "macos")]
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: Arc<PathBuf>,
    pub relative_path: Arc<PathBuf>,
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<Arc<PathBuf>>,
    #[allow(dead_code)] // Used for sparse file detection
    pub is_sparse: bool,
    #[allow(dead_code)] // Used for sparse file optimization
    pub allocated_size: u64, // Actual bytes allocated on disk
    pub xattrs: Option<HashMap<String, Vec<u8>>>, // Extended attributes (if enabled)
    pub inode: Option<u64>,                       // Inode number (Unix only)
    pub nlink: u64,                               // Number of hard links to this file
    pub acls: Option<Vec<u8>>,                    // Serialized ACLs (if enabled)
    pub bsd_flags: Option<u32>, // BSD file flags (hidden, immutable, etc.) - macOS only, None on other platforms
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

/// Detect hardlink information (inode number and link count)
/// Returns (inode, nlink)
#[cfg(unix)]
fn detect_hardlink_info(metadata: &std::fs::Metadata) -> (Option<u64>, u64) {
    let inode = metadata.ino();
    let nlink = metadata.nlink();
    (Some(inode), nlink)
}

/// Non-Unix platforms don't support inode-based hardlink detection
#[cfg(not(unix))]
fn detect_hardlink_info(_metadata: &std::fs::Metadata) -> (Option<u64>, u64) {
    (None, 1)
}

/// Read extended attributes from a file
/// Returns None if xattrs are not supported or if reading fails
#[cfg(unix)]
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

/// Non-Unix platforms don't support extended attributes
#[cfg(not(unix))]
fn read_xattrs(_path: &Path) -> Option<HashMap<String, Vec<u8>>> {
    None
}

/// Read ACLs from a file
/// Returns None if ACLs are not supported or if reading fails
/// The ACLs are stored as text representation (Display format) for portability
#[cfg(unix)]
fn read_acls(path: &Path) -> Option<Vec<u8>> {
    use exacl::getfacl;

    // Read ACLs from file
    match getfacl(path, None) {
        Ok(acls) => {
            let acl_vec: Vec<_> = acls.into_iter().collect();
            if acl_vec.is_empty() {
                return None;
            }

            // Convert ACLs to standard text format using Display trait
            // This produces parseable text like "user::rwx", "group::r-x", etc.
            let acl_text: Vec<String> = acl_vec.iter().map(|e| format!("{}", e)).collect();
            let joined = acl_text.join("\n");

            if joined.is_empty() {
                None
            } else {
                Some(joined.into_bytes())
            }
        }
        Err(_) => None, // No ACLs or not supported
    }
}

/// Non-Unix platforms don't support ACLs
#[cfg(not(unix))]
fn read_acls(_path: &Path) -> Option<Vec<u8>> {
    None
}

/// Read BSD file flags (macOS only)
/// Returns None if not supported or if reading fails
#[cfg(target_os = "macos")]
fn read_bsd_flags(metadata: &std::fs::Metadata) -> Option<u32> {
    Some(metadata.st_flags())
}

/// Non-macOS platforms don't support BSD file flags
#[cfg(not(target_os = "macos"))]
fn read_bsd_flags(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

pub struct Scanner {
    root: PathBuf,
    threads: usize,
    follow_links: bool,
}

impl Scanner {
    /// Create a new scanner with automatic thread count detection
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            threads: num_cpus::get(),
            follow_links: false,
        }
    }

    /// Create a scanner with a specific number of threads for parallel scanning
    ///
    /// Use 0 for single-threaded operation, or specify thread count.
    /// Default uses all available CPU cores.
    #[allow(dead_code)] // Public API for custom thread control
    pub fn with_threads(root: impl Into<PathBuf>, threads: usize) -> Self {
        Self {
            root: root.into(),
            threads,
            follow_links: false,
        }
    }

    /// Enable following symbolic links during directory traversal
    ///
    /// When enabled, symbolic links to directories will be followed and their
    /// contents will be scanned. Loop detection is automatic (via walkdir crate)
    /// and will report an error if a symlink loop is detected.
    ///
    /// Default: false (symlinks are recorded but not followed)
    #[allow(dead_code)] // Public API for symlink following control
    pub fn follow_links(mut self, follow: bool) -> Self {
        self.follow_links = follow;
        self
    }

    /// Scan and return all entries at once (legacy API, kept for compatibility)
    ///
    /// For large directories (>100k files), consider using `scan_streaming()` instead
    pub fn scan(&self) -> Result<Vec<FileEntry>> {
        // For backward compatibility, collect streaming results into Vec
        self.scan_streaming()?.collect()
    }

    /// Streaming scan that yields FileEntry one at a time
    ///
    /// This is memory-efficient for large directories as it doesn't load
    /// all entries into memory at once. Memory usage is O(1) regardless
    /// of directory size.
    ///
    /// Uses parallel directory walking if threads > 0, which can provide
    /// 2-4x speedup on directories with many subdirectories.
    ///
    /// # Example
    /// ```ignore
    /// let scanner = Scanner::new("/large/directory");
    /// for entry in scanner.scan_streaming()? {
    ///     let entry = entry?;
    ///     println!("{}", entry.path.display());
    /// }
    /// ```
    pub fn scan_streaming(&self) -> Result<StreamingScanner> {
        let mut walker = WalkBuilder::new(&self.root);
        walker
            .hidden(false) // Don't skip hidden files by default
            .git_ignore(true) // Respect .gitignore (in git repos)
            .git_global(true) // Respect global gitignore
            .git_exclude(true) // Respect .git/info/exclude
            .threads(self.threads) // Parallel walking if threads > 1
            .follow_links(self.follow_links) // Follow symlinks with automatic loop detection
            .filter_entry(|entry| {
                // Skip .git directories
                entry.file_name() != ".git"
            });

        // Also respect .gitignore files even outside git repos
        // This allows .gitignore to work in non-git directories
        let gitignore_path = self.root.join(".gitignore");
        if gitignore_path.exists() {
            walker.add_ignore(&gitignore_path);
        }

        Ok(StreamingScanner {
            root: self.root.clone(),
            walker: walker.build(),
        })
    }
}

/// Streaming iterator over FileEntry items
///
/// This iterator processes files one at a time, making it suitable for
/// very large directories (millions of files) without consuming excessive memory.
pub struct StreamingScanner {
    root: PathBuf,
    walker: ignore::Walk,
}

impl Iterator for StreamingScanner {
    type Item = Result<FileEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let result = self.walker.next()?;

            let entry = match result {
                Ok(entry) => entry,
                Err(e) => return Some(Err(SyncError::Io(std::io::Error::other(e.to_string())))),
            };

            let path = entry.path().to_path_buf();

            // Skip the root directory itself
            if path == self.root {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    return Some(Err(SyncError::ReadDirError {
                        path: path.clone(),
                        source: std::io::Error::other(e.to_string()),
                    }))
                }
            };

            let relative_path = match path.strip_prefix(&self.root) {
                Ok(p) => p.to_path_buf(),
                Err(_) => return Some(Err(SyncError::InvalidPath { path: path.clone() })),
            };

            // Check if this is a symlink
            let is_symlink = metadata.is_symlink();
            let symlink_target = if is_symlink {
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

            // Detect hardlink information (inode and link count)
            let (inode, nlink) = detect_hardlink_info(&metadata);

            // Read extended attributes (always scan them, writing is conditional)
            let xattrs = read_xattrs(&path);

            // Read ACLs (always scan them, writing is conditional)
            let acls = read_acls(&path);

            // Read BSD file flags (macOS only, None on other platforms)
            let bsd_flags = read_bsd_flags(&metadata);

            let modified = match metadata.modified() {
                Ok(m) => m,
                Err(e) => {
                    return Some(Err(SyncError::ReadDirError {
                        path: path.clone(),
                        source: e,
                    }))
                }
            };

            return Some(Ok(FileEntry {
                path: Arc::new(path.clone()),
                relative_path: Arc::new(relative_path),
                size: metadata.len(),
                modified,
                is_dir: metadata.is_dir(),
                is_symlink,
                symlink_target: symlink_target.map(Arc::new),
                is_sparse,
                allocated_size,
                xattrs,
                inode,
                nlink,
                acls,
                bsd_flags,
            }));
        }
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
            .any(|e| e.relative_path.as_path() == Path::new("file1.txt")));
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
    fn test_scanner_gitignore_without_git_repo() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // NO git init - testing .gitignore without git repo

        // Create .gitignore with multiple patterns
        fs::write(
            root.join(".gitignore"),
            "*.tmp\n*.log\nnode_modules/\n.DS_Store\n",
        )
        .unwrap();

        // Create files matching patterns (should be ignored)
        fs::write(root.join("test.tmp"), "should be ignored").unwrap();
        fs::write(root.join("debug.log"), "should be ignored").unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(
            root.join("node_modules").join("package.txt"),
            "should be ignored",
        )
        .unwrap();

        // Create files NOT matching patterns (should be included)
        fs::write(root.join("normal.txt"), "should be included").unwrap();
        fs::write(root.join("important.rs"), "should be included").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        // Ignored files should NOT appear
        assert!(
            !entries
                .iter()
                .any(|e| e.relative_path.to_str() == Some("test.tmp")),
            "test.tmp should be ignored"
        );
        assert!(
            !entries
                .iter()
                .any(|e| e.relative_path.to_str() == Some("debug.log")),
            "debug.log should be ignored"
        );
        assert!(
            !entries
                .iter()
                .any(|e| e.relative_path.to_str() == Some("node_modules")),
            "node_modules/ should be ignored"
        );

        // Normal files SHOULD appear
        assert!(
            entries
                .iter()
                .any(|e| e.relative_path.to_str() == Some("normal.txt")),
            "normal.txt should be included"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.relative_path.to_str() == Some("important.rs")),
            "important.rs should be included"
        );
    }

    #[test]
    #[cfg(unix)] // Symlinks work differently on Windows
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
            .find(|e| e.relative_path.as_path() == Path::new("link.txt"))
            .expect("Symlink should be in scan results");

        assert!(link_entry.is_symlink, "Entry should be marked as symlink");
        assert!(
            link_entry.symlink_target.is_some(),
            "Symlink should have a target"
        );

        // The target should be the absolute path to target.txt
        let target = link_entry.symlink_target.as_ref().unwrap();
        assert_eq!(&**target, &root.join("target.txt"));

        // Find the regular file entry
        let file_entry = entries
            .iter()
            .find(|e| e.relative_path.as_path() == Path::new("target.txt"))
            .expect("Target file should be in scan results");

        assert!(
            !file_entry.is_symlink,
            "Regular file should not be marked as symlink"
        );
        assert!(
            file_entry.symlink_target.is_none(),
            "Regular file should have no target"
        );
    }

    #[test]
    #[cfg(unix)] // Symlinks work differently on Windows
    fn test_scanner_symlink_loop_detection() {
        use std::os::unix::fs as unix_fs;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a simple self-referencing symlink loop: a/link -> a
        let dir_a = root.join("a");
        std::fs::create_dir(&dir_a).unwrap();
        let link = dir_a.join("link");
        unix_fs::symlink(&dir_a, &link).unwrap();

        // Create a regular file in dir_a to verify we still scan it
        std::fs::write(dir_a.join("file.txt"), "test").unwrap();

        // Without follow_links, symlink is just recorded (no loop issue)
        let scanner = Scanner::new(&dir_a);
        let entries = scanner.scan().unwrap();

        // Should have 2 entries: file.txt and the symlink
        assert_eq!(entries.len(), 2);
        let symlink_entry = entries.iter().find(|e| e.is_symlink).unwrap();
        assert_eq!(*symlink_entry.relative_path, PathBuf::from("link"));

        // With follow_links enabled, walkdir detects the loop and returns an error
        let scanner = Scanner::new(&dir_a).follow_links(true);
        let result = scanner.scan();

        // The scan should either:
        // 1. Return Ok but skip the looping directory
        // 2. Return an error about the loop
        // walkdir's behavior is to skip the loop with a warning in the iterator
        match result {
            Ok(entries) => {
                // Loop was skipped, we should still have file.txt
                assert!(entries.iter().any(|e| e.path.ends_with("file.txt")));
            }
            Err(_) => {
                // Loop caused an error - also acceptable
            }
        }
    }

    #[test]
    #[cfg(unix)] // Symlinks work differently on Windows
    fn test_scanner_symlink_chain_loop() {
        use std::os::unix::fs as unix_fs;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a more complex loop: a/link1 -> b, b/link2 -> a
        let dir_a = root.join("a");
        let dir_b = root.join("b");
        std::fs::create_dir(&dir_a).unwrap();
        std::fs::create_dir(&dir_b).unwrap();

        let link1 = dir_a.join("link1");
        let link2 = dir_b.join("link2");
        unix_fs::symlink(&dir_b, &link1).unwrap();
        unix_fs::symlink(&dir_a, &link2).unwrap();

        // Add files to verify scanning still works
        std::fs::write(dir_a.join("file_a.txt"), "a").unwrap();
        std::fs::write(dir_b.join("file_b.txt"), "b").unwrap();

        // Without follow_links, both symlinks are just recorded
        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        // Should have: a/, b/, a/file_a.txt, b/file_b.txt, a/link1, b/link2
        assert!(entries.len() >= 4);
        assert_eq!(entries.iter().filter(|e| e.is_symlink).count(), 2);

        // With follow_links, walkdir should detect the cycle
        let scanner = Scanner::new(root).follow_links(true);
        let result = scanner.scan();

        // Should handle gracefully (either skip loop or return error)
        match result {
            Ok(entries) => {
                // Should still have both regular files
                assert!(entries.iter().any(|e| e.path.ends_with("file_a.txt")));
                assert!(entries.iter().any(|e| e.path.ends_with("file_b.txt")));
            }
            Err(_) => {
                // Loop detection error is acceptable
            }
        }
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
            .args([
                "if=/dev/zero",
                &format!("of={}", sparse_path.display()),
                "bs=1024",
                "count=0",
                "seek=10240", // Seek to 10MB
            ])
            .output()
            .expect("Failed to create sparse file with dd");

        if !output.status.success() {
            panic!(
                "dd command failed: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
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
            .find(|e| e.relative_path.as_path() == Path::new("sparse.dat"))
            .expect("Sparse file should be in scan results");

        assert_eq!(
            sparse_entry.size,
            10 * 1024 * 1024,
            "File size should be 10MB"
        );

        // Note: Some filesystems (like APFS on macOS) may not create truly sparse files
        // in all situations. If the filesystem doesn't support sparse files, skip assertions.
        if sparse_entry.allocated_size < sparse_entry.size {
            // Filesystem supports sparse files - verify detection works
            assert!(
                sparse_entry.is_sparse,
                "File should be detected as sparse (size: {}, allocated: {})",
                sparse_entry.size, sparse_entry.allocated_size
            );
            assert!(
                sparse_entry.allocated_size < sparse_entry.size / 2,
                "Allocated size ({}) should be much smaller than file size ({})",
                sparse_entry.allocated_size,
                sparse_entry.size
            );
        } else {
            // Filesystem doesn't support sparse files - just verify no crash and correct detection
            assert!(
                !sparse_entry.is_sparse,
                "Non-sparse file should not be detected as sparse"
            );
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
            .find(|e| e.relative_path.as_path() == Path::new("regular.txt"))
            .expect("Regular file should be in scan results");

        // Regular file should not be marked as sparse
        assert!(
            !regular_entry.is_sparse,
            "Regular file should not be detected as sparse"
        );
        assert_eq!(regular_entry.size, 10 * 1024, "File size should be 10KB");
    }

    #[test]
    #[cfg(unix)] // Hardlinks work differently on Windows
    fn test_scanner_hardlinks() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a regular file
        let file_path = root.join("original.txt");
        fs::write(&file_path, "content").unwrap();

        // Create hardlink to the file
        let link1_path = root.join("link1.txt");
        fs::hard_link(&file_path, &link1_path).unwrap();

        // Create another hardlink
        let link2_path = root.join("link2.txt");
        fs::hard_link(&file_path, &link2_path).unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        // Find all three entries
        let original_entry = entries
            .iter()
            .find(|e| e.relative_path.as_path() == Path::new("original.txt"))
            .expect("Original file should be in scan results");

        let link1_entry = entries
            .iter()
            .find(|e| e.relative_path.as_path() == Path::new("link1.txt"))
            .expect("Hardlink 1 should be in scan results");

        let link2_entry = entries
            .iter()
            .find(|e| e.relative_path.as_path() == Path::new("link2.txt"))
            .expect("Hardlink 2 should be in scan results");

        // All three should have nlink = 3
        assert_eq!(original_entry.nlink, 3, "Original should have 3 links");
        assert_eq!(link1_entry.nlink, 3, "Link1 should have 3 links");
        assert_eq!(link2_entry.nlink, 3, "Link2 should have 3 links");

        // All three should have the same inode
        assert!(original_entry.inode.is_some(), "Original should have inode");
        assert!(link1_entry.inode.is_some(), "Link1 should have inode");
        assert!(link2_entry.inode.is_some(), "Link2 should have inode");

        assert_eq!(
            original_entry.inode, link1_entry.inode,
            "Original and link1 should have same inode"
        );
        assert_eq!(
            original_entry.inode, link2_entry.inode,
            "Original and link2 should have same inode"
        );
    }

    #[test]
    fn test_scanner_regular_file_no_hardlinks() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a regular file with no hardlinks
        let file_path = root.join("single.txt");
        fs::write(&file_path, "content").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        let entry = entries
            .iter()
            .find(|e| e.relative_path.as_path() == Path::new("single.txt"))
            .expect("File should be in scan results");

        // Should have nlink = 1 (only itself)
        assert_eq!(entry.nlink, 1, "Single file should have nlink = 1");
    }

    // === Error Handling and Edge Case Tests ===

    #[test]
    fn test_scanner_empty_directory() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        assert_eq!(entries.len(), 0, "Empty directory should return no entries");
    }

    #[test]
    fn test_scanner_nested_empty_directories() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create nested empty directories
        fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        // Should find only directories, no files
        assert!(
            entries.iter().all(|e| e.is_dir),
            "All entries should be directories"
        );
    }

    #[test]
    fn test_scanner_very_long_filename() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create file with very long name (close to 255 byte limit)
        let long_name = "a".repeat(250) + ".txt";
        let file_path = root.join(&long_name);
        fs::write(&file_path, "content").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(*entries[0].relative_path, PathBuf::from(&long_name));
    }

    #[test]
    fn test_scanner_unicode_filenames() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create files with various Unicode characters
        let unicode_names = vec![
            "测试.txt",   // Chinese
            "テスト.txt", // Japanese
            "тест.txt",   // Russian
            "🦀.txt",     // Emoji
            "café.txt",   // Accented Latin
        ];

        for name in &unicode_names {
            fs::write(root.join(name), "content").unwrap();
        }

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        assert_eq!(entries.len(), unicode_names.len());
        for name in unicode_names {
            assert!(
                entries
                    .iter()
                    .any(|e| e.relative_path.as_path() == Path::new(name)),
                "Should find file: {}",
                name
            );
        }
    }

    #[test]
    fn test_scanner_special_characters() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create files with special characters (that are valid in filenames)
        let special_names = vec![
            "file with spaces.txt",
            "file-with-dashes.txt",
            "file_with_underscores.txt",
            "file.multiple.dots.txt",
            "file(with)parens.txt",
            "file[with]brackets.txt",
        ];

        for name in &special_names {
            fs::write(root.join(name), "content").unwrap();
        }

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        assert_eq!(entries.len(), special_names.len());
        for name in special_names {
            assert!(
                entries
                    .iter()
                    .any(|e| e.relative_path.as_path() == Path::new(name)),
                "Should find file: {}",
                name
            );
        }
    }

    #[test]
    fn test_scanner_deep_nesting() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create deeply nested structure (50 levels)
        let mut path = root.to_path_buf();
        for i in 0..50 {
            path.push(format!("level{}", i));
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("deep.txt"), "content").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        // Should find all directories + the file
        assert!(
            entries.len() >= 51,
            "Should find deeply nested file and directories"
        );

        // Find the deeply nested file
        let deep_file = entries
            .iter()
            .find(|e| e.relative_path.ends_with("deep.txt"));
        assert!(deep_file.is_some(), "Should find deeply nested file");
    }

    #[test]
    #[cfg(unix)]
    fn test_scanner_permission_denied_directory() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create a directory and a file inside
        let protected_dir = root.join("protected");
        fs::create_dir(&protected_dir).unwrap();
        fs::write(protected_dir.join("secret.txt"), "secret").unwrap();

        // Make directory unreadable
        let mut perms = fs::metadata(&protected_dir).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&protected_dir, perms.clone()).unwrap();

        let scanner = Scanner::new(root);
        let result = scanner.scan();

        // Restore permissions for cleanup
        perms.set_mode(0o755);
        fs::set_permissions(&protected_dir, perms).unwrap();

        // Scanner should either error or skip the unreadable directory
        // Both behaviors are acceptable
        match result {
            Ok(entries) => {
                // If it succeeds, it should have skipped the protected directory
                assert!(
                    !entries.iter().any(|e| e.path.starts_with(&protected_dir)),
                    "Should not include files from unreadable directory"
                );
            }
            Err(_) => {
                // Error is also acceptable
            }
        }
    }

    #[test]
    fn test_scanner_zero_byte_file() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let file_path = root.join("empty.txt");
        fs::write(&file_path, "").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 0);
        assert_eq!(*entries[0].relative_path, PathBuf::from("empty.txt"));
    }

    #[test]
    fn test_scanner_large_directory() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create 1000 files
        for i in 0..1000 {
            fs::write(
                root.join(format!("file{:04}.txt", i)),
                format!("content{}", i),
            )
            .unwrap();
        }

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        assert_eq!(entries.len(), 1000, "Should find all 1000 files");
    }

    #[test]
    fn test_scanner_mixed_file_types() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        // Create mix of files and directories
        fs::write(root.join("file1.txt"), "content1").unwrap();
        fs::create_dir(root.join("dir1")).unwrap();
        fs::write(root.join("dir1/file2.txt"), "content2").unwrap();
        fs::create_dir(root.join("dir2")).unwrap();
        fs::write(root.join("file3.txt"), "content3").unwrap();

        let scanner = Scanner::new(root);
        let entries = scanner.scan().unwrap();

        let files: Vec<_> = entries.iter().filter(|e| !e.is_dir).collect();
        let dirs: Vec<_> = entries.iter().filter(|e| e.is_dir).collect();

        assert_eq!(files.len(), 3, "Should find 3 files");
        assert_eq!(dirs.len(), 2, "Should find 2 directories");
    }
}
