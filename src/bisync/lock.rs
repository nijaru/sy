// Concurrent sync safety - file-based locking
//
// Prevents multiple sy processes from syncing the same directory pair simultaneously,
// which could lead to race conditions and data corruption.

use crate::error::Result;
use fs2::FileExt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// Lock guard for a sync pair
/// Lock is automatically released when guard is dropped
#[derive(Debug)]
pub struct SyncLock {
    _lock_file: File,
    lock_path: PathBuf,
}

impl SyncLock {
    /// Acquire exclusive lock for source/dest pair
    ///
    /// Returns error if another process already holds the lock
    pub fn acquire(source: &Path, dest: &Path) -> Result<Self> {
        let lock_path = Self::get_lock_path(source, dest)?;

        // Create lock file
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)?;

        // Try to acquire exclusive lock (non-blocking)
        match lock_file.try_lock_exclusive() {
            Ok(()) => {
                // Write PID to lock file for debugging
                use std::io::Write;
                let mut file_mut = &lock_file;
                let pid = std::process::id();
                writeln!(file_mut, "{}", pid)?;

                Ok(Self {
                    _lock_file: lock_file,
                    lock_path,
                })
            }
            Err(_) => {
                // Lock is held by another process
                Err(crate::error::SyncError::SyncLocked {
                    source_path: source.display().to_string(),
                    dest_path: dest.display().to_string(),
                    lock_file: lock_path.display().to_string(),
                })
            }
        }
    }

    /// Get lock file path for source/dest pair
    fn get_lock_path(source: &Path, dest: &Path) -> Result<PathBuf> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        source.to_string_lossy().hash(&mut hasher);
        dest.to_string_lossy().hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());

        let cache_dir = if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
            PathBuf::from(xdg_cache)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".cache")
        } else {
            return Err(crate::error::SyncError::Config(
                "Cannot determine cache directory (HOME not set)".to_string(),
            ));
        };

        let lock_dir = cache_dir.join("sy").join("locks");
        fs::create_dir_all(&lock_dir)?;

        Ok(lock_dir.join(format!("{}.lock", hash)))
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        // Lock file is automatically unlocked when _lock_file is dropped
        // Clean up lock file on drop (best effort)
        let _ = fs::remove_file(&self.lock_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    #[serial]
    fn test_acquire_lock() {
        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("source");
        let dest = temp_dir.path().join("dest");

        // Save original env var
        let original = std::env::var("XDG_CACHE_HOME").ok();

        // Set cache dir for test isolation
        let cache_dir = temp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache_dir);

        let lock = SyncLock::acquire(&source, &dest).unwrap();

        // Verify lock file exists
        let lock_path = SyncLock::get_lock_path(&source, &dest).unwrap();
        assert!(lock_path.exists());

        drop(lock);

        // Verify lock file is cleaned up
        assert!(!lock_path.exists());

        // Restore original env var
        match original {
            Some(val) => std::env::set_var("XDG_CACHE_HOME", val),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }

    #[test]
    #[serial]
    fn test_concurrent_lock_fails() {
        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("source");
        let dest = temp_dir.path().join("dest");

        // Save original env var
        let original = std::env::var("XDG_CACHE_HOME").ok();

        // Set cache dir for test isolation
        let cache_dir = temp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache_dir);

        // Acquire first lock
        let _lock1 = SyncLock::acquire(&source, &dest).unwrap();

        // Second lock attempt should fail
        let result = SyncLock::acquire(&source, &dest);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("already in progress"));

        // Restore original env var
        match original {
            Some(val) => std::env::set_var("XDG_CACHE_HOME", val),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }

    #[test]
    #[serial]
    fn test_lock_released_on_drop() {
        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("source");
        let dest = temp_dir.path().join("dest");

        // Save original env var
        let original = std::env::var("XDG_CACHE_HOME").ok();

        // Set cache dir for test isolation
        let cache_dir = temp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache_dir);

        {
            let _lock1 = SyncLock::acquire(&source, &dest).unwrap();
            // Lock is held here
        } // lock1 dropped, lock released

        // Should be able to acquire lock again
        let _lock2 = SyncLock::acquire(&source, &dest).unwrap();

        // Restore original env var
        match original {
            Some(val) => std::env::set_var("XDG_CACHE_HOME", val),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }

    #[test]
    #[serial]
    fn test_different_pairs_independent() {
        let temp_dir = TempDir::new().unwrap();
        let source1 = temp_dir.path().join("source1");
        let dest1 = temp_dir.path().join("dest1");
        let source2 = temp_dir.path().join("source2");
        let dest2 = temp_dir.path().join("dest2");

        // Save original env var
        let original = std::env::var("XDG_CACHE_HOME").ok();

        // Set cache dir for test isolation
        let cache_dir = temp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache_dir);

        // Should be able to lock different pairs simultaneously
        let _lock1 = SyncLock::acquire(&source1, &dest1).unwrap();
        let _lock2 = SyncLock::acquire(&source2, &dest2).unwrap();

        // Restore original env var
        match original {
            Some(val) => std::env::set_var("XDG_CACHE_HOME", val),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }

    #[test]
    #[serial]
    fn test_lock_across_threads() {
        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("source");
        let dest = temp_dir.path().join("dest");

        // Save original env var
        let original = std::env::var("XDG_CACHE_HOME").ok();

        // Set cache dir for test isolation (before spawning thread)
        let cache_dir = temp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache_dir);

        // Acquire lock in main thread
        let lock = SyncLock::acquire(&source, &dest).unwrap();

        let source_clone = source.clone();
        let dest_clone = dest.clone();

        // Spawn thread that tries to acquire same lock
        let handle = thread::spawn(move || SyncLock::acquire(&source_clone, &dest_clone));

        // Give thread time to attempt lock acquisition
        thread::sleep(Duration::from_millis(100));

        // Thread should have failed to acquire lock
        let result = handle.join().unwrap();
        assert!(
            result.is_err(),
            "Expected lock acquisition to fail while lock is held"
        );

        // Release lock
        drop(lock);

        // Now should be able to acquire lock again
        let _lock2 = SyncLock::acquire(&source, &dest).unwrap();

        // Restore original env var
        match original {
            Some(val) => std::env::set_var("XDG_CACHE_HOME", val),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }
}
