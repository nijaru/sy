use crate::error::Result;
use crate::integrity::Checksum;
use fjall::{Config, Keyspace, PartitionHandle};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Entry stored in the checksum database
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChecksumEntry {
    mtime_secs: i64,
    mtime_nanos: i32,
    size: u64,
    checksum_type: String,
    checksum: Vec<u8>,
    updated_at: i64,
}

/// Persistent checksum database for fast re-verification
///
/// Stores file checksums with metadata to avoid recomputing on every sync.
/// Uses fjall LSM-tree for efficient key-value storage.
#[allow(dead_code)] // Integration with SyncEngine pending
pub struct ChecksumDatabase {
    /// Keyspace owns the underlying storage - serves as lifetime anchor for partition.
    /// The partition handle holds references into the keyspace's memory; dropping keyspace
    /// invalidates the partition. Rust's ownership rules (keyspace field) ensure this never happens.
    keyspace: Keyspace,
    partition: PartitionHandle,
}

#[allow(dead_code)] // Integration with SyncEngine pending
impl ChecksumDatabase {
    /// Database directory name in destination directory
    const DB_DIR: &'static str = ".sy-checksums";

    /// Partition name for checksums
    const PARTITION_NAME: &'static str = "checksums";

    /// Open or create checksum database in destination directory
    pub fn open(dest_path: &Path) -> Result<Self> {
        let db_path = dest_path.join(Self::DB_DIR);

        // Create keyspace with default config
        let keyspace = Config::new(&db_path).open()?;

        // Open or create partition for checksums
        let partition = keyspace.open_partition(Self::PARTITION_NAME, Default::default())?;

        Ok(Self {
            keyspace,
            partition,
        })
    }

    /// Convert path to database key (UTF-8 lossy bytes)
    ///
    /// Note: Paths with invalid UTF-8 sequences are converted lossily (replacement
    /// characters) on both encoding and decoding. This is consistent with Rust's
    /// `Path::to_string_lossy()` semantics. Since database operations only use
    /// paths reconstructed from stored keys via the same function, round-tripping
    /// is consistent within a session. Non-UTF-8 paths are not officially supported.
    fn path_to_key(path: &Path) -> Vec<u8> {
        path.to_string_lossy().as_bytes().to_vec()
    }

    /// Get cached checksum if file unchanged (mtime + size match)
    ///
    /// Returns None if:
    /// - No entry found
    /// - File metadata changed (stale cache)
    /// - Checksum type doesn't match
    pub fn get_checksum(
        &self,
        path: &Path,
        mtime: SystemTime,
        size: u64,
        checksum_type: &str,
    ) -> Result<Option<Checksum>> {
        let key = Self::path_to_key(path);
        let (mtime_secs, mtime_nanos) = system_time_to_parts(mtime);

        // Get entry from database
        let value = match self.partition.get(&key)? {
            Some(v) => v,
            None => {
                tracing::debug!("Cache miss for {}", path.display());
                return Ok(None);
            }
        };

        // Deserialize entry
        let entry: ChecksumEntry = bincode::deserialize(&value).map_err(|e| {
            crate::error::SyncError::Database(format!(
                "Failed to deserialize checksum entry for {}: {}",
                path.display(),
                e
            ))
        })?;

        // Verify metadata matches
        if entry.mtime_secs != mtime_secs || entry.mtime_nanos != mtime_nanos || entry.size != size
        {
            tracing::debug!("Metadata mismatch for {}", path.display());
            return Ok(None);
        }

        // Verify checksum type matches
        if entry.checksum_type != checksum_type {
            tracing::debug!(
                "Checksum type mismatch for {}: expected {}, got {}",
                path.display(),
                checksum_type,
                entry.checksum_type
            );
            return Ok(None);
        }

        // Reconstruct Checksum based on type
        let checksum = match entry.checksum_type.as_str() {
            "fast" => Checksum::Fast(entry.checksum),
            "cryptographic" => Checksum::Cryptographic(entry.checksum),
            _ => {
                tracing::warn!("Unknown checksum type in database: {}", entry.checksum_type);
                return Ok(None);
            }
        };

        tracing::debug!("Cache hit for {}", path.display());
        Ok(Some(checksum))
    }

