//! Unified per-operation output for the sync engines.
//!
//! One reporter serves `--itemize`, `--json`, and `--perf` on local sync and
//! v3 remote push. The reporter owns quiet gating: `--quiet` prints no
//! per-operation lines, and JSON mode implies quiet for human surfaces so
//! stdout belongs to NDJSON events alone. Itemize lines go to stderr in the
//! rsync shape (`<f<`, `<f>`, `*deleting`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

/// What one completed operation did to the destination namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemizeOp {
    Create,
    Update,
    Delete,
}

impl ItemizeOp {
    /// Map the legacy `SyncAction` onto a reporter op for the live local
    /// executor. `Skip` is reported by the reconcile layer, not here.
    pub fn for_action(action: &crate::sync::strategy::SyncAction) -> Self {
        use crate::sync::strategy::SyncAction;
        match action {
            SyncAction::Create => ItemizeOp::Create,
            SyncAction::Update => ItemizeOp::Update,
            SyncAction::Delete => ItemizeOp::Delete,
            SyncAction::Skip => ItemizeOp::Create,
        }
    }
}

/// Entry kind as the itemize type column reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemizeKind {
    File,
    Directory,
    Symlink,
}

impl ItemizeKind {
    fn marker(self) -> char {
        match self {
            ItemizeKind::File => 'f',
            ItemizeKind::Directory => 'd',
            ItemizeKind::Symlink => 'L',
        }
    }
}

/// rsync-shaped 11-character itemize field.
///
/// Position 1 is the transfer direction: `>` the destination is receiving
/// this entry, `*` a deletion (rendered as `*deleting` by `operation`).
/// Position 2 is the kind. The remaining attribute positions use `+` for
/// attributes a create establishes; updates print `.` where attribute
/// tracking is not yet wired (never a fake `+`).
fn itemize_field(op: ItemizeOp, kind: ItemizeKind) -> String {
    let (state, filler) = match op {
        ItemizeOp::Create => ('>', "+"),
        ItemizeOp::Update => ('>', "."),
        ItemizeOp::Delete => ('*', "+"),
    };
    let mut field = format!("{state}{}", kind.marker());
    field.push_str(&filler.repeat(9));
    field
}

/// Machine-readable sync events (NDJSON on stdout).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncEvent {
    /// Emitted once when a run has established both roots. Deliberately no
    /// total-files field: local sync scans in bounded batches and never knows
    /// the tree size up front; `Summary` carries final counts.
    Start {
        source: PathBuf,
        destination: PathBuf,
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
    /// Final event after all work, including deletion replay and directory
    /// finalization, has finished.
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
    /// `--verify=only` result mode.
    VerificationResult {
        files_matched: usize,
        files_mismatched: Vec<PathBuf>,
        files_only_in_source: Vec<PathBuf>,
        files_only_in_dest: Vec<PathBuf>,
        errors: Vec<VerificationError>,
        duration_secs: f64,
        exit_code: i32,
    },
    /// `--perf` wall-clock breakdown. Engines report what they measured;
    /// phases an engine does not separate are zero, never guessed.
    Performance {
        total_duration_secs: f64,
        scan_duration_secs: f64,
        transfer_duration_secs: f64,
        bytes_transferred: u64,
        files_created: u64,
        files_updated: u64,
        files_deleted: u64,
        avg_transfer_speed: f64,
    },
}

#[derive(Debug, Serialize)]
pub struct VerificationError {
    pub path: PathBuf,
    pub error: String,
    pub action: String,
}

impl SyncEvent {
    /// Emit this event as NDJSON on stdout.
    pub fn emit(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            println!("{json}");
        }
    }
}

/// Wall-clock split a run reports under `--perf`. `transfer` is the mutation
/// phase: main work, deletion replay, and directory finalization.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncTimings {
    pub scan: Duration,
    pub transfer: Duration,
}

/// Final counters for `finish`. Primitive-typed so the reporter compiles
/// identically from both module-tree roots.
#[derive(Debug, Clone, Copy, Default)]
pub struct SummaryCounts {
    pub files_created: u64,
    pub files_updated: u64,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub bytes_transferred: u64,
    pub duration_secs: f64,
    pub files_verified: u64,
    pub verification_failures: usize,
}

