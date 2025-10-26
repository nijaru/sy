// Bidirectional sync state tracking
//
// Stores filesystem state from prior sync to detect changes and conflicts.
// Uses text-based format for persistent state storage in ~/.cache/sy/bisync/

use crate::error::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
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

/// Bidirectional sync state database (text-based)
pub struct BisyncStateDb {
    state_file: PathBuf,
    source_path: PathBuf,
    dest_path: PathBuf,
    // In-memory cache for faster lookups
    states: HashMap<PathBuf, (Option<SyncState>, Option<SyncState>)>,
}

impl BisyncStateDb {
    /// Format version
    const FORMAT_VERSION: &'static str = "v1";

    /// Generate unique hash for source+dest pair
    fn generate_sync_pair_hash(source: &Path, dest: &Path) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        source.to_string_lossy().hash(&mut hasher);
        dest.to_string_lossy().hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// Get state directory (~/.cache/sy/bisync/)
    fn get_state_dir() -> Result<PathBuf> {
        let cache_dir = if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
            PathBuf::from(xdg_cache)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".cache")
        } else {
            return Err(crate::error::SyncError::Config(
                "Cannot determine cache directory (HOME not set)".to_string(),
            ));
        };

        let state_dir = cache_dir.join("sy").join("bisync");
        fs::create_dir_all(&state_dir)?;
        Ok(state_dir)
    }

    /// Open or create bisync state database for source/dest pair
    pub fn open(source: &Path, dest: &Path) -> Result<Self> {
        let sync_pair_hash = Self::generate_sync_pair_hash(source, dest);
        let state_dir = Self::get_state_dir()?;
        let state_file = state_dir.join(format!("{}.lst", sync_pair_hash));

        let states = if state_file.exists() {
            Self::load_from_file(&state_file)?
        } else {
            HashMap::new()
        };

        Ok(Self {
            state_file,
            source_path: source.to_path_buf(),
            dest_path: dest.to_path_buf(),
            states,
        })
    }

    /// Load state from file
    fn load_from_file(
        path: &Path,
    ) -> Result<HashMap<PathBuf, (Option<SyncState>, Option<SyncState>)>> {
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut states: HashMap<PathBuf, (Option<SyncState>, Option<SyncState>)> =
            HashMap::new();

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();

            // Skip comments and blank lines
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse: <side> <mtime_ns> <size> <checksum> <path>
            let parts: Vec<&str> = line.splitn(5, ' ').collect();
            if parts.len() != 5 {
                continue; // Skip malformed lines
            }

            let side = match Side::from_str(parts[0]) {
                Some(s) => s,
                None => continue,
            };

            let mtime_ns: i64 = parts[1].parse().unwrap_or(0);
            let size: u64 = parts[2].parse().unwrap_or(0);
            let checksum: Option<u64> = if parts[3] == "-" {
                None
            } else {
                u64::from_str_radix(parts[3], 16).ok()
            };

            // Unquote path if needed
            let path_str = parts[4];
            let path = if path_str.starts_with('"') && path_str.ends_with('"') {
                PathBuf::from(&path_str[1..path_str.len() - 1])
            } else {
                PathBuf::from(path_str)
            };

            let state = SyncState {
                path: path.clone(),
                side,
                mtime: UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns as u64),
                size,
                checksum,
                last_sync: UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns as u64), // Use mtime as last_sync
            };

            let entry = states.entry(path).or_insert((None, None));
            match side {
                Side::Source => entry.0 = Some(state),
                Side::Dest => entry.1 = Some(state),
            }
        }

        Ok(states)
    }

    /// Save all state to file (atomic write)
    fn save_to_file(&self) -> Result<()> {
        let temp_file = self.state_file.with_extension("tmp");

        {
            let mut file = fs::File::create(&temp_file)?;

            // Write header
            writeln!(file, "# sy bisync {}", Self::FORMAT_VERSION)?;
            writeln!(
                file,
                "# sync_pair: {} <-> {}",
                self.source_path.display(),
                self.dest_path.display()
            )?;
            let now = chrono::Utc::now();
            writeln!(file, "# last_sync: {}", now.to_rfc3339())?;

            // Collect and sort entries for deterministic output
            let mut entries: Vec<(&PathBuf, &(Option<SyncState>, Option<SyncState>))> =
                self.states.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));

            // Write each state
            for (_, (source_state, dest_state)) in entries {
                if let Some(state) = source_state {
                    self.write_state(&mut file, state)?;
                }
                if let Some(state) = dest_state {
                    self.write_state(&mut file, state)?;
                }
            }
        }

        // Atomic rename
        fs::rename(&temp_file, &self.state_file)?;

        Ok(())
    }

    /// Write a single state entry
    fn write_state(&self, file: &mut fs::File, state: &SyncState) -> Result<()> {
        let mtime_ns = state
            .mtime
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;

        let checksum_str = if let Some(cs) = state.checksum {
            format!("{:x}", cs)
        } else {
            "-".to_string()
        };

        let path_str = state.path.to_string_lossy();
        let path_formatted = if path_str.contains(' ') || path_str.contains('"') {
            format!("\"{}\"", path_str.replace('"', "\\\""))
        } else {
            path_str.to_string()
        };

        writeln!(
            file,
            "{} {} {} {} {}",
            state.side.as_str(),
            mtime_ns,
            state.size,
            checksum_str,
            path_formatted
        )?;

        Ok(())
    }

    /// Store state for a file
    pub fn store(&mut self, state: &SyncState) -> Result<()> {
        let entry = self.states.entry(state.path.clone()).or_insert((None, None));
        match state.side {
            Side::Source => entry.0 = Some(state.clone()),
            Side::Dest => entry.1 = Some(state.clone()),
        }
        self.save_to_file()?;
        Ok(())
    }

    /// Retrieve state for a specific file and side
    pub fn get(&self, path: &Path, side: Side) -> Result<Option<SyncState>> {
        if let Some((source_state, dest_state)) = self.states.get(path) {
            match side {
                Side::Source => Ok(source_state.clone()),
                Side::Dest => Ok(dest_state.clone()),
            }
        } else {
            Ok(None)
        }
    }

    /// Load all state records
    pub fn load_all(&self) -> Result<HashMap<PathBuf, (Option<SyncState>, Option<SyncState>)>> {
        Ok(self.states.clone())
    }

    /// Delete state for a specific file
    pub fn delete(&mut self, path: &Path) -> Result<()> {
        self.states.remove(path);
        self.save_to_file()?;
        Ok(())
    }

    /// Clear all state (for --clear-bisync-state)
    pub fn clear_all(&mut self) -> Result<()> {
        self.states.clear();
        self.save_to_file()?;
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
    pub fn sync_pair_hash(&self) -> String {
        Self::generate_sync_pair_hash(&self.source_path, &self.dest_path)
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
        std::mem::forget(temp_dir); // Keep temp dir alive
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
