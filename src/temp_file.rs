use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process counter guaranteeing distinct temp names even when
/// two calls land in the same nanosecond (parallel writes to the same dir).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// RAII guard for temporary filesystem entries that automatically cleans up on
/// drop unless explicitly defused.
pub struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_path_buf()),
        }
    }

    /// Generate a short same-directory staging path.
    pub fn temp_path_for(target: &Path) -> PathBuf {
        let parent = target.parent().unwrap_or(Path::new("."));
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
        parent.join(format!(".sy-{hash:08x}.tmp"))
    }

    pub fn defuse(mut self) {
        self.path = None;
    }

    #[allow(dead_code)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let Some(path) = &self.path else {
            return;
        };

        // `Path::exists` follows symlinks and therefore returns false for a
        // dangling staged symlink. `symlink_metadata` inspects the directory
        // entry itself, so cleanup covers files, valid links, and dangling links.
        if std::fs::symlink_metadata(path).is_ok() {
            let _ = std::fs::remove_file(path);
            tracing::debug!("cleaned temporary entry: {}", path.display());
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
        fs::write(&temp_path, b"test data").unwrap();

        {
            let _guard = TempFileGuard::new(&temp_path);
        }
        assert!(!temp_path.exists());
    }

    #[test]
    fn test_temp_file_guard_defuse() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("test.tmp");
        fs::write(&temp_path, b"test data").unwrap();

        TempFileGuard::new(&temp_path).defuse();
        assert!(temp_path.exists());
    }

    #[test]
    fn test_temp_file_guard_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("nonexistent.tmp");
        {
            let _guard = TempFileGuard::new(&temp_path);
        }
        assert!(!temp_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_temp_file_guard_cleans_dangling_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("dangling.tmp");
        std::os::unix::fs::symlink("missing-target", &temp_path).unwrap();
        assert!(std::fs::symlink_metadata(&temp_path).is_ok());
        assert!(!temp_path.exists());

        {
            let _guard = TempFileGuard::new(&temp_path);
        }
        assert!(std::fs::symlink_metadata(&temp_path).is_err());
    }

    #[test]
    fn test_temp_file_guard_path() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().join("test.tmp");
        let guard = TempFileGuard::new(&temp_path);
        assert_eq!(guard.path(), Some(temp_path.as_path()));
        guard.defuse();
    }
}
