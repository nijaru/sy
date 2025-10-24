// Bidirectional sync engine
//
// Orchestrates the complete bidirectional sync workflow

use crate::bisync::{
    classify_changes, conflict_filename, resolve_changes, BisyncStateDb, Change, ChangeType,
    ConflictResolution, ResolvedChanges, Side, SyncAction, SyncState,
};
use crate::error::{Result, SyncError};
use crate::sync::scanner::Scanner;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Options for bidirectional sync
#[derive(Debug, Clone)]
pub struct BisyncOptions {
    pub conflict_resolution: ConflictResolution,
    pub max_delete_percent: u8, // 0-100, 0 = unlimited
    pub dry_run: bool,
    pub clear_state: bool,
}

impl Default for BisyncOptions {
    fn default() -> Self {
        Self {
            conflict_resolution: ConflictResolution::Newer,
            max_delete_percent: 50,
            dry_run: false,
            clear_state: false,
        }
    }
}

/// Statistics from bidirectional sync
#[derive(Debug, Clone, Default)]
pub struct BisyncStats {
    pub files_synced_to_dest: usize,
    pub files_synced_to_source: usize,
    pub files_deleted_from_source: usize,
    pub files_deleted_from_dest: usize,
    pub conflicts_resolved: usize,
    pub conflicts_renamed: usize,
    pub bytes_transferred: u64,
    pub duration_ms: u128,
}

/// Conflict information for reporting
#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub path: PathBuf,
    pub source_mtime: Option<SystemTime>,
    pub source_size: Option<u64>,
    pub dest_mtime: Option<SystemTime>,
    pub dest_size: Option<u64>,
    pub resolution: ConflictResolution,
    pub action: String,
}

/// Result of bidirectional sync
#[derive(Debug)]
pub struct BisyncResult {
    pub stats: BisyncStats,
    pub conflicts: Vec<ConflictInfo>,
    pub errors: Vec<String>,
}

/// Bidirectional sync engine
pub struct BisyncEngine {}

impl BisyncEngine {
    pub fn new() -> Self {
        Self {}
    }

    /// Perform bidirectional sync
    pub fn sync(
        &self,
        source: &Path,
        dest: &Path,
        opts: BisyncOptions,
    ) -> Result<BisyncResult> {
        let start = std::time::Instant::now();

        // 1. Open state database
        let mut state_db = BisyncStateDb::open(source, dest)?;

        if opts.clear_state {
            state_db.clear_all()?;
        }

        // 2. Load prior state
        let prior_state = state_db.load_all()?;

        // 3. Scan both sides
        let source_scanner = Scanner::new(source);
        let dest_scanner = Scanner::new(dest);

        let source_files = source_scanner.scan()?;
        let dest_files = dest_scanner.scan()?;

        // 4. Classify changes
        let changes = classify_changes(&source_files, &dest_files, &prior_state)?;

        // 5. Check deletion limit
        check_deletion_limit(&changes, opts.max_delete_percent)?;

        // 6. Resolve conflicts
        let resolved = resolve_changes(changes.clone(), opts.conflict_resolution)?;

        // 7. Collect conflict info for reporting
        let conflicts = collect_conflict_info(&changes, opts.conflict_resolution);

        // 8. Execute sync actions (or dry run)
        let (stats, errors) = if opts.dry_run {
            // Dry run - just report what would happen
            let stats = simulate_actions(&resolved);
            (stats, Vec::new())
        } else {
            // Actually perform sync
            let (stats, errors) = execute_actions(source, dest, &resolved)?;

            // 9. Update state database
            update_state(&mut state_db, &resolved)?;

            (stats, errors)
        };

        let duration_ms = start.elapsed().as_millis();
        let final_stats = BisyncStats {
            duration_ms,
            ..stats
        };

        Ok(BisyncResult {
            stats: final_stats,
            conflicts,
            errors,
        })
    }
}

impl Default for BisyncEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if deletion limit would be exceeded
fn check_deletion_limit(changes: &[Change], max_delete_percent: u8) -> Result<()> {
    if max_delete_percent == 0 {
        return Ok(()); // Unlimited
    }

    let total_files = changes.len();
    if total_files == 0 {
        return Ok(());
    }

    let deletions = changes
        .iter()
        .filter(|c| {
            matches!(
                c.change_type,
                ChangeType::DeletedFromSource | ChangeType::DeletedFromDest
            )
        })
        .count();

    let deletion_percent = (deletions as f64 / total_files as f64) * 100.0;

    if deletion_percent > max_delete_percent as f64 {
        return Err(SyncError::Config(format!(
            "Deletion limit exceeded: {} deletions ({:.1}%) > {}% limit. \
             Use --max-delete 0 for unlimited or increase threshold.",
            deletions, deletion_percent, max_delete_percent
        )));
    }

    Ok(())
}

