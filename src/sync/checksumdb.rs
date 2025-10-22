use crate::error::Result;
use crate::integrity::Checksum;
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Persistent checksum database for fast re-verification
///
/// Stores file checksums with metadata to avoid recomputing on every sync.
/// Uses SQLite for reliability and efficient querying.
#[allow(dead_code)] // Integration with SyncEngine pending
pub struct ChecksumDatabase {
    conn: Connection,
}

#[allow(dead_code)] // Integration with SyncEngine pending
impl ChecksumDatabase {
    /// Database file name in destination directory
    const DB_FILE: &'static str = ".sy-checksums.db";

    /// Database schema version
    const SCHEMA_VERSION: i32 = 1;

    /// Open or create checksum database in destination directory
    pub fn open(dest_path: &Path) -> Result<Self> {
        let db_path = dest_path.join(Self::DB_FILE);
        let conn = Connection::open(&db_path)?;

        // Create schema if not exists
        conn.execute(
            "CREATE TABLE IF NOT EXISTS checksums (
                path TEXT PRIMARY KEY,
                mtime_secs INTEGER NOT NULL,
                mtime_nanos INTEGER NOT NULL,
                size INTEGER NOT NULL,
                checksum_type TEXT NOT NULL,
                checksum BLOB NOT NULL,
                updated_at INTEGER NOT NULL
            )",
            [],
        )?;

        // Create index for faster queries
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_updated_at ON checksums(updated_at)",
            [],
        )?;

        // Store schema version in metadata table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            )",
            [],
        )?;

        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?1)",
            params![Self::SCHEMA_VERSION],
        )?;

        Ok(Self { conn })
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
        let path_str = path.to_string_lossy();
        let (mtime_secs, mtime_nanos) = system_time_to_parts(mtime);

        let mut stmt = self.conn.prepare(
            "SELECT checksum_type, checksum FROM checksums
             WHERE path = ?1 AND mtime_secs = ?2 AND mtime_nanos = ?3 AND size = ?4",
        )?;

        let result = stmt.query_row(
            params![path_str.as_ref(), mtime_secs, mtime_nanos, size as i64],
            |row| {
                let stored_type: String = row.get(0)?;
                let checksum_blob: Vec<u8> = row.get(1)?;
                Ok((stored_type, checksum_blob))
            },
        );

        match result {
            Ok((stored_type, checksum_blob)) => {
                // Verify checksum type matches
                if stored_type != checksum_type {
                    tracing::debug!(
                        "Checksum type mismatch for {}: expected {}, got {}",
                        path.display(),
                        checksum_type,
                        stored_type
                    );
                    return Ok(None);
                }

                // Reconstruct Checksum based on type
                let checksum = match stored_type.as_str() {
                    "fast" => Checksum::Fast(checksum_blob),
                    "cryptographic" => Checksum::Cryptographic(checksum_blob),
                    _ => {
                        tracing::warn!("Unknown checksum type in database: {}", stored_type);
                        return Ok(None);
                    }
                };

                tracing::debug!("Cache hit for {}", path.display());
                Ok(Some(checksum))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                tracing::debug!("Cache miss for {}", path.display());
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Store checksum after successful transfer
    pub fn store_checksum(
        &self,
        path: &Path,
        mtime: SystemTime,
        size: u64,
        checksum: &Checksum,
    ) -> Result<()> {
        let path_str = path.to_string_lossy();
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

        self.conn.execute(
            "INSERT OR REPLACE INTO checksums
             (path, mtime_secs, mtime_nanos, size, checksum_type, checksum, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                path_str.as_ref(),
                mtime_secs,
                mtime_nanos,
                size as i64,
                checksum_type,
                checksum_blob,
                now
            ],
        )?;

        tracing::debug!("Stored checksum for {}", path.display());
        Ok(())
    }

    /// Clear all cached checksums
    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM checksums", [])?;
        tracing::info!("Cleared checksum database");
        Ok(())
    }

    /// Remove checksums for files that no longer exist
    ///
    /// Takes a set of existing file paths and removes database entries
    /// for paths not in the set.
    pub fn prune(&self, existing_files: &HashSet<PathBuf>) -> Result<usize> {
        // Get all paths in database
        let mut stmt = self.conn.prepare("SELECT path FROM checksums")?;
        let db_paths: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Find paths to delete (in database but not in existing_files)
        let mut to_delete = Vec::new();
        for db_path in &db_paths {
            let path = PathBuf::from(db_path);
            if !existing_files.contains(&path) {
                to_delete.push(db_path.clone());
            }
        }

        // Delete stale entries
        let deleted_count = to_delete.len();
        for path in to_delete {
            self.conn
                .execute("DELETE FROM checksums WHERE path = ?1", params![path])?;
        }

        if deleted_count > 0 {
            tracing::info!("Pruned {} stale entries from checksum database", deleted_count);
        }

        Ok(deleted_count)
    }

    /// Get database statistics
    pub fn stats(&self) -> Result<ChecksumDbStats> {
        let total_entries: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM checksums", [], |row| row.get(0))?;

        let fast_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM checksums WHERE checksum_type = 'fast'",
            [],
            |row| row.get(0),
        )?;

        let crypto_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM checksums WHERE checksum_type = 'cryptographic'",
            [],
            |row| row.get(0),
        )?;

        Ok(ChecksumDbStats {
            total_entries: total_entries as usize,
            fast_checksums: fast_count as usize,
            cryptographic_checksums: crypto_count as usize,
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

        // Verify database file was created
        assert!(temp_dir.path().join(ChecksumDatabase::DB_FILE).exists());

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
