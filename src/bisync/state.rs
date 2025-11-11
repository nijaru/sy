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

/// Type alias for the state map returned by load_all()
/// Maps path to (source_state, dest_state) tuple
pub type StateMap = HashMap<PathBuf, (Option<SyncState>, Option<SyncState>)>;

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
    states: StateMap,
}

impl BisyncStateDb {
    /// Format version
    const FORMAT_VERSION: &'static str = "v2";

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
    ///
    /// If `force_resync` is true, deletes any existing corrupted state file
    /// and starts fresh (used for recovery from corruption)
    pub fn open(source: &Path, dest: &Path, force_resync: bool) -> Result<Self> {
        let sync_pair_hash = Self::generate_sync_pair_hash(source, dest);
        let state_dir = Self::get_state_dir()?;
        let state_file = state_dir.join(format!("{}.lst", sync_pair_hash));

        // If force_resync is true, delete the state file if it exists (corruption recovery)
        if force_resync && state_file.exists() {
            fs::remove_file(&state_file)?;
        }

        let states = if state_file.exists() {
            // Validate before loading to catch corruption early
            Self::validate_state_file(&state_file)?;
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

    /// Unescape a path string (handles \n, \", \\)
    fn unescape_path(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars();

        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => result.push('\n'),
                    Some('t') => result.push('\t'),
                    Some('r') => result.push('\r'),
                    Some('"') => result.push('"'),
                    Some('\\') => result.push('\\'),
                    Some(other) => {
                        result.push('\\');
                        result.push(other);
                    }
                    None => result.push('\\'),
                }
            } else {
                result.push(c);
            }
        }