/// Output surface shared by the local and v3 remote engines.
///
/// Built from user flags by each run; all human per-operation output goes to
/// stderr so JSON owns stdout.
pub struct SyncReporter {
    itemize: bool,
    json: bool,
    quiet: bool,
    perf: bool,
}

impl SyncReporter {
    pub fn new(itemize: bool, json: bool, quiet: bool, perf: bool) -> Self {
        // JSON owns stdout: suppress every human surface, including itemize.
        Self {
            itemize,
            json,
            quiet: quiet || json,
            perf,
        }
    }

    /// Whether NDJSON events are emitted.
    pub fn json_enabled(&self) -> bool {
        self.json
    }

    /// Whether the human perf block is printed at the end.
    pub fn perf_enabled(&self) -> bool {
        self.perf
    }

    pub fn operation(&self, op: ItemizeOp, kind: ItemizeKind, path: &Path) {
        if !self.itemize || self.quiet {
            return;
        }
        // rsync renders deletions as a bare `*deleting` field.
        if op == ItemizeOp::Delete {
            eprintln!("*deleting   {path}", path = path.display());
        } else {
            eprintln!("{} {}", itemize_field(op, kind), path.display());
        }
    }

    pub fn start(&self, source: &Path, destination: &Path) {
        if self.json {
            SyncEvent::Start {
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
            }
            .emit();
        }
    }

    pub fn created(&self, kind: ItemizeKind, path: &Path, size: u64, bytes: u64) {
        self.operation(ItemizeOp::Create, kind, path);
        if self.json {
            SyncEvent::Create {
                path: path.to_path_buf(),
                size,
                bytes_transferred: bytes,
            }
            .emit();
        }
    }

    pub fn updated(&self, path: &Path, size: u64, bytes: u64, delta_used: bool) {
        self.operation(ItemizeOp::Update, ItemizeKind::File, path);
        if self.json {
            SyncEvent::Update {
                path: path.to_path_buf(),
                size,
                bytes_transferred: bytes,
                delta_used,
            }
            .emit();
        }
    }

    /// A planned skip: source unchanged, destination untouched. Itemize
    /// renders the rsync no-change field (`.`).
    pub fn skipped(&self, path: &Path, reason: &str) {
        if !self.itemize || self.quiet {
            return;
        }
        eprintln!(".f........... {path}", path = path.display());
        if self.json {
            SyncEvent::Skip {
                path: path.to_path_buf(),
                reason: reason.to_string(),
            }
            .emit();
        }
    }

    pub fn deleted(&self, kind: ItemizeKind, path: &Path) {
        self.operation(ItemizeOp::Delete, kind, path);
        if self.json {
            SyncEvent::Delete {
                path: path.to_path_buf(),
            }
            .emit();
        }
    }

    pub fn failed(&self, path: &Path, error: &str) {
        if self.json {
            SyncEvent::Error {
                path: path.to_path_buf(),
                error: error.to_string(),
            }
            .emit();
        }
    }

