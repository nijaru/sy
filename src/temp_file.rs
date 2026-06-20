use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process counter guaranteeing distinct temp names even when
/// two calls land in the same nanosecond (parallel writes to the same dir).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

    /// Generate a temp file path in the same directory as `target` that won't exceed
    /// the 255-byte filename limit. Uses process ID + nanoseconds + a monotonic
    /// counter to avoid conflicts between concurrent syncs.
    ///
    /// Pattern: `.sy-<8-char-hex>.tmp` — always 17 chars, fits in any filesystem.
    pub fn temp_path_for(target: &Path) -> PathBuf {
        let parent = target.parent().unwrap_or(Path::new("."));
        // PID + nanoseconds + a monotonic counter. The counter guarantees
        // uniqueness across concurrent calls in the same process even when
        // two calls share the same nanosecond.
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed) as u32;
        let hash = pid
            .wrapping_mul(31)
            .wrapping_add(nanos)
            .wrapping_mul(31)
            .wrapping_add(counter);
        let name = format!(".sy-{:08x}.tmp", hash);
        parent.join(name)
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