        result
    }

    /// Validate state file for corruption before loading
    fn validate_state_file(path: &Path) -> Result<()> {
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().collect::<std::io::Result<Vec<_>>>()?;

        // Check for completely empty file
        if lines.is_empty() {
            return Err(crate::error::SyncError::StateCorruption {
                path: path.to_path_buf(),
                reason: "State file is empty".to_string(),
            });
        }

        // Check for valid header (should start with format version comment)
        let has_header = lines
            .iter()
            .any(|line| line.trim().starts_with("# sy bisync"));

        if !has_header {
            return Err(crate::error::SyncError::StateCorruption {
                path: path.to_path_buf(),
                reason: "Missing or invalid format version header".to_string(),
            });
        }

        // Count non-comment lines for basic sanity check
        let data_lines: Vec<&String> = lines
            .iter()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .collect();

        // If there are data lines, validate structure of first few
        if !data_lines.is_empty() {
            for (idx, line) in data_lines.iter().take(5).enumerate() {
                let parts: Vec<&str> = line.splitn(6, ' ').collect();
                if parts.len() != 5 && parts.len() != 6 {
                    return Err(crate::error::SyncError::StateCorruption {
                        path: path.to_path_buf(),
                        reason: format!(
                            "Invalid field count at line {}: expected 5 or 6 fields, got {}",
                            idx + 1,
                            parts.len()
                        ),
                    });
                }

                // Validate side field
                if parts[0] != "source" && parts[0] != "dest" {
                    return Err(crate::error::SyncError::StateCorruption {
                        path: path.to_path_buf(),
                        reason: format!(
                            "Invalid side field '{}' at line {}: must be 'source' or 'dest'",
                            parts[0],
                            idx + 1
                        ),
                    });
                }

                // Validate mtime is parseable number
                if parts[1].parse::<i64>().is_err() {
                    return Err(crate::error::SyncError::StateCorruption {
                        path: path.to_path_buf(),
                        reason: format!(
                            "Invalid mtime '{}' at line {}: not a valid number",
                            parts[1],
                            idx + 1
                        ),
                    });
                }

                // Validate size is parseable number
                if parts[2].parse::<u64>().is_err() {
                    return Err(crate::error::SyncError::StateCorruption {
                        path: path.to_path_buf(),
                        reason: format!(
                            "Invalid size '{}' at line {}: not a valid number",
                            parts[2],
                            idx + 1
                        ),
                    });
                }

                // Validate checksum format (hex or '-')
                let checksum = parts[3];
                if checksum != "-" && u64::from_str_radix(checksum, 16).is_err() {
                    return Err(crate::error::SyncError::StateCorruption {
                        path: path.to_path_buf(),
                        reason: format!(
                            "Invalid checksum '{}' at line {}: must be hex or '-'",
                            checksum,
                            idx + 1
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    /// Load state from file
    fn load_from_file(path: &Path) -> Result<StateMap> {
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut states: StateMap = HashMap::new();

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            let line = line.trim();

            // Skip comments and blank lines
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse: <side> <mtime_ns> <size> <checksum> <last_sync_ns> <path>
            let parts: Vec<&str> = line.splitn(6, ' ').collect();

            // Support both v1 (5 parts) and v2 (6 parts) formats
            let (side_str, mtime_str, size_str, checksum_str, last_sync_str, path_str) =
                if parts.len() == 6 {
                    // v2 format
                    (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5])
                } else if parts.len() == 5 {
                    // v1 format (backward compat): last_sync = mtime
                    (parts[0], parts[1], parts[2], parts[3], parts[1], parts[4])
                } else {
                    return Err(crate::error::SyncError::Config(format!(
                        "Malformed state file line {}: expected 5 or 6 fields, got {}",
                        line_num + 1,
                        parts.len()
                    )));
                };

            let side = Side::from_str(side_str).ok_or_else(|| {
                crate::error::SyncError::Config(format!(
                    "Invalid side '{}' on line {}",
                    side_str,
                    line_num + 1
                ))
            })?;

            let mtime_ns: i64 = mtime_str.parse().map_err(|_| {
                crate::error::SyncError::Config(format!(
                    "Invalid mtime '{}' on line {}",
                    mtime_str,
                    line_num + 1
                ))
            })?;

            let size: u64 = size_str.parse().map_err(|_| {
                crate::error::SyncError::Config(format!(
                    "Invalid size '{}' on line {}",
                    size_str,
                    line_num + 1
                ))
            })?;

            let checksum: Option<u64> = if checksum_str == "-" {
                None
            } else {
                Some(u64::from_str_radix(checksum_str, 16).map_err(|_| {
                    crate::error::SyncError::Config(format!(
                        "Invalid checksum '{}' on line {}",
                        checksum_str,
                        line_num + 1
                    ))
                })?)
            };

            let last_sync_ns: i64 = last_sync_str.parse().map_err(|_| {
                crate::error::SyncError::Config(format!(
                    "Invalid last_sync '{}' on line {}",
                    last_sync_str,
                    line_num + 1
                ))
            })?;

            // Unquote and unescape path
            let path_unescaped = if path_str.starts_with('"') && path_str.ends_with('"') {
                Self::unescape_path(&path_str[1..path_str.len() - 1])
            } else {
                path_str.to_string()
            };

            let path = PathBuf::from(path_unescaped);

            let state = SyncState {
                path: path.clone(),
                side,
                mtime: UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns as u64),
                size,
                checksum,
                last_sync: UNIX_EPOCH + std::time::Duration::from_nanos(last_sync_ns as u64),
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
            let mut entries: Vec<_> = self.states.iter().collect();
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

    /// Escape a path string (handles \n, \", \\, \t, \r)
    fn escape_path(s: &str) -> String {
        let mut result = String::with_capacity(s.len() + 10);
        result.push('"');

        for c in s.chars() {
            match c {
                '\\' => result.push_str("\\\\"),
                '"' => result.push_str("\\\""),
                '\n' => result.push_str("\\n"),
                '\t' => result.push_str("\\t"),
                '\r' => result.push_str("\\r"),
                _ => result.push(c),
            }
        }

        result.push('"');
        result
    }

    /// Write a single state entry
    fn write_state(&self, file: &mut fs::File, state: &SyncState) -> Result<()> {
        let mtime_ns = state.mtime.duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64;

        let last_sync_ns = state
            .last_sync
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;

        let checksum_str = if let Some(cs) = state.checksum {
            format!("{:x}", cs)
        } else {
            "-".to_string()
        };

        let path_str = state.path.to_string_lossy();
        let path_escaped = Self::escape_path(&path_str);

        writeln!(
            file,
            "{} {} {} {} {} {}",
            state.side.as_str(),
            mtime_ns,
            state.size,
            checksum_str,
            last_sync_ns,
            path_escaped
        )?;

        Ok(())
    }

    /// Store state for a file
    pub fn store(&mut self, state: &SyncState) -> Result<()> {
        let entry = self
            .states
            .entry(state.path.clone())
            .or_insert((None, None));
        match state.side {
            Side::Source => entry.0 = Some(state.clone()),
            Side::Dest => entry.1 = Some(state.clone()),
        }
        self.save_to_file()?;
        Ok(())
    }

    /// Retrieve state for a specific file and side
    #[allow(dead_code)] // Used in tests and future features
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
    pub fn load_all(&self) -> Result<StateMap> {
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
    #[allow(dead_code)] // Future feature: automatic state pruning
    pub fn prune_stale(&mut self, keep_syncs: usize) -> Result<usize> {
        // Not implemented yet - will add in follow-up
        // For now, just return 0 (no pruning)
        let _ = keep_syncs;
        Ok(0)
    }

    /// Get sync pair hash (for logging/debugging)
    #[allow(dead_code)] // Useful for debugging and future features
    pub fn sync_pair_hash(&self) -> String {
        Self::generate_sync_pair_hash(&self.source_path, &self.dest_path)
    }

    /// Log conflicts to history file for audit trail
    pub fn log_conflicts(&self, conflicts: &[crate::bisync::engine::ConflictInfo]) -> Result<()> {
        if conflicts.is_empty() {
            return Ok(());
        }

        let cache_dir = Self::get_state_dir()?;
        let hash = self.sync_pair_hash();
        let log_path = cache_dir.join(format!("{}.conflicts.log", hash));

        // Open in append mode to preserve history
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;

        let now = SystemTime::now();
        let timestamp = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        for conflict in conflicts {
            // Determine winner based on resolution strategy
            let winner = determine_winner(conflict);

            // Format: timestamp | path | conflict_type | strategy | winner
            writeln!(
                file,
                "{} | {} | {} | {} | {}",
                timestamp,
                conflict.path.display(),
                conflict.action,
                format!("{:?}", conflict.resolution).to_lowercase(),
                winner
            )?;
        }

        Ok(())
    }
}

/// Determine winner for conflict logging
fn determine_winner(conflict: &crate::bisync::engine::ConflictInfo) -> String {
    use crate::bisync::resolver::ConflictResolution;

    match conflict.resolution {
        ConflictResolution::Source => "source".to_string(),
        ConflictResolution::Dest => "dest".to_string(),
        ConflictResolution::Rename => "both (renamed)".to_string(),
        ConflictResolution::Newer => {
            if let (Some(s_mtime), Some(d_mtime)) = (conflict.source_mtime, conflict.dest_mtime) {
                if s_mtime > d_mtime {
                    "source (newer)".to_string()
                } else if d_mtime > s_mtime {
                    "dest (newer)".to_string()
                } else {
                    "both (tie)".to_string()
                }
            } else {
                "unknown".to_string()
            }
        }
        ConflictResolution::Larger => {
            if let (Some(s_size), Some(d_size)) = (conflict.source_size, conflict.dest_size) {
                if s_size > d_size {
                    "source (larger)".to_string()
                } else if d_size > s_size {
                    "dest (larger)".to_string()
                } else {
                    "both (tie)".to_string()
                }
            } else {
                "unknown".to_string()
            }
        }
        ConflictResolution::Smaller => {
            if let (Some(s_size), Some(d_size)) = (conflict.source_size, conflict.dest_size) {
                if s_size < d_size {
                    "source (smaller)".to_string()
                } else if d_size < s_size {
                    "dest (smaller)".to_string()
                } else {
                    "both (tie)".to_string()
                }
            } else {
                "unknown".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::Duration;

    fn temp_db() -> (BisyncStateDb, PathBuf) {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source");
        let dest = temp_dir.path().join("dest");
        let db = BisyncStateDb::open(&source, &dest, false).unwrap();
        let temp_path = temp_dir.path().to_path_buf();
        std::mem::forget(temp_dir); // Keep temp dir alive
        (db, temp_path)
    }

    #[test]
    #[serial]
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
    #[serial]
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
    #[serial]
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
    #[serial]
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
    #[serial]
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

        let db1 = BisyncStateDb::open(&source1, &dest, false).unwrap();
        let db2 = BisyncStateDb::open(&source2, &dest, false).unwrap();

        // Different source → different hash
        assert_ne!(db1.sync_pair_hash(), db2.sync_pair_hash());
    }

    #[test]
    fn test_escape_unescape_quotes() {
        let original = r#"file"with"quotes.txt"#;
        let escaped = BisyncStateDb::escape_path(original);
        assert_eq!(escaped, r#""file\"with\"quotes.txt""#);

        let unescaped = BisyncStateDb::unescape_path(&escaped[1..escaped.len() - 1]);
        assert_eq!(unescaped, original);
    }

    #[test]
    fn test_escape_unescape_newlines() {
        let original = "file\nwith\nnewlines.txt";
        let escaped = BisyncStateDb::escape_path(original);
        assert_eq!(escaped, r#""file\nwith\nnewlines.txt""#);

        let unescaped = BisyncStateDb::unescape_path(&escaped[1..escaped.len() - 1]);
        assert_eq!(unescaped, original);
    }

    #[test]
    fn test_escape_unescape_backslashes() {
        let original = r"file\with\backslashes.txt";
        let escaped = BisyncStateDb::escape_path(original);
        assert_eq!(escaped, r#""file\\with\\backslashes.txt""#);

        let unescaped = BisyncStateDb::unescape_path(&escaped[1..escaped.len() - 1]);
        assert_eq!(unescaped, original);
    }

    #[test]
    fn test_escape_unescape_tabs() {
        let original = "file\twith\ttabs.txt";
        let escaped = BisyncStateDb::escape_path(original);
        assert_eq!(escaped, r#""file\twith\ttabs.txt""#);

        let unescaped = BisyncStateDb::unescape_path(&escaped[1..escaped.len() - 1]);
        assert_eq!(unescaped, original);
    }

    #[test]
    #[serial]
    fn test_edge_case_round_trip() {
        let (mut db, _temp) = temp_db();

        // Create state with edge-case filename
        let edge_case_path = PathBuf::from("file\"with\nnewline\tand\\backslash.txt");
        let state = SyncState {
            path: edge_case_path.clone(),
            side: Side::Source,
            mtime: SystemTime::now(),
            size: 1024,
            checksum: Some(0xdeadbeef),
            last_sync: SystemTime::now(),
        };

        // Store and retrieve
        db.store(&state).unwrap();
        let retrieved = db.get(&edge_case_path, Side::Source).unwrap().unwrap();

        // Path should round-trip correctly
        assert_eq!(retrieved.path, edge_case_path);
        assert_eq!(retrieved.size, 1024);
        assert_eq!(retrieved.checksum, Some(0xdeadbeef));
    }

    #[test]
    #[serial]
    fn test_last_sync_separate_from_mtime() {
        let (mut db, _temp) = temp_db();

        let now = SystemTime::now();
        let earlier = now - Duration::from_secs(3600);

        let state = SyncState {
            path: PathBuf::from("test.txt"),
            side: Side::Source,
            mtime: earlier, // File modified 1 hour ago
            size: 1024,
            checksum: None,
            last_sync: now, // But synced just now
        };

        db.store(&state).unwrap();
        let retrieved = db.get(&state.path, Side::Source).unwrap().unwrap();

        // last_sync should be preserved separately
        let mtime_diff = retrieved.mtime.duration_since(earlier).unwrap();
        let sync_diff = retrieved.last_sync.duration_since(now).unwrap();

        assert!(mtime_diff < Duration::from_millis(10)); // Close to earlier
        assert!(sync_diff < Duration::from_millis(10)); // Close to now
    }

    #[test]
    fn test_v1_backward_compatibility() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let _source = temp_dir.path().join("source");
        let _dest = temp_dir.path().join("dest");

        // Create a v1 format state file manually
        let state_dir = temp_dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state_file = state_dir.join("test.lst");

        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v1").unwrap();
        writeln!(file, "# sync_pair: /source <-> /dest").unwrap();
        writeln!(file, "# last_sync: 2025-01-01T00:00:00Z").unwrap();
        writeln!(file, "source 1730000000000000000 1024 abc123 test.txt").unwrap();

        // Load v1 file
        let states = BisyncStateDb::load_from_file(&state_file).unwrap();

        assert_eq!(states.len(), 1);
        let (source_state, _) = states.get(&PathBuf::from("test.txt")).unwrap();
        let state = source_state.as_ref().unwrap();

        // In v1, last_sync should equal mtime (backward compat)
        assert_eq!(state.mtime, state.last_sync);
    }

    #[test]
    fn test_parse_error_handling() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("bad.lst");

        // Create malformed state file
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v2").unwrap();
        writeln!(
            file,
            "source INVALID_NUMBER 1024 - 1730000000000000000 test.txt"
        )
        .unwrap();

        // Should error, not return 0/1970
        let result = BisyncStateDb::load_from_file(&state_file);
        assert!(result.is_err());
    }

    // --- Corruption Detection Tests ---

    #[test]
    fn test_detect_empty_state_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("empty.lst");

        // Create empty file
        std::fs::File::create(&state_file).unwrap();

        // Validation should catch empty file
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("empty"));
    }

    #[test]
    fn test_detect_missing_header() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("no_header.lst");

        // Create file without proper header
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# some random comment").unwrap();
        writeln!(
            file,
            "source 1730000000000000000 1024 - 1730000000000000000 \"test.txt\""
        )
        .unwrap();

        // Validation should catch missing header
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("header"));
    }

    #[test]
    fn test_detect_invalid_side() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("bad_side.lst");

        // Create file with invalid side field
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v2").unwrap();
        writeln!(
            file,
            "invalid_side 1730000000000000000 1024 - 1730000000000000000 \"test.txt\""
        )
        .unwrap();

        // Validation should catch invalid side
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("side"));
    }

    #[test]
    fn test_detect_invalid_mtime() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("bad_mtime.lst");

        // Create file with non-numeric mtime
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v2").unwrap();
        writeln!(
            file,
            "source NOT_A_NUMBER 1024 - 1730000000000000000 \"test.txt\""
        )
        .unwrap();

        // Validation should catch invalid mtime
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("mtime"));
    }

    #[test]
    fn test_detect_invalid_size() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("bad_size.lst");

        // Create file with non-numeric size
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v2").unwrap();
        writeln!(
            file,
            "source 1730000000000000000 NOT_A_SIZE - 1730000000000000000 \"test.txt\""
        )
        .unwrap();

        // Validation should catch invalid size
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("size"));
    }

    #[test]
    fn test_detect_invalid_checksum() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("bad_checksum.lst");

        // Create file with invalid checksum (not hex or '-')
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v2").unwrap();
        writeln!(
            file,
            "source 1730000000000000000 1024 INVALID_HEX 1730000000000000000 \"test.txt\""
        )
        .unwrap();

        // Validation should catch invalid checksum
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("checksum"));
    }

    #[test]
    fn test_detect_wrong_field_count() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("wrong_fields.lst");

        // Create file with wrong number of fields
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v2").unwrap();
        writeln!(file, "source 1024").unwrap(); // Only 2 fields, need 5 or 6

        // Validation should catch field count mismatch
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(err_str.contains("field count"));
    }

    #[test]
    #[serial]
    fn test_force_resync_deletes_corrupt_state() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();

        // Save original XDG_CACHE_HOME to restore later
        let original_cache_home = std::env::var("XDG_CACHE_HOME").ok();

        // Set cache dir BEFORE creating paths (so get_state_dir() uses it)
        std::env::set_var("XDG_CACHE_HOME", temp_dir.path().join("cache"));

        let source = temp_dir.path().join("source");
        let dest = temp_dir.path().join("dest");

        // Create a corrupt state file manually in the actual state directory
        let sync_pair_hash = BisyncStateDb::generate_sync_pair_hash(&source, &dest);
        let state_dir = BisyncStateDb::get_state_dir().unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        let corrupt_file = state_dir.join(format!("{}.lst", sync_pair_hash));

        let mut file = std::fs::File::create(&corrupt_file).unwrap();
        writeln!(file, "CORRUPT DATA").unwrap(); // No header, totally corrupt
        drop(file);

        assert!(corrupt_file.exists());

        // Without force_resync, opening should fail
        let result = BisyncStateDb::open(&source, &dest, false);
        assert!(result.is_err());

        // Verify file still exists after failed open
        assert!(corrupt_file.exists());

        // With force_resync, corrupt file should be deleted and new db created
        let db = BisyncStateDb::open(&source, &dest, true).unwrap();

        // Corrupt file should be deleted
        assert!(!corrupt_file.exists());

        // DB should be empty (fresh start)
        assert_eq!(db.load_all().unwrap().len(), 0);

        // Restore original XDG_CACHE_HOME to avoid leaking to other tests
        match original_cache_home {
            Some(val) => std::env::set_var("XDG_CACHE_HOME", val),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }

    #[test]
    fn test_valid_state_passes_validation() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_file = temp_dir.path().join("valid.lst");

        // Create a properly formatted state file
        let mut file = std::fs::File::create(&state_file).unwrap();
        writeln!(file, "# sy bisync v2").unwrap();
        writeln!(file, "# sync_pair: /source <-> /dest").unwrap();
        writeln!(
            file,
            "source 1730000000000000000 1024 abc123 1730000000000000000 \"test.txt\""
        )
        .unwrap();
        writeln!(
            file,
            "dest 1730000000000000000 1024 - 1730000000000000000 \"test2.txt\""
        )
        .unwrap();

        // Validation should pass
        let result = BisyncStateDb::validate_state_file(&state_file);
        assert!(result.is_ok());
    }
}
