// Bidirectional sync state tracking
//
// Stores filesystem state from prior sync to detect changes and conflicts.
// Uses SQLite for persistent state storage in ~/.cache/sy/bisync/

use crate::error::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sync state for a single file
#[derive(Debug, Clone, PartialEq)]
pub struct SyncState {
    pub path: PathBuf,
    pub side: Side,
    pub mtime: SystemTime,
    pub size: u64,
    pub checksum: Option<u64>,
    pub last_sync: SystemTime,
}

/// Which side of the sync (source or destination)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Source,
    Dest,
}

impl Side {
    fn as_str(&self) -> &'static str {
        match self {
            Side::Source => "source",
            Side::Dest => "dest",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "source" => Some(Side::Source),
            "dest" => Some(Side::Dest),
            _ => None,
        }
    }
}

/// Bidirectional sync state database
pub struct BisyncStateDb {
    conn: Connection,
    sync_pair_hash: String,
}

impl BisyncStateDb {
    /// Database schema version
    const SCHEMA_VERSION: i32 = 1;

    /// Generate unique hash for source+dest pair
    fn generate_sync_pair_hash(source: &Path, dest: &Path) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        source.to_string_lossy().hash(&mut hasher);
        dest.to_string_lossy().hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// Get database directory (~/.cache/sy/bisync/)
    fn get_db_dir() -> Result<PathBuf> {
        let cache_dir = if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
            PathBuf::from(xdg_cache)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".cache")
        } else {
            return Err(crate::error::SyncError::Config(
                "Cannot determine cache directory (HOME not set)".to_string(),
            ));
        };

        let db_dir = cache_dir.join("sy").join("bisync");
        std::fs::create_dir_all(&db_dir)?;
        Ok(db_dir)
    }

    /// Open or create bisync state database for source/dest pair
    pub fn open(source: &Path, dest: &Path) -> Result<Self> {
        let sync_pair_hash = Self::generate_sync_pair_hash(source, dest);
        let db_dir = Self::get_db_dir()?;
        let db_path = db_dir.join(format!("{}.db", sync_pair_hash));

        let conn = Connection::open(&db_path)?;

        // Create schema if not exists
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sync_state (
                path TEXT NOT NULL,
                side TEXT NOT NULL,
                mtime INTEGER NOT NULL,
                size INTEGER NOT NULL,
                checksum INTEGER,
                last_sync INTEGER NOT NULL,
                PRIMARY KEY (path, side)
            )",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_path_side ON sync_state(path, side)",
            [],
        )?;

        // Version tracking
        conn.execute(
            "CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            [],
        )?;

        // Check/set schema version
        let version: Option<i32> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .ok();

        if version.is_none() {
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)",
                params![Self::SCHEMA_VERSION.to_string()],
            )?;
        }

        Ok(Self {
            conn,
            sync_pair_hash,
        })
    }

    /// Store state for a file
    pub fn store(&mut self, state: &SyncState) -> Result<()> {
        let mtime_ns = state
            .mtime
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;

        let last_sync_ns = state
            .last_sync
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO sync_state (path, side, mtime, size, checksum, last_sync)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                state.path.to_string_lossy(),
                state.side.as_str(),
                mtime_ns,
                state.size as i64,
                state.checksum.map(|c| c as i64),
                last_sync_ns,
            ],
        )?;

        Ok(())
    }

    /// Retrieve state for a specific file and side
    pub fn get(&self, path: &Path, side: Side) -> Result<Option<SyncState>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, side, mtime, size, checksum, last_sync
             FROM sync_state
             WHERE path = ?1 AND side = ?2",
        )?;

        let result = stmt.query_row(
            params![path.to_string_lossy(), side.as_str()],
            |row| {
                let mtime_ns: i64 = row.get(2)?;
                let size: i64 = row.get(3)?;
                let checksum: Option<i64> = row.get(4)?;
                let last_sync_ns: i64 = row.get(5)?;

                Ok(SyncState {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    side: Side::from_str(&row.get::<_, String>(1)?).unwrap(),
                    mtime: UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns as u64),
                    size: size as u64,
                    checksum: checksum.map(|c| c as u64),
                    last_sync: UNIX_EPOCH + std::time::Duration::from_nanos(last_sync_ns as u64),
                })
            },
        );

        match result {
            Ok(state) => Ok(Some(state)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Load all state records
    pub fn load_all(&self) -> Result<HashMap<PathBuf, (Option<SyncState>, Option<SyncState>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, side, mtime, size, checksum, last_sync
             FROM sync_state
             ORDER BY path, side",
        )?;

        let mut states: HashMap<PathBuf, (Option<SyncState>, Option<SyncState>)> =
            HashMap::new();

        let rows = stmt.query_map([], |row| {
            let mtime_ns: i64 = row.get(2)?;
            let size: i64 = row.get(3)?;
            let checksum: Option<i64> = row.get(4)?;
            let last_sync_ns: i64 = row.get(5)?;

            Ok(SyncState {
                path: PathBuf::from(row.get::<_, String>(0)?),
                side: Side::from_str(&row.get::<_, String>(1)?).unwrap(),
                mtime: UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns as u64),
                size: size as u64,
                checksum: checksum.map(|c| c as u64),
                last_sync: UNIX_EPOCH + std::time::Duration::from_nanos(last_sync_ns as u64),
            })
        })?;

        for state_result in rows {
            let state = state_result?;
            let entry = states.entry(state.path.clone()).or_insert((None, None));
            match state.side {
                Side::Source => entry.0 = Some(state),
                Side::Dest => entry.1 = Some(state),
            }
        }

        Ok(states)
    }

    /// Delete state for a specific file
    pub fn delete(&mut self, path: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM sync_state WHERE path = ?1",
            params![path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Clear all state (for --clear-bisync-state)
    pub fn clear_all(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM sync_state", [])?;
        Ok(())
    }

    /// Prune deleted files (files not in recent syncs)
    pub fn prune_stale(&mut self, keep_syncs: usize) -> Result<usize> {
        // Not implemented yet - will add in follow-up
        // For now, just return 0 (no pruning)
        let _ = keep_syncs;
        Ok(0)
    }

    /// Get sync pair hash (for logging/debugging)
    pub fn sync_pair_hash(&self) -> &str {
        &self.sync_pair_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_db() -> (BisyncStateDb, PathBuf) {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let dest = temp_dir.path().join("dest");
        let db = BisyncStateDb::open(&source, &dest).unwrap();
        let temp_path = temp_dir.path().to_path_buf();
        std::mem::forget(temp_dir);  // Keep temp dir alive
        (db, temp_path)
    }

    #[test]
    fn test_store_and_retrieve() {
        let (mut db, _temp) = temp_db();

        let state = SyncState {
            path: PathBuf::from("test.txt"),
            side: Side::Source,
            mtime: SystemTime::now(),
            size: 1024,
            checksum: Some(0x123456789abcdef0),
            last_sync: SystemTime::now(),
        };

        db.store(&state).unwrap();

        let retrieved = db.get(&state.path, Side::Source).unwrap().unwrap();
        assert_eq!(retrieved.path, state.path);
        assert_eq!(retrieved.side, state.side);
        assert_eq!(retrieved.size, state.size);
        assert_eq!(retrieved.checksum, state.checksum);
    }

    #[test]
    fn test_store_both_sides() {
        let (mut db, _temp) = temp_db();

        let source_state = SyncState {
            path: PathBuf::from("test.txt"),
            side: Side::Source,
            mtime: SystemTime::now(),
            size: 1024,
            checksum: Some(0x111),
            last_sync: SystemTime::now(),
        };

        let dest_state = SyncState {
            path: PathBuf::from("test.txt"),
            side: Side::Dest,
            mtime: SystemTime::now() - Duration::from_secs(60),
            size: 2048,
            checksum: Some(0x222),
            last_sync: SystemTime::now(),
        };

        db.store(&source_state).unwrap();
        db.store(&dest_state).unwrap();

        let source_retrieved = db.get(&source_state.path, Side::Source).unwrap().unwrap();
        let dest_retrieved = db.get(&dest_state.path, Side::Dest).unwrap().unwrap();

        assert_eq!(source_retrieved.size, 1024);
        assert_eq!(dest_retrieved.size, 2048);
        assert_eq!(source_retrieved.checksum, Some(0x111));
        assert_eq!(dest_retrieved.checksum, Some(0x222));
    }

    #[test]
    fn test_load_all() {
        let (mut db, _temp) = temp_db();

        let states = vec![
            SyncState {
                path: PathBuf::from("file1.txt"),
                side: Side::Source,
                mtime: SystemTime::now(),
                size: 100,
                checksum: None,
                last_sync: SystemTime::now(),
            },
            SyncState {
                path: PathBuf::from("file1.txt"),
                side: Side::Dest,
                mtime: SystemTime::now(),
                size: 100,
                checksum: None,
                last_sync: SystemTime::now(),
            },
            SyncState {
                path: PathBuf::from("file2.txt"),
                side: Side::Source,
                mtime: SystemTime::now(),
                size: 200,
                checksum: None,
                last_sync: SystemTime::now(),
            },
        ];

        for state in &states {
            db.store(state).unwrap();
        }

        let all_states = db.load_all().unwrap();
        assert_eq!(all_states.len(), 2); // 2 unique paths

        let file1 = all_states.get(&PathBuf::from("file1.txt")).unwrap();
        assert!(file1.0.is_some()); // Source
        assert!(file1.1.is_some()); // Dest

        let file2 = all_states.get(&PathBuf::from("file2.txt")).unwrap();
        assert!(file2.0.is_some()); // Source
        assert!(file2.1.is_none()); // Dest
    }

    #[test]
    fn test_delete() {
        let (mut db, _temp) = temp_db();

        let state = SyncState {
            path: PathBuf::from("test.txt"),
            side: Side::Source,
            mtime: SystemTime::now(),
            size: 1024,
            checksum: None,
            last_sync: SystemTime::now(),
        };

        db.store(&state).unwrap();
        assert!(db.get(&state.path, Side::Source).unwrap().is_some());

        db.delete(&state.path).unwrap();
        assert!(db.get(&state.path, Side::Source).unwrap().is_none());
    }

    #[test]
    fn test_clear_all() {
        let (mut db, _temp) = temp_db();

        for i in 0..10 {
            let state = SyncState {
                path: PathBuf::from(format!("file{}.txt", i)),
                side: Side::Source,
                mtime: SystemTime::now(),
                size: 1024,
                checksum: None,
                last_sync: SystemTime::now(),
            };
            db.store(&state).unwrap();
        }

        let all_before = db.load_all().unwrap();
        assert_eq!(all_before.len(), 10);

        db.clear_all().unwrap();

        let all_after = db.load_all().unwrap();
        assert_eq!(all_after.len(), 0);
    }

    #[test]
    fn test_sync_pair_hash_uniqueness() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source1 = temp_dir.path().join("source1");
        let source2 = temp_dir.path().join("source2");
        let dest = temp_dir.path().join("dest");

        let db1 = BisyncStateDb::open(&source1, &dest).unwrap();
        let db2 = BisyncStateDb::open(&source2, &dest).unwrap();

        // Different source → different hash
        assert_ne!(db1.sync_pair_hash(), db2.sync_pair_hash());
    }
}
