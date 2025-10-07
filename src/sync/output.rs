use serde::Serialize;
use std::path::PathBuf;

/// JSON output mode for machine-readable sync events
/// Uses NDJSON format (newline-delimited JSON)
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncEvent {
    Start {
        source: PathBuf,
        destination: PathBuf,
        total_files: usize,
    },
    Create {
        path: PathBuf,
        size: u64,
        bytes_transferred: u64,
    },
    Update {
        path: PathBuf,
        size: u64,
        bytes_transferred: u64,
        delta_used: bool,
    },
    Skip {
        path: PathBuf,
        reason: String,
    },
    Delete {
        path: PathBuf,
    },
    Error {
        path: PathBuf,
        error: String,
    },
    Summary {
        files_created: usize,
        files_updated: usize,
        files_skipped: usize,
        files_deleted: usize,
        bytes_transferred: u64,
        duration_secs: f64,
        files_verified: usize,
        verification_failures: usize,
    },
}

impl SyncEvent {
    /// Emit this event as JSON to stdout
    pub fn emit(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            println!("{}", json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_start_event() {
        let event = SyncEvent::Start {
            source: PathBuf::from("/src"),
            destination: PathBuf::from("/dst"),
            total_files: 100,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"start"#));
        assert!(json.contains(r#""total_files":100"#));
    }

    #[test]
    fn test_serialize_create_event() {
        let event = SyncEvent::Create {
            path: PathBuf::from("file.txt"),
            size: 1234,
            bytes_transferred: 1234,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"create"#));
        assert!(json.contains(r#""size":1234"#));
    }

    #[test]
    fn test_serialize_update_event() {
        let event = SyncEvent::Update {
            path: PathBuf::from("file.txt"),
            size: 5678,
            bytes_transferred: 234,
            delta_used: true,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"update"#));
        assert!(json.contains(r#""delta_used":true"#));
    }

    #[test]
    fn test_serialize_summary_event() {
        let event = SyncEvent::Summary {
            files_created: 10,
            files_updated: 5,
            files_skipped: 20,
            files_deleted: 2,
            bytes_transferred: 123456,
            duration_secs: 12.5,
            files_verified: 15,
            verification_failures: 0,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"summary"#));
        assert!(json.contains(r#""files_created":10"#));
        assert!(json.contains(r#""duration_secs":12.5"#));
        assert!(json.contains(r#""files_verified":15"#));
        assert!(json.contains(r#""verification_failures":0"#));
    }
}