/// Collect conflict information for reporting
fn collect_conflict_info(changes: &[Change], strategy: ConflictResolution) -> Vec<ConflictInfo> {
    changes
        .iter()
        .filter(|c| {
            matches!(
                c.change_type,
                ChangeType::ModifiedBoth
                    | ChangeType::CreateCreateConflict
                    | ChangeType::ModifyDeleteConflict
            )
        })
        .map(|c| {
            let action = match &c.change_type {
                ChangeType::ModifiedBoth => "both modified".to_string(),
                ChangeType::CreateCreateConflict => "created on both sides".to_string(),
                ChangeType::ModifyDeleteConflict => "modified vs deleted".to_string(),
                _ => "conflict".to_string(),
            };

            ConflictInfo {
                path: c.path.clone(),
                source_mtime: c.source_entry.as_ref().map(|e| e.modified),
                source_size: c.source_entry.as_ref().map(|e| e.size),
                dest_mtime: c.dest_entry.as_ref().map(|e| e.modified),
                dest_size: c.dest_entry.as_ref().map(|e| e.size),
                resolution: strategy,
                action,
            }
        })
        .collect()
}

/// Simulate actions for dry run
fn simulate_actions(resolved: &ResolvedChanges) -> BisyncStats {
    let mut stats = BisyncStats::default();

    for action in &resolved.actions {
        match action {
            SyncAction::CopyToSource(entry) => {
                stats.files_synced_to_source += 1;
                stats.bytes_transferred += entry.size;
            }
            SyncAction::CopyToDest(entry) => {
                stats.files_synced_to_dest += 1;
                stats.bytes_transferred += entry.size;
            }
            SyncAction::DeleteFromSource(_) => {
                stats.files_deleted_from_source += 1;
            }
            SyncAction::DeleteFromDest(_) => {
                stats.files_deleted_from_dest += 1;
            }
            SyncAction::RenameConflict { source, dest, .. } => {
                stats.files_synced_to_source += 1;
                stats.files_synced_to_dest += 1;
                stats.bytes_transferred += source.size + dest.size;
            }
        }
    }

    stats.conflicts_resolved = resolved.conflicts_resolved;
    stats.conflicts_renamed = resolved.conflicts_renamed;

    stats
}

/// Execute sync actions
fn execute_actions(
    source_root: &Path,
    dest_root: &Path,
    resolved: &ResolvedChanges,
) -> Result<(BisyncStats, Vec<String>)> {
    let mut stats = BisyncStats::default();
    let mut errors = Vec::new();

    for action in &resolved.actions {
        let result = execute_single_action(source_root, dest_root, action);

        match result {
            Ok(bytes) => {
                match action {
                    SyncAction::CopyToSource(_) => stats.files_synced_to_source += 1,
                    SyncAction::CopyToDest(_) => stats.files_synced_to_dest += 1,
                    SyncAction::DeleteFromSource(_) => stats.files_deleted_from_source += 1,
                    SyncAction::DeleteFromDest(_) => stats.files_deleted_from_dest += 1,
                    SyncAction::RenameConflict { .. } => {
                        stats.files_synced_to_source += 1;
                        stats.files_synced_to_dest += 1;
                    }
                }
                stats.bytes_transferred += bytes;
            }
            Err(e) => {
                errors.push(format!("Failed to sync {:?}: {}", action, e));
            }
        }
    }

    stats.conflicts_resolved = resolved.conflicts_resolved;
    stats.conflicts_renamed = resolved.conflicts_renamed;

    Ok((stats, errors))
}

