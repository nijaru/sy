use crate::error::{Result, SyncError};
use crate::sync::scanner::FileEntry;
use std::fs;
use std::path::Path;

pub struct Transferrer {
    dry_run: bool,
}

impl Transferrer {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }

    /// Create a new file or directory
    pub fn create(&self, source: &FileEntry, dest_path: &Path) -> Result<()> {
        if self.dry_run {
            tracing::info!("Would create: {}", dest_path.display());
            return Ok(());
        }

        if source.is_dir {
            self.create_directory(dest_path)?;
        } else {
            self.copy_file(&source.path, dest_path)?;
        }

        Ok(())
    }

    /// Update an existing file
    pub fn update(&self, source: &FileEntry, dest_path: &Path) -> Result<()> {
        if self.dry_run {
            tracing::info!("Would update: {}", dest_path.display());
            return Ok(());
        }

        if !source.is_dir {
            self.copy_file(&source.path, dest_path)?;
        }

        Ok(())
    }

    /// Delete a file or directory
    pub fn delete(&self, dest_path: &Path) -> Result<()> {
        if self.dry_run {
            tracing::info!("Would delete: {}", dest_path.display());
            return Ok(());
        }

        if dest_path.is_dir() {
            fs::remove_dir_all(dest_path).map_err(|e| SyncError::Io(e))?;
        } else {
            fs::remove_file(dest_path).map_err(|e| SyncError::Io(e))?;
        }

        tracing::info!("Deleted: {}", dest_path.display());
        Ok(())
    }

    fn create_directory(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path).map_err(|e| SyncError::Io(e))?;
        tracing::debug!("Created directory: {}", path.display());
        Ok(())
    }

    fn copy_file(&self, source: &Path, dest: &Path) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| SyncError::Io(e))?;
        }

        // Copy file
        fs::copy(source, dest).map_err(|e| SyncError::CopyError {
            path: source.to_path_buf(),
            source: e,
        })?;

        // Preserve modification time
        if let Ok(source_meta) = fs::metadata(source) {
            if let Ok(mtime) = source_meta.modified() {
                let _ = filetime::set_file_mtime(dest, filetime::FileTime::from_system_time(mtime));
            }
        }

        tracing::debug!("Copied: {} -> {}", source.display(), dest.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;
    use tempfile::TempDir;

    #[test]
    fn test_copy_file() {
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
        };

        let transferrer = Transferrer::new(false);
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).unwrap();

        assert!(dest_path.exists());
        assert_eq!(fs::read_to_string(&dest_path).unwrap(), "test content");
    }

    #[test]
    fn test_dry_run() {
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
        };

        let transferrer = Transferrer::new(true); // dry_run = true
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).unwrap();

        // File should NOT exist in dry-run mode
        assert!(!dest_path.exists());
    }

    #[test]
    fn test_create_directory() {
        let dest_dir = TempDir::new().unwrap();

        let dir_entry = FileEntry {
            path: PathBuf::from("/source/subdir"),
            relative_path: PathBuf::from("subdir"),
            size: 0,
            modified: SystemTime::now(),
            is_dir: true,
        };

        let transferrer = Transferrer::new(false);
        let dest_path = dest_dir.path().join("subdir");
        transferrer.create(&dir_entry, &dest_path).unwrap();

        assert!(dest_path.exists());
        assert!(dest_path.is_dir());
    }
}
