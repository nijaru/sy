use crate::error::{Result, SyncError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Default chunk size for resume transfers (1 MB)
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Transfer state for resume capability
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferState {
    /// Source file path
    pub source_path: PathBuf,
    /// Destination file path
    pub dest_path: PathBuf,
    /// Total file size in bytes
    pub total_size: u64,
    /// Bytes successfully transferred so far
    pub bytes_transferred: u64,
    /// Chunk size used for this transfer
    pub chunk_size: usize,
    /// File modification time (for staleness detection)
    pub mtime: SystemTime,
    /// Optional checksum for verification
    pub checksum: Option<String>,
    /// Last update timestamp
    pub last_updated: SystemTime,
}

impl TransferState {
    /// Create a new transfer state
    pub fn new(
        source: &Path,
        dest: &Path,
        total_size: u64,
        mtime: SystemTime,
        chunk_size: usize,
    ) -> Self {
        Self {
            source_path: source.to_path_buf(),
            dest_path: dest.to_path_buf(),
            total_size,
            bytes_transferred: 0,
            chunk_size,
            mtime,
            checksum: None,
            last_updated: SystemTime::now(),
        }
    }

    /// Check if transfer is complete
    pub fn is_complete(&self) -> bool {
        self.bytes_transferred >= self.total_size
    }

    /// Get progress as percentage (0.0 to 100.0)
    pub fn progress_percentage(&self) -> f64 {
        if self.total_size == 0 {
            100.0
        } else {
            (self.bytes_transferred as f64 / self.total_size as f64) * 100.0
        }
    }

    /// Update bytes transferred
    pub fn update_progress(&mut self, bytes: u64) {
        self.bytes_transferred = bytes;
        self.last_updated = SystemTime::now();
    }

    /// Check if state is stale (file modified since state created)
    pub fn is_stale(&self, current_mtime: SystemTime) -> bool {
        current_mtime != self.mtime
    }

    /// Generate unique state ID from source, dest, and mtime
    fn generate_state_id(source: &Path, dest: &Path, mtime: SystemTime) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(source.to_string_lossy().as_bytes());
        hasher.update(dest.to_string_lossy().as_bytes());

        // Convert SystemTime to duration since UNIX_EPOCH for hashing
        if let Ok(duration) = mtime.duration_since(SystemTime::UNIX_EPOCH) {
            hasher.update(&duration.as_secs().to_le_bytes());
            hasher.update(&duration.subsec_nanos().to_le_bytes());
        }

        hasher.finalize().to_hex().to_string()
    }

    /// Get resume state directory (~/.cache/sy/resume/)
    fn get_resume_dir() -> Result<PathBuf> {
        let cache_dir = if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
            PathBuf::from(xdg_cache)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".cache")
        } else {
            return Err(SyncError::Config(
                "Cannot determine cache directory (HOME not set)".to_string(),
            ));
        };

        let resume_dir = cache_dir.join("sy").join("resume");
        fs::create_dir_all(&resume_dir)?;
        Ok(resume_dir)
    }

    /// Get state file path for this transfer
    fn get_state_file_path(source: &Path, dest: &Path, mtime: SystemTime) -> Result<PathBuf> {
        let state_id = Self::generate_state_id(source, dest, mtime);
        let resume_dir = Self::get_resume_dir()?;
        Ok(resume_dir.join(format!("{}.json", state_id)))
    }

    /// Save state to disk
    pub fn save(&self) -> Result<()> {
        let state_file = Self::get_state_file_path(&self.source_path, &self.dest_path, self.mtime)?;

        // Atomic write: write to temp file, then rename
        let temp_file = state_file.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| SyncError::Config(format!("Failed to serialize resume state: {}", e)))?;

        fs::write(&temp_file, json)?;
        fs::rename(temp_file, state_file)?;

        Ok(())
    }

    /// Load state from disk if it exists
    pub fn load(source: &Path, dest: &Path, mtime: SystemTime) -> Result<Option<Self>> {
        let state_file = Self::get_state_file_path(source, dest, mtime)?;

        if !state_file.exists() {
            return Ok(None);
        }

        let json = fs::read_to_string(&state_file)?;
        let state: Self = serde_json::from_str(&json).map_err(|e| {
            SyncError::Config(format!("Failed to parse resume state: {}", e))
        })?;

        // Verify state matches current transfer
        if state.source_path != source || state.dest_path != dest {
            eprintln!(
                "Warning: Resume state mismatch (expected {:?} -> {:?}, got {:?} -> {:?}). Ignoring.",
                source, dest, state.source_path, state.dest_path
            );
            return Ok(None);
        }

        // Check if stale
        if state.is_stale(mtime) {
            eprintln!(
                "Warning: Resume state is stale (file modified). Starting fresh transfer."
            );
            Self::clear(source, dest, mtime)?;
            return Ok(None);
        }

        Ok(Some(state))
    }

    /// Clear resume state for a specific transfer
    pub fn clear(source: &Path, dest: &Path, mtime: SystemTime) -> Result<()> {
        let state_file = Self::get_state_file_path(source, dest, mtime)?;

        if state_file.exists() {
            fs::remove_file(state_file)?;
        }

        Ok(())
    }

    /// Clear all resume states
    pub fn clear_all() -> Result<()> {
        let resume_dir = Self::get_resume_dir()?;

        if resume_dir.exists() {
            for entry in fs::read_dir(&resume_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
                    fs::remove_file(path)?;
                }
            }
        }

        Ok(())
    }

    /// Get number of chunks needed for this transfer
    pub fn total_chunks(&self) -> usize {
        ((self.total_size + self.chunk_size as u64 - 1) / self.chunk_size as u64) as usize
    }

    /// Get the next chunk to transfer (start_offset, length)
    pub fn next_chunk(&self) -> Option<(u64, usize)> {
        if self.is_complete() {
            return None;
        }

        let start_offset = self.bytes_transferred;
        let remaining = self.total_size - self.bytes_transferred;
        let chunk_len = std::cmp::min(remaining, self.chunk_size as u64) as usize;

        Some((start_offset, chunk_len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_new_transfer_state() {
        let source = PathBuf::from("/tmp/source.txt");
        let dest = PathBuf::from("/tmp/dest.txt");
        let mtime = SystemTime::now();
        let state = TransferState::new(&source, &dest, 1024 * 1024, mtime, DEFAULT_CHUNK_SIZE);

        assert_eq!(state.source_path, source);
        assert_eq!(state.dest_path, dest);
        assert_eq!(state.total_size, 1024 * 1024);
        assert_eq!(state.bytes_transferred, 0);
        assert_eq!(state.chunk_size, DEFAULT_CHUNK_SIZE);
        assert!(!state.is_complete());
    }

    #[test]
    fn test_progress_percentage() {
        let source = PathBuf::from("/tmp/source.txt");
        let dest = PathBuf::from("/tmp/dest.txt");
        let mtime = SystemTime::now();
        let mut state = TransferState::new(&source, &dest, 1000, mtime, DEFAULT_CHUNK_SIZE);

        assert_eq!(state.progress_percentage(), 0.0);

        state.update_progress(500);
        assert_eq!(state.progress_percentage(), 50.0);

        state.update_progress(1000);
        assert_eq!(state.progress_percentage(), 100.0);
        assert!(state.is_complete());
    }

    #[test]
    fn test_progress_percentage_zero_size() {
        let source = PathBuf::from("/tmp/source.txt");
        let dest = PathBuf::from("/tmp/dest.txt");
        let mtime = SystemTime::now();
        let state = TransferState::new(&source, &dest, 0, mtime, DEFAULT_CHUNK_SIZE);

        assert_eq!(state.progress_percentage(), 100.0);
        assert!(state.is_complete());
    }

    #[test]
    fn test_is_stale() {
        let source = PathBuf::from("/tmp/source.txt");
        let dest = PathBuf::from("/tmp/dest.txt");
        let mtime = SystemTime::now();
        let state = TransferState::new(&source, &dest, 1000, mtime, DEFAULT_CHUNK_SIZE);

        assert!(!state.is_stale(mtime));

        let new_mtime = mtime + Duration::from_secs(10);
        assert!(state.is_stale(new_mtime));
    }

    #[test]
    fn test_total_chunks() {
        let source = PathBuf::from("/tmp/source.txt");
        let dest = PathBuf::from("/tmp/dest.txt");
        let mtime = SystemTime::now();

        // Exactly 1 chunk
        let state1 = TransferState::new(&source, &dest, 1024 * 1024, mtime, DEFAULT_CHUNK_SIZE);
        assert_eq!(state1.total_chunks(), 1);

        // 1.5 chunks -> rounds up to 2
        let state2 = TransferState::new(
            &source,
            &dest,
            1024 * 1024 + 512 * 1024,
            mtime,
            DEFAULT_CHUNK_SIZE,
        );
        assert_eq!(state2.total_chunks(), 2);

        // 10 chunks
        let state3 = TransferState::new(
            &source,
            &dest,
            10 * 1024 * 1024,
            mtime,
            DEFAULT_CHUNK_SIZE,
        );
        assert_eq!(state3.total_chunks(), 10);
    }

    #[test]
    fn test_next_chunk() {
        let source = PathBuf::from("/tmp/source.txt");
        let dest = PathBuf::from("/tmp/dest.txt");
        let mtime = SystemTime::now();
        let chunk_size = 1024;
        let mut state = TransferState::new(&source, &dest, 2500, mtime, chunk_size);

        // First chunk
        let (offset, len) = state.next_chunk().unwrap();
        assert_eq!(offset, 0);
        assert_eq!(len, 1024);

        // Second chunk
        state.update_progress(1024);
        let (offset, len) = state.next_chunk().unwrap();
        assert_eq!(offset, 1024);
        assert_eq!(len, 1024);

        // Third chunk (partial)
        state.update_progress(2048);
        let (offset, len) = state.next_chunk().unwrap();
        assert_eq!(offset, 2048);
        assert_eq!(len, 452); // Remaining bytes

        // Complete
        state.update_progress(2500);
        assert!(state.next_chunk().is_none());
    }

    #[test]
    fn test_save_and_load() -> Result<()> {
        let source = PathBuf::from("/tmp/test_source.txt");
        let dest = PathBuf::from("/tmp/test_dest.txt");
        let mtime = SystemTime::now();
        let mut state = TransferState::new(&source, &dest, 5000, mtime, DEFAULT_CHUNK_SIZE);
        state.update_progress(2500);
        state.checksum = Some("abc123".to_string());

        // Save
        state.save()?;

        // Load
        let loaded = TransferState::load(&source, &dest, mtime)?.unwrap();
        assert_eq!(loaded.source_path, source);
        assert_eq!(loaded.dest_path, dest);
        assert_eq!(loaded.total_size, 5000);
        assert_eq!(loaded.bytes_transferred, 2500);
        assert_eq!(loaded.checksum, Some("abc123".to_string()));

        // Clear
        TransferState::clear(&source, &dest, mtime)?;
        assert!(TransferState::load(&source, &dest, mtime)?.is_none());

        Ok(())
    }

    #[test]
    fn test_load_stale_state() -> Result<()> {
        let source = PathBuf::from("/tmp/test_stale_source.txt");
        let dest = PathBuf::from("/tmp/test_stale_dest.txt");
        let old_mtime = SystemTime::now() - Duration::from_secs(3600);
        let state = TransferState::new(&source, &dest, 1000, old_mtime, DEFAULT_CHUNK_SIZE);

        // Save with old mtime
        state.save()?;

        // Try to load with new mtime (simulates file modification)
        let new_mtime = SystemTime::now();
        let loaded = TransferState::load(&source, &dest, new_mtime)?;
        assert!(loaded.is_none()); // Should be rejected as stale

        Ok(())
    }

    #[test]
    fn test_clear_all() -> Result<()> {
        let source1 = PathBuf::from("/tmp/test_clear_source1.txt");
        let dest1 = PathBuf::from("/tmp/test_clear_dest1.txt");
        let source2 = PathBuf::from("/tmp/test_clear_source2.txt");
        let dest2 = PathBuf::from("/tmp/test_clear_dest2.txt");
        let mtime = SystemTime::now();

        // Create two states
        let state1 = TransferState::new(&source1, &dest1, 1000, mtime, DEFAULT_CHUNK_SIZE);
        let state2 = TransferState::new(&source2, &dest2, 2000, mtime, DEFAULT_CHUNK_SIZE);
        state1.save()?;
        state2.save()?;

        // Clear all
        TransferState::clear_all()?;

        // Both should be gone
        assert!(TransferState::load(&source1, &dest1, mtime)?.is_none());
        assert!(TransferState::load(&source2, &dest2, mtime)?.is_none());

        Ok(())
    }

    #[test]
    fn test_generate_state_id_uniqueness() {
        let source1 = PathBuf::from("/tmp/source1.txt");
        let source2 = PathBuf::from("/tmp/source2.txt");
        let dest = PathBuf::from("/tmp/dest.txt");
        let mtime1 = SystemTime::now();
        let mtime2 = mtime1 + Duration::from_secs(1);

        // Different sources
        let id1 = TransferState::generate_state_id(&source1, &dest, mtime1);
        let id2 = TransferState::generate_state_id(&source2, &dest, mtime1);
        assert_ne!(id1, id2);

        // Different mtimes
        let id3 = TransferState::generate_state_id(&source1, &dest, mtime1);
        let id4 = TransferState::generate_state_id(&source1, &dest, mtime2);
        assert_ne!(id3, id4);

        // Same params should produce same ID
        let id5 = TransferState::generate_state_id(&source1, &dest, mtime1);
        assert_eq!(id1, id5);
    }
}