    /// Store checksum after successful transfer
    pub fn store_checksum(
        &self,
        path: &Path,
        mtime: SystemTime,
        size: u64,
        checksum: &Checksum,
    ) -> Result<()> {
        let (mtime_secs, mtime_nanos) = system_time_to_parts(mtime);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let (checksum_type, checksum_blob) = match checksum {
            Checksum::None => return Ok(()), // Don't store None checksums
            Checksum::Fast(bytes) => ("fast", bytes.clone()),
            Checksum::Cryptographic(bytes) => ("cryptographic", bytes.clone()),
        };

        // Create entry
        let entry = ChecksumEntry {
            mtime_secs,
            mtime_nanos,
            size,
            checksum_type: checksum_type.to_string(),
            checksum: checksum_blob,
            updated_at: now,
        };

        // Serialize and store
        let key = Self::path_to_key(path);
        let value = bincode::serialize(&entry)?;
        self.partition.insert(&key, &value)?;

        tracing::debug!("Stored checksum for {}", path.display());
        Ok(())
    }

    /// Clear all cached checksums
    pub fn clear(&self) -> Result<()> {
        // Collect all keys first (can't delete while iterating)
        let keys: Vec<_> = self
            .partition
            .iter()
            .map(|item| item.map(|(k, _)| k.to_vec()))
            .collect::<std::result::Result<_, _>>()?;

        // Delete all entries
        for key in keys {
            self.partition.remove(&key)?;
        }

        tracing::info!("Cleared checksum database");
        Ok(())
    }

    /// Remove checksums for files that no longer exist
    ///
    /// Takes a set of existing file paths and removes database entries
    /// for paths not in the set.
    ///
    /// Note: Uses the same lossy UTF-8 conversion as path_to_key() for consistency.
    /// Paths are matched against the set after round-tripping through to_string_lossy().
    pub fn prune(&self, existing_files: &HashSet<PathBuf>) -> Result<usize> {
        // Collect paths to delete
        let mut to_delete = Vec::new();

        for item in self.partition.iter() {
            let (key, _) = item?;
            // Use same lossy conversion as path_to_key() for consistent matching
            let path_str = String::from_utf8_lossy(&key);
            let path = PathBuf::from(path_str.as_ref());

            if !existing_files.contains(&path) {
                to_delete.push(key.to_vec());
            }
        }

        // Delete stale entries
        let deleted_count = to_delete.len();
        for key in to_delete {
            self.partition.remove(&key)?;
        }

        if deleted_count > 0 {
            tracing::info!(
                "Pruned {} stale entries from checksum database",
                deleted_count
            );
        }

        Ok(deleted_count)
    }

    /// Get database statistics
    pub fn stats(&self) -> Result<ChecksumDbStats> {
        let mut total_entries = 0;
        let mut fast_count = 0;
        let mut crypto_count = 0;

        for item in self.partition.iter() {
            let (key, value) = item?;
            let entry: ChecksumEntry = bincode::deserialize(&value).map_err(|e| {
                crate::error::SyncError::Database(format!(
                    "Failed to deserialize checksum entry for {}: {}",
                    String::from_utf8_lossy(&key),
                    e
                ))
            })?;

            total_entries += 1;
            match entry.checksum_type.as_str() {
                "fast" => fast_count += 1,
                "cryptographic" => crypto_count += 1,
                _ => {}
            }
        }

        Ok(ChecksumDbStats {
            total_entries,
            fast_checksums: fast_count,
            cryptographic_checksums: crypto_count,
        })
    }
}

/// Database statistics
#[derive(Debug, Clone)]
#[allow(dead_code)] // Integration with SyncEngine pending
pub struct ChecksumDbStats {
    pub total_entries: usize,
    pub fast_checksums: usize,
    pub cryptographic_checksums: usize,
}

