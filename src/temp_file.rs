use std::path::{Path, PathBuf};

/// RAII guard for temporary files that automatically cleans up on drop.
///
/// This ensures temp files are deleted even if:
/// - The program panics
/// - The user interrupts with Ctrl+C
/// - An error occurs during processing
///
/// # Example
///
/// ```rust,no_run
/// use sy::temp_file::TempFileGuard;
/// use std::path::Path;
///
/// let temp_path = Path::new("/tmp/file.sy.tmp");
/// let guard = TempFileGuard::new(temp_path);
///
/// // Do work with temp file...
/// std::fs::write(temp_path, b"data")?;
///
/// // If successful, defuse the guard to prevent deletion
/// guard.defuse();
///
/// // If error occurs or panic happens, drop() will delete the temp file
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    /// Create a new guard for a temporary file path.
    ///
    /// The file will be deleted when this guard is dropped, unless `defuse()` is called.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_path_buf()),
        }
    }

    /// Defuse the guard, preventing automatic cleanup.
    ///
    /// Call this after successfully completing an operation to prevent
    /// the temporary file from being deleted.
    pub fn defuse(mut self) {
        self.path = None;
    }

    /// Get the path to the temporary file.
    #[allow(dead_code)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            // Best-effort cleanup - ignore errors
            // (file might not exist yet, or might have been moved)
            if path.exists() {
                let _ = std::fs::remove_file(path);
                tracing::debug!("Cleaned up temporary file: {}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_temp_file_guard_cleans_up() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("test.tmp");

        // Create file
        fs::write(&temp_path, b"test data").unwrap();
        assert!(temp_path.exists());

        {
            // Guard created but not defused
            let _guard = TempFileGuard::new(&temp_path);
        } // Drop called here

        // File should be deleted
        assert!(!temp_path.exists());
    }

    #[test]
    fn test_temp_file_guard_defuse() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("test.tmp");

        // Create file
        fs::write(&temp_path, b"test data").unwrap();
        assert!(temp_path.exists());

        {
            // Guard created and defused
            let guard = TempFileGuard::new(&temp_path);
            guard.defuse();
        } // Drop called, but path is None

        // File should still exist
        assert!(temp_path.exists());
    }

    #[test]
    fn test_temp_file_guard_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("nonexistent.tmp");

        {
            // Guard for file that doesn't exist yet
            let _guard = TempFileGuard::new(&temp_path);
            // Don't create the file
        } // Drop called - should not panic

        // File still doesn't exist
        assert!(!temp_path.exists());
    }

    #[test]
    fn test_temp_file_guard_path() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("test.tmp");

        let guard = TempFileGuard::new(&temp_path);
        assert_eq!(guard.path(), Some(temp_path.as_path()));

        guard.defuse();
        // Path is cleared after defuse
    }
}
