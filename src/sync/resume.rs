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
        let state: Self = serde_json::from_reader(reader).map_err(|e| {
            tracing::warn!("Failed to parse state file: {}", e);
            SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Corrupted state file: {}", e),
            ))
        })?;

        // Version check
        if state.version != STATE_VERSION {
            tracing::warn!(
                "State file version mismatch: expected {}, got {}",
                STATE_VERSION,
                state.version
            );
            return Ok(None);
        }

        Ok(Some(state))
    }

    /// Save resume state to destination directory (atomic)
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
            SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to write state file: {}", e),
            ))
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
    pub fn add_completed_file(&mut self, file: CompletedFile, bytes_transferred: u64) {
        self.completed_files.push(file);
        self.total_bytes_transferred += bytes_transferred;
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
fn format_timestamp(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_resume_state_save_load() {
        let temp_dir = tempdir().unwrap();
        let dest = temp_dir.path();

        let flags = SyncFlags {
            delete: true,
            exclude: vec!["*.log".to_string()],
            min_size: Some(1024),
            max_size: None,
        };

        let mut state = ResumeState::new(
            PathBuf::from("/src"),
            PathBuf::from("/dst"),
            flags.clone(),
            100,
        );

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

        let flags = SyncFlags {
            delete: false,
            exclude: Vec::new(),
            min_size: None,
            max_size: None,
        };

        let state = ResumeState::new(
            PathBuf::from("/src"),
            PathBuf::from("/dst"),
            flags,
            10,
        );

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

        let mut state = ResumeState::new(
            PathBuf::from("/src"),
            PathBuf::from("/dst"),
            flags,
            10,
        );

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
}
