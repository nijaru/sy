use crate::error::{Result, SyncError};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const STATE_FILE_NAME: &str = ".sy-state.json";
const STATE_VERSION: u32 = 1;

/// Resume state for interrupted sync operations
#[derive(Debug, Serialize, Deserialize)]
pub struct ResumeState {
    version: u32,
    source: PathBuf,
    destination: PathBuf,
    started_at: String,
    checkpoint_at: String,
    flags: SyncFlags,
    completed_files: Vec<CompletedFile>,
    total_files: usize,
    total_bytes_transferred: u64,
}

/// Sync flags that must match for resume compatibility
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncFlags {
    pub delete: bool,
    pub exclude: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
}

/// Information about a completed file transfer
#[derive(Debug, Serialize, Deserialize)]
pub struct CompletedFile {
    pub relative_path: PathBuf,
    pub action: String, // "create", "update", "delete"
    pub size: u64,
    pub checksum: String, // "xxhash3:..." format
    pub completed_at: String,
}

impl ResumeState {
    /// Create a new resume state
    pub fn new(
        source: PathBuf,
        destination: PathBuf,
        flags: SyncFlags,
        total_files: usize,
    ) -> Self {
        let now = format_timestamp(SystemTime::now());
        Self {
            version: STATE_VERSION,
            source,
            destination,
            started_at: now.clone(),
            checkpoint_at: now,
            flags,
            completed_files: Vec::new(),
            total_files,
            total_bytes_transferred: 0,
        }
    }

    /// Load resume state from destination directory
    pub fn load(destination: &Path) -> Result<Option<Self>> {
        let state_path = destination.join(STATE_FILE_NAME);

        if !state_path.exists() {
            return Ok(None);
        }

        tracing::debug!("Loading resume state from {}", state_path.display());

        let file = File::open(&state_path).map_err(|e| {
            SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to open state file: {}", e),
            ))
        })?;

        let reader = BufReader::new(file);
        let state: Self = match serde_json::from_reader(reader) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!("Failed to parse resume state (corrupted JSON): {}", e);
                tracing::info!("Deleting corrupted state file and starting fresh");
                Self::delete(destination)?;
                return Ok(None);
            }
        };

        // Verify state integrity
        if let Err(e) = state.verify_integrity() {
            tracing::warn!("Resume state failed integrity check: {}", e);
            tracing::info!("Deleting invalid state file and starting fresh");
            Self::delete(destination)?;
            return Ok(None);
        }

        Ok(Some(state))
    }

    /// Verify the integrity of this resume state
    fn verify_integrity(&self) -> Result<()> {
        // Check version is supported
        if self.version != STATE_VERSION {
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Unsupported state version: expected {}, got {}",
                    STATE_VERSION, self.version
                ),
            )));
        }

        // Check paths are absolute
        if !self.source.is_absolute() {
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Source path is not absolute: {}", self.source.display()),
            )));
        }

        if !self.destination.is_absolute() {
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Destination path is not absolute: {}",
                    self.destination.display()
                ),
            )));
        }

        // Parse and validate timestamps
        let started_at = chrono::DateTime::parse_from_rfc3339(&self.started_at).map_err(|e| {
            SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid started_at timestamp: {}", e),
            ))
        })?;

        let checkpoint_at =
            chrono::DateTime::parse_from_rfc3339(&self.checkpoint_at).map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid checkpoint_at timestamp: {}", e),
                ))
            })?;

        // Check timestamps are reasonable (not in the future + reasonable window)
        let now = chrono::Utc::now();
        if started_at > now {
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Started timestamp is in the future",
            )));
        }

        if checkpoint_at > now {
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Checkpoint timestamp is in the future",
            )));
        }

        // Check checkpoint is not before start
        if checkpoint_at < started_at {
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Checkpoint timestamp is before start timestamp",
            )));
        }

        // Check completed files count doesn't exceed total
        if self.completed_files.len() > self.total_files {
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Completed files ({}) exceeds total files ({})",
                    self.completed_files.len(),
                    self.total_files
                ),
            )));
        }

        // Validate completed file entries
        for file in &self.completed_files {
            // Check action is valid
            match file.action.as_str() {
                "create" | "update" | "delete" => {}
                _ => {
                    return Err(SyncError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Invalid file action: {}", file.action),
                    )));
                }
            }

            // Parse completed_at timestamp
            chrono::DateTime::parse_from_rfc3339(&file.completed_at).map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid completed_at timestamp for file: {}", e),
                ))
            })?;
        }

        Ok(())
    }

    /// Save resume state to destination directory (atomic)
    #[allow(dead_code)] // Public API for manual state saving
    pub fn save(&self, destination: &Path) -> Result<()> {
        let state_path = destination.join(STATE_FILE_NAME);
        let temp_path = destination.join(format!("{}.tmp", STATE_FILE_NAME));

        tracing::trace!("Saving resume state to {}", state_path.display());

        // Write to temporary file
        let file = File::create(&temp_path).map_err(|e| {
            SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to create temp state file: {}", e),
            ))
        })?;

        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, self).map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to write state file: {}",
                e
            )))
        })?;

        // Atomic rename
        std::fs::rename(&temp_path, &state_path).map_err(|e| {
            SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to save state file: {}", e),
            ))
        })?;

        Ok(())
    }

    /// Delete resume state file
    pub fn delete(destination: &Path) -> Result<()> {
        let state_path = destination.join(STATE_FILE_NAME);

        if state_path.exists() {
            tracing::debug!("Deleting resume state file");
            std::fs::remove_file(&state_path).map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Failed to delete state file: {}", e),
                ))
            })?;
        }

        Ok(())
    }

    /// Check if this state is compatible with current sync flags
    pub fn is_compatible_with(&self, current_flags: &SyncFlags) -> bool {
        // Flags must match exactly for safe resume
        self.flags == *current_flags
    }

    /// Add a completed file to the state
    #[allow(dead_code)] // Public API for state management
    pub fn add_completed_file(&mut self, file: CompletedFile, bytes_transferred: u64) {
        self.completed_files.push(file);
        self.total_bytes_transferred += bytes_transferred;
        // Update checkpoint timestamp whenever we modify state
        self.checkpoint_at = format_timestamp(SystemTime::now());
    }

    /// Get the set of completed file paths for quick lookup
    pub fn completed_paths(&self) -> std::collections::HashSet<PathBuf> {
        self.completed_files
            .iter()
            .map(|f| f.relative_path.clone())
            .collect()
    }

    /// Get progress information
    pub fn progress(&self) -> (usize, usize) {
        (self.completed_files.len(), self.total_files)
    }
}

