use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    pub files_scanned: u64,
    pub files_created: u64,
    pub files_updated: u64,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub bytes_transferred: u64,
    pub files_delta_synced: usize,
    pub delta_bytes_saved: u64,
    pub files_compressed: usize,
    pub compression_bytes_saved: u64,
    pub files_verified: usize,
    pub verification_failures: usize,
    pub duration: Duration,
    pub bytes_would_add: u64,
    pub bytes_would_change: u64,
    pub bytes_would_delete: u64,
    #[allow(dead_code)]
    pub dirs_created: u64,
    #[allow(dead_code)]
    pub symlinks_created: u64,
    pub errors: Vec<SyncError>,
}

impl SyncStats {
    #[allow(dead_code)]
    pub fn new(scanned: u64) -> Self {
        Self {
            files_scanned: scanned,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncError {
    pub path: PathBuf,
    pub error: String,
    pub action: String,
}

#[derive(Debug)]
pub struct VerificationResult {
    pub files_matched: usize,
    pub files_mismatched: Vec<PathBuf>,
    pub files_only_in_source: Vec<PathBuf>,
    pub files_only_in_dest: Vec<PathBuf>,
    pub errors: Vec<SyncError>,
    pub duration: Duration,
}
