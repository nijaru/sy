// Conflict resolution for bidirectional sync

use crate::bisync::classifier::{Change, ChangeType};
use crate::error::Result;
use crate::sync::scanner::FileEntry;
use std::path::{Path, PathBuf};

/// Conflict resolution strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    Newer,   // Winner = most recent mtime (DEFAULT)
    Larger,  // Winner = largest size
    Smaller, // Winner = smallest size
    Source,  // Winner = source (force push)
    Dest,    // Winner = dest (force pull)
    Rename,  // Keep both: file.conflict-<timestamp>-<side>
}

impl ConflictResolution {
    #[allow(clippy::should_implement_trait)] // Named to match other from_str methods in codebase
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "newer" => Some(Self::Newer),
            "larger" => Some(Self::Larger),
            "smaller" => Some(Self::Smaller),
            "source" => Some(Self::Source),
            "dest" => Some(Self::Dest),
            "rename" => Some(Self::Rename),
            _ => None,
        }
    }
}

/// Resolution action to take
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // RenameConflict is intentionally larger, used rarely
pub enum SyncAction {
    CopyToSource(FileEntry),   // Copy dest → source
    CopyToDest(FileEntry),     // Copy source → dest
    DeleteFromSource(PathBuf), // Delete file from source
    DeleteFromDest(PathBuf),   // Delete file from dest
    RenameConflict {
        source: FileEntry,
        dest: FileEntry,
        timestamp: String,
    },
}

/// Result of conflict resolution
#[derive(Debug)]
pub struct ResolvedChanges {
    pub actions: Vec<SyncAction>,
    pub conflicts_resolved: usize,
    pub conflicts_renamed: usize,
}

/// Resolve all changes according to strategy
pub fn resolve_changes(
    changes: Vec<Change>,
    strategy: ConflictResolution,
) -> Result<ResolvedChanges> {
    let mut actions = Vec::new();
    let mut conflicts_resolved = 0;
    let mut conflicts_renamed = 0;

    for change in changes {
        match change.change_type {
            // No conflicts - straightforward actions
            ChangeType::NewInSource => {
                if let Some(source) = change.source_entry {
                    actions.push(SyncAction::CopyToDest(source));
                }
            }
            ChangeType::NewInDest => {
                if let Some(dest) = change.dest_entry {
                    actions.push(SyncAction::CopyToSource(dest));
                }
            }
            ChangeType::ModifiedInSource => {
                if let Some(source) = change.source_entry {
                    actions.push(SyncAction::CopyToDest(source));
                }
            }
            ChangeType::ModifiedInDest => {
                if let Some(dest) = change.dest_entry {
                    actions.push(SyncAction::CopyToSource(dest));
                }
            }
            ChangeType::DeletedFromSource => {
                actions.push(SyncAction::DeleteFromDest(change.path.clone()));
            }
            ChangeType::DeletedFromDest => {
                actions.push(SyncAction::DeleteFromSource(change.path.clone()));
            }

            // Conflicts - apply resolution strategy
            ChangeType::ModifiedBoth
            | ChangeType::CreateCreateConflict
            | ChangeType::ModifyDeleteConflict => {
                let resolved_action = resolve_conflict(&change, strategy)?;
                if matches!(resolved_action, SyncAction::RenameConflict { .. }) {
                    conflicts_renamed += 1;
                } else {
                    conflicts_resolved += 1;
                }
                actions.push(resolved_action);
            }
        }
    }

    Ok(ResolvedChanges {
        actions,
        conflicts_resolved,
        conflicts_renamed,
    })
}

/// Resolve a single conflict
fn resolve_conflict(change: &Change, strategy: ConflictResolution) -> Result<SyncAction> {
    let source = change.source_entry.as_ref();
    let dest = change.dest_entry.as_ref();

    match strategy {
        ConflictResolution::Newer => resolve_by_mtime(source, dest, &change.path),
        ConflictResolution::Larger => resolve_by_size(source, dest, &change.path, false),
        ConflictResolution::Smaller => resolve_by_size(source, dest, &change.path, true),
        ConflictResolution::Source => {
            if let Some(s) = source {
                Ok(SyncAction::CopyToDest(s.clone()))
            } else {
                // Source deleted, dest exists (modify-delete conflict)
                Ok(SyncAction::DeleteFromDest(change.path.clone()))
            }
        }
        ConflictResolution::Dest => {
            if let Some(d) = dest {
                Ok(SyncAction::CopyToSource(d.clone()))
            } else {
                // Dest deleted, source exists (modify-delete conflict)
                Ok(SyncAction::DeleteFromSource(change.path.clone()))
            }
        }
        ConflictResolution::Rename => {
            if let (Some(s), Some(d)) = (source, dest) {
                Ok(SyncAction::RenameConflict {
                    source: s.clone(),
                    dest: d.clone(),
                    timestamp: generate_conflict_timestamp(),
                })
            } else {
                // Modify-delete conflict: can't rename if one doesn't exist
                // Fall back to keeping the existing file
                if let Some(s) = source {
                    Ok(SyncAction::CopyToDest(s.clone()))
                } else if let Some(d) = dest {
                    Ok(SyncAction::CopyToSource(d.clone()))
                } else {
                    // Both None - shouldn't happen, but return a no-op action
                    Ok(SyncAction::DeleteFromSource(PathBuf::new()))
                }
            }
        }
    }
}