/// Format a timestamp for serialization (ISO 8601)
pub fn format_timestamp(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Create platform-appropriate absolute paths for testing
    fn test_absolute_paths() -> (PathBuf, PathBuf) {
        // Use temp directories which are guaranteed to be absolute on all platforms
        let src_temp = tempdir().unwrap();
        let dst_temp = tempdir().unwrap();
        (src_temp.path().to_path_buf(), dst_temp.path().to_path_buf())
    }

    #[test]
    fn test_resume_state_save_load() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let flags = SyncFlags {
            delete: true,
            exclude: vec!["*.log".to_string()],
            min_size: Some(1024),
            max_size: None,
        };

        let mut state = ResumeState::new(src, dst, flags.clone(), 100);

        state.add_completed_file(
            CompletedFile {
                relative_path: PathBuf::from("file1.txt"),
                action: "create".to_string(),
                size: 1234,
                checksum: "xxhash3:abc123".to_string(),
                completed_at: format_timestamp(SystemTime::now()),
            },
            1234,
        );

        // Save
        state.save(dest).unwrap();

        // Load
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();

        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.total_files, 100);
        assert_eq!(loaded.completed_files.len(), 1);
        assert_eq!(loaded.total_bytes_transferred, 1234);
        assert!(loaded.is_compatible_with(&flags));
    }

    #[test]
    fn test_resume_state_compatibility() {
        let flags1 = SyncFlags {
            delete: true,
            exclude: vec!["*.log".to_string()],
            min_size: Some(1024),
            max_size: None,
        };

        let flags2 = SyncFlags {
            delete: false, // Different!
            exclude: vec!["*.log".to_string()],
            min_size: Some(1024),
            max_size: None,
        };

        let state = ResumeState::new(
            PathBuf::from("/src"),
            PathBuf::from("/dst"),
            flags1.clone(),
            100,
        );

        assert!(state.is_compatible_with(&flags1));
        assert!(!state.is_compatible_with(&flags2));
    }

    #[test]
    fn test_resume_state_delete() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let state = ResumeState::new(src, dst, flags, 10);

        state.save(dest).unwrap();
        assert!(dest.join(STATE_FILE_NAME).exists());

        ResumeState::delete(dest).unwrap();
        assert!(!dest.join(STATE_FILE_NAME).exists());
    }

    #[test]
    fn test_completed_paths() {
        let flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let mut state = ResumeState::new(PathBuf::from("/src"), PathBuf::from("/dst"), flags, 10);

        state.add_completed_file(
            CompletedFile {
                relative_path: PathBuf::from("file1.txt"),
                action: "create".to_string(),
                size: 100,
                checksum: "xxhash3:abc".to_string(),
                completed_at: format_timestamp(SystemTime::now()),
            },
            100,
        );

        state.add_completed_file(
            CompletedFile {
                relative_path: PathBuf::from("file2.txt"),
                action: "update".to_string(),
                size: 200,
                checksum: "xxhash3:def".to_string(),
                completed_at: format_timestamp(SystemTime::now()),
            },
            200,
        );

        let paths = state.completed_paths();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&PathBuf::from("file1.txt")));
        assert!(paths.contains(&PathBuf::from("file2.txt")));
    }

    #[test]
    fn test_corrupted_json_auto_deleted() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let state_path = dest.join(STATE_FILE_NAME);

        // Write corrupted JSON
        std::fs::write(&state_path, "{ invalid json }").unwrap();
        assert!(state_path.exists());

        // Load should return None and delete the file
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_invalid_version_auto_deleted() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();
        let state_path = dest.join(STATE_FILE_NAME);

        // Write state with invalid version
        let invalid_state = serde_json::json!({
            "version": 999,
            "source": src,
            "destination": dst,
            "started_at": "2025-01-01T00:00:00Z",
            "checkpoint_at": "2025-01-01T00:00:00Z",
            "flags": {
                "delete": false,
                "exclude": [],
                "min_size": null,
                "max_size": null
            },
            "completed_files": [],
            "total_files": 10,
            "total_bytes_transferred": 0
        });
        std::fs::write(&state_path, serde_json::to_string(&invalid_state).unwrap()).unwrap();
        assert!(state_path.exists());

        // Load should return None and delete the file
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_relative_paths_rejected() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (_, dst) = test_absolute_paths();
        let state_path = dest.join(STATE_FILE_NAME);

        // Write state with relative source path
        let invalid_state = serde_json::json!({
            "version": STATE_VERSION,
            "source": "relative/path",  // Relative!
            "destination": dst,
            "started_at": "2025-01-01T00:00:00Z",
            "checkpoint_at": "2025-01-01T00:00:00Z",
            "flags": {
                "delete": false,
                "exclude": [],
                "min_size": null,
                "max_size": null
            },
            "completed_files": [],
            "total_files": 10,
            "total_bytes_transferred": 0
        });
        std::fs::write(&state_path, serde_json::to_string(&invalid_state).unwrap()).unwrap();

        // Load should reject and delete
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_future_timestamp_rejected() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();
        let state_path = dest.join(STATE_FILE_NAME);

        // Write state with future timestamp
        let future = chrono::Utc::now() + chrono::Duration::days(1);
        let invalid_state = serde_json::json!({
            "version": STATE_VERSION,
            "source": src,
            "destination": dst,
            "started_at": future.to_rfc3339(),  // Future!
            "checkpoint_at": future.to_rfc3339(),
            "flags": {
                "delete": false,
                "exclude": [],
                "min_size": null,
                "max_size": null
            },
            "completed_files": [],
            "total_files": 10,
            "total_bytes_transferred": 0
        });
        std::fs::write(&state_path, serde_json::to_string(&invalid_state).unwrap()).unwrap();

        // Load should reject and delete
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_checkpoint_before_start_rejected() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();
        let state_path = dest.join(STATE_FILE_NAME);

        let start = chrono::Utc::now();
        let checkpoint = start - chrono::Duration::hours(1); // Before start!

        let invalid_state = serde_json::json!({
            "version": STATE_VERSION,
            "source": src,
            "destination": dst,
            "started_at": start.to_rfc3339(),
            "checkpoint_at": checkpoint.to_rfc3339(),
            "flags": {
                "delete": false,
                "exclude": [],
                "min_size": null,
                "max_size": null
            },
            "completed_files": [],
            "total_files": 10,
            "total_bytes_transferred": 0
        });
        std::fs::write(&state_path, serde_json::to_string(&invalid_state).unwrap()).unwrap();

        // Load should reject and delete
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_completed_exceeds_total_rejected() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();
        let state_path = dest.join(STATE_FILE_NAME);

        let now = chrono::Utc::now();
        let invalid_state = serde_json::json!({
            "version": STATE_VERSION,
            "source": src,
            "destination": dst,
            "started_at": now.to_rfc3339(),
            "checkpoint_at": now.to_rfc3339(),
            "flags": {
                "delete": false,
                "exclude": [],
                "min_size": null,
                "max_size": null
            },
            "completed_files": [
                {
                    "relative_path": "file1.txt",
                    "action": "create",
                    "size": 100,
                    "checksum": "xxhash3:abc",
                    "completed_at": now.to_rfc3339()
                },
                {
                    "relative_path": "file2.txt",
                    "action": "create",
                    "size": 200,
                    "checksum": "xxhash3:def",
                    "completed_at": now.to_rfc3339()
                }
            ],
            "total_files": 1,  // Only 1 but 2 completed!
            "total_bytes_transferred": 300
        });
        std::fs::write(&state_path, serde_json::to_string(&invalid_state).unwrap()).unwrap();

        // Load should reject and delete
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_invalid_file_action_rejected() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();
        let state_path = dest.join(STATE_FILE_NAME);

        let now = chrono::Utc::now();
        let invalid_state = serde_json::json!({
            "version": STATE_VERSION,
            "source": src,
            "destination": dst,
            "started_at": now.to_rfc3339(),
            "checkpoint_at": now.to_rfc3339(),
            "flags": {
                "delete": false,
                "exclude": [],
                "min_size": null,
                "max_size": null
            },
            "completed_files": [
                {
                    "relative_path": "file1.txt",
                    "action": "invalid_action",  // Invalid!
                    "size": 100,
                    "checksum": "xxhash3:abc",
                    "completed_at": now.to_rfc3339()
                }
            ],
            "total_files": 10,
            "total_bytes_transferred": 100
        });
        std::fs::write(&state_path, serde_json::to_string(&invalid_state).unwrap()).unwrap();

        // Load should reject and delete
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_valid_state_passes_integrity_check() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let mut state = ResumeState::new(src, dst, flags, 10);

        state.add_completed_file(
            CompletedFile {
                relative_path: PathBuf::from("file1.txt"),
                action: "create".to_string(),
                size: 100,
                checksum: "xxhash3:abc".to_string(),
                completed_at: format_timestamp(SystemTime::now()),
            },
            100,
        );

        // Save and reload
        state.save(dest).unwrap();
        let loaded = ResumeState::load(dest).unwrap();

        // Should successfully load
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.completed_files.len(), 1);
    }

    // === Additional Edge Case Tests ===

    #[test]
    fn test_corrupted_json_deleted() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();

        // Create corrupted JSON file
        let state_path = dest.join(".sy-state.json");
        std::fs::write(&state_path, "{ corrupted json }}}}").unwrap();

        // Should handle corruption gracefully
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none(), "Corrupted state should be rejected");
        assert!(!state_path.exists(), "Corrupted state should be deleted");
    }

    #[test]
    fn test_empty_state_file() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();

        // Create empty state file
        let state_path = dest.join(".sy-state.json");
        std::fs::write(&state_path, "").unwrap();

        // Should handle empty file gracefully
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none(), "Empty state should be rejected");
        assert!(!state_path.exists(), "Empty state should be deleted");
    }

    #[test]
    fn test_state_with_missing_version() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let state_path = dest.join(".sy-state.json");

        // Create state without version field
        let invalid_state = serde_json::json!({
            "source": src,
            "destination": dst,
            "started_at": format_timestamp(SystemTime::now()),
            "flags": {
                "delete": false,
                "exclude": [],
                "min_size": null,
                "max_size": null
            },
            "completed_files": [],
            "total_files": 0,
            "total_bytes_transferred": 0
        });
        std::fs::write(&state_path, serde_json::to_string(&invalid_state).unwrap()).unwrap();

        // Should fail to deserialize and delete
        let loaded = ResumeState::load(dest).unwrap();
        assert!(loaded.is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn test_flag_change_delete_added() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let original_flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let state = ResumeState::new(src, dst, original_flags.clone(), 10);
        state.save(dest).unwrap();

        // Try to resume with delete flag changed
        let new_flags = SyncFlags {
            delete: true, // Changed!
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let loaded = ResumeState::load(dest).unwrap().unwrap();
        assert!(
            !loaded.is_compatible_with(&new_flags),
            "Should detect delete flag change"
        );
    }

    #[test]
    fn test_flag_change_exclude_added() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let original_flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let state = ResumeState::new(src, dst, original_flags.clone(), 10);
        state.save(dest).unwrap();

        // Try to resume with exclude patterns added
        let new_flags = SyncFlags {
            delete: false,
            exclude: vec!["*.tmp".to_string()], // Added!
            min_size: None,
            max_size: None,
        };

        let loaded = ResumeState::load(dest).unwrap().unwrap();
        assert!(
            !loaded.is_compatible_with(&new_flags),
            "Should detect exclude pattern change"
        );
    }

    #[test]
    fn test_flag_change_size_filters() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let original_flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let state = ResumeState::new(src, dst, original_flags.clone(), 10);
        state.save(dest).unwrap();

        // Try to resume with size filters changed
        let new_flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: Some(1024),     // Added!
            max_size: Some(10485760), // Added!
        };

        let loaded = ResumeState::load(dest).unwrap().unwrap();
        assert!(
            !loaded.is_compatible_with(&new_flags),
            "Should detect size filter change"
        );
    }

    #[test]
    fn test_multiple_resume_cycles() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        // First cycle: save with 3 files
        let mut state = ResumeState::new(src, dst, flags.clone(), 10);

        for i in 0..3 {
            state.add_completed_file(
                CompletedFile {
                    relative_path: PathBuf::from(format!("file{}.txt", i)),
                    action: "create".to_string(),
                    size: 100 * (i + 1) as u64,
                    checksum: format!("xxhash3:abc{}", i),
                    completed_at: format_timestamp(SystemTime::now()),
                },
                100 * (i + 1) as u64,
            );
        }
        state.save(dest).unwrap();

        // Second cycle: load and add more files
        let mut state = ResumeState::load(dest).unwrap().unwrap();
        assert_eq!(state.completed_files.len(), 3);

        for i in 3..6 {
            state.add_completed_file(
                CompletedFile {
                    relative_path: PathBuf::from(format!("file{}.txt", i)),
                    action: "create".to_string(),
                    size: 100 * (i + 1) as u64,
                    checksum: format!("xxhash3:abc{}", i),
                    completed_at: format_timestamp(SystemTime::now()),
                },
                100 * (i + 1) as u64,
            );
        }
        state.save(dest).unwrap();

        // Third cycle: verify all files preserved
        let state = ResumeState::load(dest).unwrap().unwrap();
        assert_eq!(state.completed_files.len(), 6);
        assert_eq!(state.total_bytes_transferred, 2100); // 100+200+300+400+500+600
    }

    #[test]
    fn test_progress_tracking_accuracy() {
        let temp_dir = tempdir().unwrap();
        let _dest = temp_dir.path();

        let flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let mut state = ResumeState::new(
            PathBuf::from("/src"),
            PathBuf::from("/dst"),
            flags,
            100, // Total files
        );

        // Add 30 completed files
        for i in 0..30 {
            state.add_completed_file(
                CompletedFile {
                    relative_path: PathBuf::from(format!("file{}.txt", i)),
                    action: "create".to_string(),
                    size: 1000,
                    checksum: format!("xxhash3:abc{}", i),
                    completed_at: format_timestamp(SystemTime::now()),
                },
                1000,
            );
        }

        let (completed, total) = state.progress();
        assert_eq!(completed, 30);
        assert_eq!(total, 100);
        assert_eq!(state.total_bytes_transferred, 30000);
    }

    #[test]
    fn test_state_with_large_number_of_files() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();
        let (src, dst) = test_absolute_paths();

        let flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let mut state = ResumeState::new(src, dst, flags, 10000);

        // Add 1000 completed files
        for i in 0..1000 {
            state.add_completed_file(
                CompletedFile {
                    relative_path: PathBuf::from(format!("file{:04}.txt", i)),
                    action: "create".to_string(),
                    size: 1024,
                    checksum: format!("xxhash3:hash{}", i),
                    completed_at: format_timestamp(SystemTime::now()),
                },
                1024,
            );
        }

        // Save and reload
        state.save(dest).unwrap();
        let loaded = ResumeState::load(dest).unwrap().unwrap();

        assert_eq!(loaded.completed_files.len(), 1000);
        assert_eq!(loaded.total_bytes_transferred, 1024 * 1000);
    }

    #[test]
    fn test_delete_nonexistent_state() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();

        // Try to delete state that doesn't exist
        let result = ResumeState::delete(dest);

        // Should succeed (idempotent)
        assert!(result.is_ok(), "Deleting nonexistent state should succeed");
    }
}