    /// Terminal output for a completed run: NDJSON summary, the `--perf`
    /// event, and the human perf block (stderr, quiet-gated).
    ///
    /// Takes a `SummaryCounts` of primitives so callers from either compile of
    /// the module tree share this reporter without a cross-crate type
    /// dependency.
    pub fn finish(&self, counts: &SummaryCounts, timings: SyncTimings) {
        if self.json {
            SyncEvent::Summary {
                files_created: counts.files_created as usize,
                files_updated: counts.files_updated as usize,
                files_skipped: counts.files_skipped,
                files_deleted: counts.files_deleted,
                bytes_transferred: counts.bytes_transferred,
                duration_secs: counts.duration_secs,
                files_verified: counts.files_verified as usize,
                verification_failures: counts.verification_failures,
            }
            .emit();
        }
        if !self.perf {
            return;
        }
        let total_secs = counts.duration_secs;
        let avg = if total_secs > 0.0 {
            counts.bytes_transferred as f64 / total_secs
        } else {
            0.0
        };
        if self.json {
            SyncEvent::Performance {
                total_duration_secs: total_secs,
                scan_duration_secs: timings.scan.as_secs_f64(),
                transfer_duration_secs: timings.transfer.as_secs_f64(),
                bytes_transferred: counts.bytes_transferred,
                files_created: counts.files_created,
                files_updated: counts.files_updated,
                files_deleted: counts.files_deleted as u64,
                avg_transfer_speed: avg,
            }
            .emit();
        } else if !self.quiet {
            eprintln!(
                "Performance: total {total_secs:.3}s, scan {:.3}s, transfer {:.3}s, avg {avg}/s",
                timings.scan.as_secs_f64(),
                timings.transfer.as_secs_f64(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn itemize_field_matches_rsync_shape() {
        // Measured against live rsync (-ai): creates and updates are
        // receiving-direction `>` entries; kind char then nine `+` (create)
        // or `.` (update, attrs not yet tracked) positions.
        assert_eq!(
            itemize_field(ItemizeOp::Create, ItemizeKind::File),
            ">f+++++++++"
        );
        assert_eq!(
            itemize_field(ItemizeOp::Update, ItemizeKind::File),
            ">f........."
        );
        assert_eq!(
            itemize_field(ItemizeOp::Create, ItemizeKind::Directory),
            ">d+++++++++"
        );
        assert_eq!(
            itemize_field(ItemizeOp::Create, ItemizeKind::Symlink),
            ">L+++++++++"
        );
    }

    #[test]
    fn json_implies_quiet() {
        let reporter = SyncReporter::new(true, true, false, false);
        assert!(reporter.json_enabled());
        // itemize+json together: events only, no human lines.
        let plain = SyncReporter::new(true, false, false, false);
        assert!(!plain.quiet_impl_for_test());
        let json = SyncReporter::new(true, true, false, false);
        assert!(json.quiet_impl_for_test());
    }

    impl SyncReporter {
        fn quiet_impl_for_test(&self) -> bool {
            self.quiet
        }
    }

    #[test]
    fn quiet_suppresses_itemize() {
        let reporter = SyncReporter::new(true, false, true, false);
        assert!(reporter.quiet_impl_for_test());
    }

    #[test]
    fn start_event_has_no_total_files() {
        let event = SyncEvent::Start {
            source: PathBuf::from("/src"),
            destination: PathBuf::from("/dst"),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"start""#));
        assert!(!json.contains("total_files"));
    }

    #[test]
    fn create_and_delete_events_serialize() {
        let create = SyncEvent::Create {
            path: PathBuf::from("f.txt"),
            size: 10,
            bytes_transferred: 10,
        };
        let json = serde_json::to_string(&create).unwrap();
        assert!(json.contains(r#""type":"create""#));
        assert!(json.contains(r#""bytes_transferred":10"#));

        let delete = SyncEvent::Delete {
            path: PathBuf::from("f.txt"),
        };
        assert!(serde_json::to_string(&delete)
            .unwrap()
            .contains(r#""type":"delete""#));
    }

    #[test]
    fn update_event_serializes_delta_flag() {
        let event = SyncEvent::Update {
            path: PathBuf::from("f.txt"),
            size: 20,
            bytes_transferred: 5,
            delta_used: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""delta_used":true"#));
    }

    #[test]
    fn summary_event_serializes() {
        let event = SyncEvent::Summary {
            files_created: 3,
            files_updated: 2,
            files_skipped: 4,
            files_deleted: 1,
            bytes_transferred: 100,
            duration_secs: 1.5,
            files_verified: 5,
            verification_failures: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"summary""#));
        assert!(json.contains(r#""files_deleted":1"#));
    }

    #[test]
    fn performance_event_serializes() {
        let event = SyncEvent::Performance {
            total_duration_secs: 2.0,
            scan_duration_secs: 0.5,
            transfer_duration_secs: 1.5,
            bytes_transferred: 1_000,
            files_created: 1,
            files_updated: 1,
            files_deleted: 0,
            avg_transfer_speed: 500.0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"performance""#));
        assert!(json.contains(r#""scan_duration_secs":0.5"#));
    }

    #[test]
    fn verification_result_event_keeps_shape() {
        let event = SyncEvent::VerificationResult {
            files_matched: 1,
            files_mismatched: vec![],
            files_only_in_source: vec![],
            files_only_in_dest: vec![],
            errors: vec![],
            duration_secs: 1.0,
            exit_code: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"verification_result""#));
    }
}
