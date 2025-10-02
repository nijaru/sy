use crate::error::{Result, SyncError};
use ignore::WalkBuilder;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
}

pub struct Scanner {
    root: PathBuf,
}

impl Scanner {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn scan(&self) -> Result<Vec<FileEntry>> {
        let mut entries = Vec::new();

        let walker = WalkBuilder::new(&self.root)
            .hidden(false)          // Don't skip hidden files by default
            .git_ignore(true)       // Respect .gitignore
            .git_global(true)       // Respect global gitignore
            .git_exclude(true)      // Respect .git/info/exclude
            .filter_entry(|entry| {
                // Skip .git directories
                entry.file_name() != ".git"
            })
            .build();

        for result in walker {
            let entry = result.map_err(|e| {
                SyncError::Io(std::io::Error::other(e.to_string()))
            })?;

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

            entries.push(FileEntry {
                path: path.clone(),
                relative_path,
                size: metadata.len(),
                modified: metadata.modified().map_err(|e| SyncError::ReadDirError {
                    path: path.clone(),
                    source: e,
                })?,
                is_dir: metadata.is_dir(),
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
        assert!(entries.iter().any(|e| e.relative_path == PathBuf::from("file1.txt")));
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
        assert!(!entries.iter().any(|e| e.relative_path.to_str() == Some("ignored.txt")));
        // included.txt should appear
        assert!(entries.iter().any(|e| e.relative_path.to_str() == Some("included.txt")));
    }
}