/// Resolve by modification time (newer wins)
fn resolve_by_mtime(
    source: Option<&FileEntry>,
    dest: Option<&FileEntry>,
    path: &Path,
) -> Result<SyncAction> {
    match (source, dest) {
        (Some(s), Some(d)) => {
            if s.modified > d.modified {
                Ok(SyncAction::CopyToDest(s.clone()))
            } else if d.modified > s.modified {
                Ok(SyncAction::CopyToSource(d.clone()))
            } else {
                // Tie - fall back to rename
                Ok(SyncAction::RenameConflict {
                    source: s.clone(),
                    dest: d.clone(),
                    timestamp: generate_conflict_timestamp(),
                })
            }
        }
        (Some(s), None) => Ok(SyncAction::CopyToDest(s.clone())),
        (None, Some(d)) => Ok(SyncAction::CopyToSource(d.clone())),
        (None, None) => {
            // Both deleted - nothing to do (shouldn't happen)
            Ok(SyncAction::DeleteFromSource(path.to_path_buf()))
        }
    }
}

/// Resolve by size (larger or smaller wins)
fn resolve_by_size(
    source: Option<&FileEntry>,
    dest: Option<&FileEntry>,
    path: &Path,
    prefer_smaller: bool,
) -> Result<SyncAction> {
    match (source, dest) {
        (Some(s), Some(d)) => {
            let source_wins = if prefer_smaller {
                s.size < d.size
            } else {
                s.size > d.size
            };

            if source_wins {
                Ok(SyncAction::CopyToDest(s.clone()))
            } else if s.size != d.size {
                Ok(SyncAction::CopyToSource(d.clone()))
            } else {
                // Tie - fall back to rename
                Ok(SyncAction::RenameConflict {
                    source: s.clone(),
                    dest: d.clone(),
                    timestamp: generate_conflict_timestamp(),
                })
            }
        }
        (Some(s), None) => Ok(SyncAction::CopyToDest(s.clone())),
        (None, Some(d)) => Ok(SyncAction::CopyToSource(d.clone())),
        (None, None) => Ok(SyncAction::DeleteFromSource(path.to_path_buf())),
    }
}

/// Generate timestamp for conflict filename
fn generate_conflict_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    format!("{}", now.as_secs())
}

