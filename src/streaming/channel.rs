//! Channel types for streaming sync pipeline.
//!
//! Three-task pipeline: Generator -> Sender -> Receiver
//! Using bounded channels for backpressure.

use crate::streaming::protocol::BlockChecksum;
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Channel size for Generator -> Sender (file entries)
pub const GENERATOR_CHANNEL_SIZE: usize = 1024;

/// Channel size for Sender -> Receiver (data chunks)
pub const SENDER_CHANNEL_SIZE: usize = 64;

/// Data chunk size for transfer
pub const DATA_CHUNK_SIZE: usize = 256 * 1024; // 256KB

/// Maximum delta chunk size (16MB - well under 64MB frame limit)
pub const DELTA_CHUNK_SIZE: usize = 16 * 1024 * 1024;

/// Minimum file size for delta sync
pub const DELTA_MIN_SIZE: u64 = 64 * 1024; // 64KB

// =============================================================================
// FileJob: Generator -> Sender
// =============================================================================

/// A file job sent from Generator to Sender.
/// Contains all information needed to read and transfer the file.
#[derive(Debug, Clone)]
pub struct FileJob {
    /// Path relative to sync root
    pub path: Arc<PathBuf>,

    /// File size in bytes
    pub size: u64,

    /// Modification time (Unix timestamp)
    pub mtime: i64,

    /// File mode/permissions
    pub mode: u32,

    /// Inode number (for hard link detection)
    pub inode: u64,

    /// Whether this file needs delta transfer
    pub need_delta: bool,

    /// Block checksums from destination (for delta computation)
    /// Only present if need_delta is true and file exists on dest
    pub checksums: Option<DeltaInfo>,
}

/// Delta information from destination file
#[derive(Debug, Clone)]
pub struct DeltaInfo {
    /// Block size used for checksums
    pub block_size: u32,

    /// Destination file size (needed to calculate last block size)
    pub file_size: u64,

    /// Block checksums
    pub checksums: Vec<BlockChecksum>,
}

// =============================================================================
// DataChunk: Sender -> wire
// =============================================================================

/// A chunk of file data ready for transmission.
#[derive(Debug)]
pub struct DataChunk {
    /// Path relative to sync root
    pub path: Arc<PathBuf>,

    /// Offset within file
    pub offset: u64,

    /// Data content
    pub data: Bytes,

    /// Whether this is the final chunk for this file
    pub is_final: bool,

    /// Whether this is delta data
    pub is_delta: bool,

    /// Whether the data is compressed
    pub is_compressed: bool,
}

// =============================================================================
// Pipeline messages
// =============================================================================

/// Message from Generator to the rest of the pipeline
#[derive(Debug)]
pub enum GeneratorMessage {
    /// A file that needs to be transferred
    File(FileJob),

    /// A directory that needs to be created
    Mkdir { path: Arc<PathBuf>, mode: u32 },

    /// A symlink that needs to be created
    Symlink { path: Arc<PathBuf>, target: String },

    /// A file or directory that needs to be deleted
    Delete { path: Arc<PathBuf>, is_dir: bool },

    /// End of file list - no more files coming
    FileEnd { total_files: u64, total_bytes: u64 },

    /// End of deletes
    DeleteEnd { count: u64 },
}

/// Sync direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    /// Local -> Remote (push)
    Push,
    /// Remote -> Local (pull)
    Pull,
}

// =============================================================================
// Channel types
// =============================================================================

/// Sender for file jobs from Generator
pub type FileJobSender = mpsc::Sender<GeneratorMessage>;

/// Receiver for file jobs in Sender task
pub type FileJobReceiver = mpsc::Receiver<GeneratorMessage>;

/// Create a bounded channel for Generator -> Sender communication
pub fn file_job_channel() -> (FileJobSender, FileJobReceiver) {
    mpsc::channel(GENERATOR_CHANNEL_SIZE)
}

// =============================================================================
// Destination state (from Initial Exchange)
// =============================================================================

/// State of a destination file, received during Initial Exchange
#[derive(Debug, Clone)]
pub struct DestFileState {
    /// File size
    pub size: u64,

    /// Modification time
    pub mtime: i64,

    /// File mode
    pub mode: u32,

    /// Whether this is a directory
    pub is_dir: bool,

    /// Block checksums for delta (if file is a delta candidate)
    pub delta_info: Option<DeltaInfo>,
}

/// Destination file index, built during Initial Exchange
#[derive(Debug, Default)]
pub struct DestIndex {
    /// Map of path -> dest state
    files: std::collections::HashMap<String, DestFileState>,
}

impl DestIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: String, state: DestFileState) {
        self.files.insert(path, state);
    }

    pub fn get(&self, path: &str) -> Option<&DestFileState> {
        self.files.get(path)
    }

    pub fn remove(&mut self, path: &str) -> Option<DestFileState> {
        self.files.remove(path)
    }

    pub fn contains(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Get all remaining paths (for delete detection)
    pub fn remaining_paths(&self) -> impl Iterator<Item = (&String, &DestFileState)> {
        self.files.iter()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

// =============================================================================
// Sync statistics
// =============================================================================

/// Statistics for a sync operation
#[derive(Debug, Default, Clone)]
pub struct SyncStats {
    /// Files successfully transferred
    pub files_ok: u64,

    /// Files that failed
    pub files_err: u64,

    /// Total bytes transferred
    pub bytes_transferred: u64,

    /// Files transferred via delta
    pub delta_files: u64,

    /// Bytes saved by delta transfer
    pub delta_bytes_saved: u64,

    /// Directories created
    pub dirs_created: u64,

    /// Symlinks created
    pub symlinks_created: u64,

    /// Files/directories deleted
    pub deleted: u64,

    /// Hard links created
    pub hardlinks_created: u64,
}

impl SyncStats {
    pub fn new() -> Self {
        Self::default()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dest_index() {
        let mut index = DestIndex::new();

        index.insert(
            "file.txt".to_string(),
            DestFileState {
                size: 1024,
                mtime: 1234567890,
                mode: 0o644,
                is_dir: false,
                delta_info: None,
            },
        );

        assert!(index.contains("file.txt"));
        assert!(!index.contains("other.txt"));

        let state = index.get("file.txt").unwrap();
        assert_eq!(state.size, 1024);

        index.remove("file.txt");
        assert!(!index.contains("file.txt"));
    }

    #[tokio::test]
    async fn test_file_job_channel() {
        let (tx, mut rx) = file_job_channel();

        let job = GeneratorMessage::File(FileJob {
            path: Arc::new(PathBuf::from("test.txt")),
            size: 100,
            mtime: 0,
            mode: 0o644,
            inode: 0,
            need_delta: false,
            checksums: None,
        });

        tx.send(job).await.unwrap();
        drop(tx);

        let received = rx.recv().await.unwrap();
        match received {
            GeneratorMessage::File(job) => {
                assert_eq!(job.path.as_ref(), &PathBuf::from("test.txt"));
                assert_eq!(job.size, 100);
            }
            _ => panic!("Expected File message"),
        }
    }

    #[test]
    fn test_sync_stats() {
        let mut stats = SyncStats::new();
        stats.files_ok = 100;
        stats.bytes_transferred = 1024 * 1024;
        stats.delta_files = 10;
        stats.delta_bytes_saved = 512 * 1024;

        assert_eq!(stats.files_ok, 100);
        assert_eq!(stats.delta_files, 10);
    }
}