/// Execute a single sync action
fn execute_single_action(
    source_root: &Path,
    dest_root: &Path,
    action: &SyncAction,
) -> Result<u64> {
    match action {
        SyncAction::CopyToSource(entry) => {
            let src = dest_root.join(&entry.relative_path);
            let dst = source_root.join(&entry.relative_path);
            copy_file(&src, &dst)
        }
        SyncAction::CopyToDest(entry) => {
            let src = source_root.join(&entry.relative_path);
            let dst = dest_root.join(&entry.relative_path);
            copy_file(&src, &dst)
        }
        SyncAction::DeleteFromSource(path) => {
            let target = source_root.join(path);
            delete_file(&target)?;
            Ok(0)
        }
        SyncAction::DeleteFromDest(path) => {
            let target = dest_root.join(path);
            delete_file(&target)?;
            Ok(0)
        }
        SyncAction::RenameConflict {
            source,
            dest,
            timestamp,
        } => {
            // Rename both files with conflict suffix
            let source_path = source_root.join(&source.relative_path);
            let dest_path = dest_root.join(&dest.relative_path);

            let source_conflict = conflict_filename(&source_path, timestamp, "source");
            let dest_conflict = conflict_filename(&dest_path, timestamp, "dest");

            std::fs::rename(&source_path, &source_conflict)?;
            std::fs::rename(&dest_path, &dest_conflict)?;

            Ok(0)
        }
    }
}

/// Copy a file (simple implementation, will use transport layer in full version)
fn copy_file(src: &Path, dst: &Path) -> Result<u64> {
    // Create parent directory if needed
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::copy(src, dst).map_err(Into::into)
}

/// Delete a file
fn delete_file(path: &Path) -> Result<()> {
    std::fs::remove_file(path).map_err(Into::into)
}

/// Update state database after sync
fn update_state(
    state_db: &mut BisyncStateDb,
    resolved: &ResolvedChanges,
) -> Result<()> {
    let now = SystemTime::now();

    for action in &resolved.actions {
        match action {
            SyncAction::CopyToSource(entry) => {
                // File now exists on both sides with same content
                let state = SyncState {
                    path: entry.relative_path.clone(),
                    side: Side::Source,
                    mtime: entry.modified,
                    size: entry.size,
                    checksum: None,
                    last_sync: now,
                };
                state_db.store(&state)?;
            }
            SyncAction::CopyToDest(entry) => {
                let state = SyncState {
                    path: entry.relative_path.clone(),
                    side: Side::Dest,
                    mtime: entry.modified,
                    size: entry.size,
                    checksum: None,
                    last_sync: now,
                };
                state_db.store(&state)?;
            }
            SyncAction::DeleteFromSource(path) => {
                state_db.delete(path)?;
            }
            SyncAction::DeleteFromDest(path) => {
                state_db.delete(path)?;
            }
            SyncAction::RenameConflict { source, dest, .. } => {
                // Both files kept with new names - update state
                let source_state = SyncState {
                    path: source.relative_path.clone(),
                    side: Side::Source,
                    mtime: source.modified,
                    size: source.size,
                    checksum: None,
                    last_sync: now,
                };
                state_db.store(&source_state)?;

                let dest_state = SyncState {
                    path: dest.relative_path.clone(),
                    side: Side::Dest,
                    mtime: dest.modified,
                    size: dest.size,
                    checksum: None,
                    last_sync: now,
                };
                state_db.store(&dest_state)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_deletion_limit_ok() {
        let changes = vec![
            Change {
                path: PathBuf::from("file1.txt"),
                change_type: ChangeType::NewInSource,
                source_entry: None,
                dest_entry: None,
            },
            Change {
                path: PathBuf::from("file2.txt"),
                change_type: ChangeType::DeletedFromSource,
                source_entry: None,
                dest_entry: None,
            },
        ];

        // 1 deletion out of 2 files = 50%
        assert!(check_deletion_limit(&changes, 50).is_ok());
    }

    #[test]
    fn test_check_deletion_limit_exceeded() {
        let changes = vec![
            Change {
                path: PathBuf::from("file1.txt"),
                change_type: ChangeType::DeletedFromSource,
                source_entry: None,
                dest_entry: None,
            },
            Change {
                path: PathBuf::from("file2.txt"),
                change_type: ChangeType::DeletedFromDest,
                source_entry: None,
                dest_entry: None,
            },
        ];

        // 2 deletions out of 2 files = 100% > 50% limit
        assert!(check_deletion_limit(&changes, 50).is_err());
    }

    #[test]
    fn test_check_deletion_limit_unlimited() {
        let changes = vec![
            Change {
                path: PathBuf::from("file1.txt"),
                change_type: ChangeType::DeletedFromSource,
                source_entry: None,
                dest_entry: None,
            },
        ];

        // max_delete_percent = 0 means unlimited
        assert!(check_deletion_limit(&changes, 0).is_ok());
    }
}