/// Generate conflict filename
pub fn conflict_filename(original: &Path, timestamp: &str, side: &str) -> PathBuf {
    let parent = original.parent();
    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = original.extension().and_then(|e| e.to_str());

    let conflict_name = if let Some(e) = ext {
        format!("{}.conflict-{}-{}.{}", stem, timestamp, side, e)
    } else {
        format!("{}.conflict-{}-{}", stem, timestamp, side)
    };

    if let Some(p) = parent {
        p.join(conflict_name)
    } else {
        PathBuf::from(conflict_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bisync::classifier::ChangeType;
    use std::time::{Duration, SystemTime};

    fn make_file_entry(path: &str, size: u64, mtime_secs_ago: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(path),
            size,
            modified: SystemTime::now() - Duration::from_secs(mtime_secs_ago),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: size,
            xattrs: None,
            inode: None,
            nlink: 1,
            acls: None,
            bsd_flags: None,
        }
    }

    #[test]
    fn test_resolve_new_in_source() {
        let changes = vec![Change {
            path: PathBuf::from("new.txt"),
            change_type: ChangeType::NewInSource,
            source_entry: Some(make_file_entry("new.txt", 100, 0)),
            dest_entry: None,
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Newer).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(resolved.actions[0], SyncAction::CopyToDest(_)));
    }

    #[test]
    fn test_resolve_modified_both_newer_wins() {
        let source = make_file_entry("file.txt", 100, 0); // Recent
        let dest = make_file_entry("file.txt", 100, 120); // 2 minutes ago

        let changes = vec![Change {
            path: PathBuf::from("file.txt"),
            change_type: ChangeType::ModifiedBoth,
            source_entry: Some(source),
            dest_entry: Some(dest),
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Newer).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(resolved.actions[0], SyncAction::CopyToDest(_)));
        assert_eq!(resolved.conflicts_resolved, 1);
    }

    #[test]
    fn test_resolve_modified_both_larger_wins() {
        let source = make_file_entry("file.txt", 200, 0);
        let dest = make_file_entry("file.txt", 100, 0);

        let changes = vec![Change {
            path: PathBuf::from("file.txt"),
            change_type: ChangeType::ModifiedBoth,
            source_entry: Some(source),
            dest_entry: Some(dest),
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Larger).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(resolved.actions[0], SyncAction::CopyToDest(_)));
    }

    #[test]
    fn test_resolve_modified_both_smaller_wins() {
        let source = make_file_entry("file.txt", 100, 0);
        let dest = make_file_entry("file.txt", 200, 0);

        let changes = vec![Change {
            path: PathBuf::from("file.txt"),
            change_type: ChangeType::ModifiedBoth,
            source_entry: Some(source),
            dest_entry: Some(dest),
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Smaller).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(resolved.actions[0], SyncAction::CopyToDest(_)));
    }

    #[test]
    fn test_resolve_modified_both_source_wins() {
        let source = make_file_entry("file.txt", 100, 120);
        let dest = make_file_entry("file.txt", 200, 0);

        let changes = vec![Change {
            path: PathBuf::from("file.txt"),
            change_type: ChangeType::ModifiedBoth,
            source_entry: Some(source),
            dest_entry: Some(dest),
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Source).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(resolved.actions[0], SyncAction::CopyToDest(_)));
    }

    #[test]
    fn test_resolve_modified_both_dest_wins() {
        let source = make_file_entry("file.txt", 200, 0);
        let dest = make_file_entry("file.txt", 100, 120);

        let changes = vec![Change {
            path: PathBuf::from("file.txt"),
            change_type: ChangeType::ModifiedBoth,
            source_entry: Some(source),
            dest_entry: Some(dest),
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Dest).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(resolved.actions[0], SyncAction::CopyToSource(_)));
    }

    #[test]
    fn test_resolve_modified_both_rename() {
        let source = make_file_entry("file.txt", 100, 0);
        let dest = make_file_entry("file.txt", 100, 0);

        let changes = vec![Change {
            path: PathBuf::from("file.txt"),
            change_type: ChangeType::ModifiedBoth,
            source_entry: Some(source),
            dest_entry: Some(dest),
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Rename).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(
            resolved.actions[0],
            SyncAction::RenameConflict { .. }
        ));
        assert_eq!(resolved.conflicts_renamed, 1);
    }

    #[test]
    fn test_resolve_mtime_tie_falls_back_to_rename() {
        let now = SystemTime::now();
        let mut source = make_file_entry("file.txt", 100, 0);
        let mut dest = make_file_entry("file.txt", 200, 0);
        source.modified = now;
        dest.modified = now; // Same mtime

        let changes = vec![Change {
            path: PathBuf::from("file.txt"),
            change_type: ChangeType::ModifiedBoth,
            source_entry: Some(source),
            dest_entry: Some(dest),
        }];

        let resolved = resolve_changes(changes, ConflictResolution::Newer).unwrap();
        assert_eq!(resolved.actions.len(), 1);
        assert!(matches!(
            resolved.actions[0],
            SyncAction::RenameConflict { .. }
        ));
    }

    #[test]
    fn test_conflict_filename() {
        let original = PathBuf::from("path/to/file.txt");
        let conflict = conflict_filename(&original, "1234567890", "source");
        assert_eq!(
            conflict,
            PathBuf::from("path/to/file.conflict-1234567890-source.txt")
        );

        let no_ext = PathBuf::from("file");
        let conflict = conflict_filename(&no_ext, "1234567890", "dest");
        assert_eq!(conflict, PathBuf::from("file.conflict-1234567890-dest"));
    }

    #[test]
    fn test_multiple_changes_mixed() {
        let changes = vec![
            Change {
                path: PathBuf::from("new.txt"),
                change_type: ChangeType::NewInSource,
                source_entry: Some(make_file_entry("new.txt", 100, 0)),
                dest_entry: None,
            },
            Change {
                path: PathBuf::from("modified.txt"),
                change_type: ChangeType::ModifiedInDest,
                source_entry: Some(make_file_entry("modified.txt", 100, 60)),
                dest_entry: Some(make_file_entry("modified.txt", 200, 0)),
            },
            Change {
                path: PathBuf::from("conflict.txt"),
                change_type: ChangeType::ModifiedBoth,
                source_entry: Some(make_file_entry("conflict.txt", 100, 0)),
                dest_entry: Some(make_file_entry("conflict.txt", 200, 60)),
            },
        ];

        let resolved = resolve_changes(changes, ConflictResolution::Newer).unwrap();
        assert_eq!(resolved.actions.len(), 3);
        assert_eq!(resolved.conflicts_resolved, 1);
    }

    #[test]
    fn test_conflict_resolution_from_str() {
        assert_eq!(
            ConflictResolution::from_str("newer"),
            Some(ConflictResolution::Newer)
        );
        assert_eq!(
            ConflictResolution::from_str("Larger"),
            Some(ConflictResolution::Larger)
        );
        assert_eq!(
            ConflictResolution::from_str("SMALLER"),
            Some(ConflictResolution::Smaller)
        );
        assert_eq!(
            ConflictResolution::from_str("source"),
            Some(ConflictResolution::Source)
        );
        assert_eq!(
            ConflictResolution::from_str("dest"),
            Some(ConflictResolution::Dest)
        );
        assert_eq!(
            ConflictResolution::from_str("rename"),
            Some(ConflictResolution::Rename)
        );
        assert_eq!(ConflictResolution::from_str("invalid"), None);
    }
}