/// Convert SystemTime to (seconds, nanoseconds) tuple
#[allow(dead_code)] // Integration with SyncEngine pending
fn system_time_to_parts(time: SystemTime) -> (i64, i32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos() as i32),
        Err(_) => (0, 0), // Handle times before UNIX_EPOCH
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_open_database() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        // Verify database directory was created
        assert!(temp_dir.path().join(ChecksumDatabase::DB_DIR).exists());

        // Verify we can query stats
        let stats = db.stats().unwrap();
        assert_eq!(stats.total_entries, 0);
    }

    #[test]
    fn test_store_and_retrieve_checksum() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        let path = PathBuf::from("test/file.txt");
        let mtime = SystemTime::now();
        let size = 1024;
        let checksum = Checksum::Fast(vec![1, 2, 3, 4, 5, 6, 7, 8]);

        // Store checksum
        db.store_checksum(&path, mtime, size, &checksum).unwrap();

        // Retrieve checksum
        let retrieved = db.get_checksum(&path, mtime, size, "fast").unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap(), checksum);

        // Verify stats
        let stats = db.stats().unwrap();
        assert_eq!(stats.total_entries, 1);
        assert_eq!(stats.fast_checksums, 1);
    }

    #[test]
    fn test_cache_miss_on_mtime_change() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        let path = PathBuf::from("test/file.txt");
        let mtime1 = SystemTime::now();
        let mtime2 = mtime1 + std::time::Duration::from_secs(10);
        let size = 1024;
        let checksum = Checksum::Fast(vec![1, 2, 3, 4]);

        // Store with mtime1
        db.store_checksum(&path, mtime1, size, &checksum).unwrap();

        // Try to retrieve with mtime2 (should miss)
        let retrieved = db.get_checksum(&path, mtime2, size, "fast").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_cache_miss_on_size_change() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        let path = PathBuf::from("test/file.txt");
        let mtime = SystemTime::now();
        let size1 = 1024;
        let size2 = 2048;
        let checksum = Checksum::Fast(vec![1, 2, 3, 4]);

        // Store with size1
        db.store_checksum(&path, mtime, size1, &checksum).unwrap();

        // Try to retrieve with size2 (should miss)
        let retrieved = db.get_checksum(&path, mtime, size2, "fast").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_clear_database() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        let path = PathBuf::from("test/file.txt");
        let mtime = SystemTime::now();
        let size = 1024;
        let checksum = Checksum::Fast(vec![1, 2, 3, 4]);

        // Store checksum
        db.store_checksum(&path, mtime, size, &checksum).unwrap();
        assert_eq!(db.stats().unwrap().total_entries, 1);

        // Clear database
        db.clear().unwrap();
        assert_eq!(db.stats().unwrap().total_entries, 0);
    }

    #[test]
    fn test_prune_stale_entries() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        let mtime = SystemTime::now();
        let size = 1024;
        let checksum = Checksum::Fast(vec![1, 2, 3, 4]);

        // Store checksums for 3 files
        db.store_checksum(&PathBuf::from("file1.txt"), mtime, size, &checksum)
            .unwrap();
        db.store_checksum(&PathBuf::from("file2.txt"), mtime, size, &checksum)
            .unwrap();
        db.store_checksum(&PathBuf::from("file3.txt"), mtime, size, &checksum)
            .unwrap();

        assert_eq!(db.stats().unwrap().total_entries, 3);

        // Prune - keep only file1 and file2
        let mut existing = HashSet::new();
        existing.insert(PathBuf::from("file1.txt"));
        existing.insert(PathBuf::from("file2.txt"));

        let pruned = db.prune(&existing).unwrap();
        assert_eq!(pruned, 1); // file3.txt should be pruned
        assert_eq!(db.stats().unwrap().total_entries, 2);
    }

    #[test]
    fn test_cryptographic_checksum_storage() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        let path = PathBuf::from("test/file.txt");
        let mtime = SystemTime::now();
        let size = 1024;
        let checksum = Checksum::Cryptographic(vec![0xde, 0xad, 0xbe, 0xef]);

        // Store cryptographic checksum
        db.store_checksum(&path, mtime, size, &checksum).unwrap();

        // Retrieve with correct type
        let retrieved = db
            .get_checksum(&path, mtime, size, "cryptographic")
            .unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap(), checksum);

        // Try to retrieve with wrong type (should miss)
        let retrieved_wrong = db.get_checksum(&path, mtime, size, "fast").unwrap();
        assert!(retrieved_wrong.is_none());

        // Verify stats
        let stats = db.stats().unwrap();
        assert_eq!(stats.cryptographic_checksums, 1);
        assert_eq!(stats.fast_checksums, 0);
    }

    #[test]
    fn test_update_existing_checksum() {
        let temp_dir = TempDir::new().unwrap();
        let db = ChecksumDatabase::open(temp_dir.path()).unwrap();

        let path = PathBuf::from("test/file.txt");
        let mtime = SystemTime::now();
        let size = 1024;
        let checksum1 = Checksum::Fast(vec![1, 2, 3, 4]);
        let checksum2 = Checksum::Fast(vec![5, 6, 7, 8]);

        // Store initial checksum
        db.store_checksum(&path, mtime, size, &checksum1).unwrap();
        assert_eq!(db.stats().unwrap().total_entries, 1);

        // Update with new checksum (same path, mtime, size)
        db.store_checksum(&path, mtime, size, &checksum2).unwrap();

        // Should still have only 1 entry (replaced, not added)
        assert_eq!(db.stats().unwrap().total_entries, 1);

        // Should retrieve the new checksum
        let retrieved = db.get_checksum(&path, mtime, size, "fast").unwrap();
        assert_eq!(retrieved.unwrap(), checksum2);
    }
}
