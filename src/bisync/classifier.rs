// Change classification for bidirectional sync
//
// Detects what changed between source, dest, and prior sync state

#[cfg(test)]
use crate::bisync::state::Side;
use crate::bisync::state::{StateMap, SyncState};
use crate::error::Result;
use crate::sync::scanner::FileEntry;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Type of change detected
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeType {
    // Single-sided changes (no conflict)
    NewInSource,       // File only in source (new or dest deleted)
    NewInDest,         // File only in dest (new or source deleted)
    ModifiedInSource,  // Source changed, dest unchanged
    ModifiedInDest,    // Dest changed, source unchanged
    DeletedFromSource, // Was in prior, now only in dest
    DeletedFromDest,   // Was in prior, now only in source

    // Conflicts (both sides changed)
    ModifiedBoth,         // Both changed since prior sync
    CreateCreateConflict, // New in both sides (different content)
    ModifyDeleteConflict, // One modified, other deleted
}

/// A detected change
#[derive(Debug, Clone)]
pub struct Change {
    pub path: PathBuf,
    pub change_type: ChangeType,
    pub source_entry: Option<FileEntry>,
    pub dest_entry: Option<FileEntry>,
}

/// Classify all changes between source, dest, and prior state
pub fn classify_changes(
    source_files: &[FileEntry],
    dest_files: &[FileEntry],
    prior_state: &StateMap,
) -> Result<Vec<Change>> {
    // Build lookups by relative path
    let mut source_map: HashMap<PathBuf, &FileEntry> = HashMap::with_capacity(source_files.len());
    for entry in source_files {
        source_map.insert((*entry.relative_path).clone(), entry);
    }

    let mut dest_map: HashMap<PathBuf, &FileEntry> = HashMap::with_capacity(dest_files.len());
    for entry in dest_files {
        dest_map.insert((*entry.relative_path).clone(), entry);
    }

    // Collect all unique paths (pre-allocate for worst case: all unique)
    let capacity = source_map.len() + dest_map.len() + prior_state.len();
    let mut all_paths: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::with_capacity(capacity);
    all_paths.extend(source_map.keys().cloned());
    all_paths.extend(dest_map.keys().cloned());
    all_paths.extend(prior_state.keys().cloned());

    let mut changes = Vec::with_capacity(all_paths.len());

    for path in all_paths {
        let source_entry = source_map.get(&path).copied();
        let dest_entry = dest_map.get(&path).copied();
        let prior = prior_state.get(&path);

        if let Some(change) = classify_single_path(
            &path,
            source_entry,
            dest_entry,
            prior.and_then(|(s, _)| s.as_ref()),
            prior.and_then(|(_, d)| d.as_ref()),
        )? {
            changes.push(change);
        }
    }

    Ok(changes)
}

/// Classify a single path
fn classify_single_path(
    path: &Path,
    source_entry: Option<&FileEntry>,
    dest_entry: Option<&FileEntry>,
    prior_source: Option<&SyncState>,
    prior_dest: Option<&SyncState>,
) -> Result<Option<Change>> {
    // Skip directories (we only sync files)
    if source_entry.is_some_and(|e| e.is_dir) || dest_entry.is_some_and(|e| e.is_dir) {
        return Ok(None);
    }

    let change_type = match (source_entry, dest_entry, prior_source, prior_dest) {
        // Both exist now, neither existed before (new in both)
        (Some(s), Some(d), None, None) => {
            if content_equal(s, d)? {
                // Same file created on both sides, no conflict
                return Ok(None);
            } else {
                ChangeType::CreateCreateConflict
            }
        }

        // Only source exists now, didn't exist before (new in source)
        (Some(_), None, None, None) => ChangeType::NewInSource,

        // Only dest exists now, didn't exist before (new in dest)
        (None, Some(_), None, None) => ChangeType::NewInDest,

        // Both exist now, both existed before (check modifications)
        (Some(s), Some(d), Some(ps), Some(pd)) => {
            let source_modified = is_modified(s, ps);
            let dest_modified = is_modified(d, pd);

            match (source_modified, dest_modified) {
                (false, false) => return Ok(None), // No changes
                (true, false) => ChangeType::ModifiedInSource,
                (false, true) => ChangeType::ModifiedInDest,
                (true, true) => {
                    if content_equal(s, d)? {
                        // Both changed to same content
                        return Ok(None);
                    } else {
                        ChangeType::ModifiedBoth
                    }
                }
            }
        }

        // Source deleted, dest unchanged
        (None, Some(d), Some(_ps), Some(pd)) => {
            if is_modified(d, pd) {
                // Dest modified while source deleted
                ChangeType::ModifyDeleteConflict
            } else {
                ChangeType::DeletedFromSource
            }
        }

        // Dest deleted, source unchanged
        (Some(s), None, Some(ps), Some(_pd)) => {
            if is_modified(s, ps) {
                // Source modified while dest deleted
                ChangeType::ModifyDeleteConflict
            } else {
                ChangeType::DeletedFromDest
            }
        }

        // Both deleted (no action needed)
        (None, None, Some(_), Some(_)) => return Ok(None),

        // Source exists now, only dest existed before (dest deleted, new source)
        (Some(_), None, None, Some(_)) => ChangeType::DeletedFromDest,

        // Dest exists now, only source existed before (source deleted, new dest)
        (None, Some(_), Some(_), None) => ChangeType::DeletedFromSource,

        // Both exist now, only source existed before
        (Some(s), Some(d), Some(ps), None) => {
            // Source may have changed, dest is new
            if is_modified(s, ps) && !content_equal(s, d)? {
                ChangeType::CreateCreateConflict
            } else if content_equal(s, d)? {
                return Ok(None);
            } else {
                ChangeType::NewInDest
            }
        }

        // Both exist now, only dest existed before
        (Some(s), Some(d), None, Some(pd)) => {
            // Dest may have changed, source is new
            if is_modified(d, pd) && !content_equal(s, d)? {
                ChangeType::CreateCreateConflict
            } else if content_equal(s, d)? {
                return Ok(None);
            } else {
                ChangeType::NewInSource
            }
        }

        // Neither exists now, but existed before (both deleted - no action)
        (None, None, _, _) => return Ok(None),

        // Edge cases: partial prior state
        _ => {
            // Conservative: treat unknown states as potential conflicts
            if source_entry.is_some() && dest_entry.is_some() {
                ChangeType::ModifiedBoth
            } else if source_entry.is_some() {
                ChangeType::NewInSource
            } else if dest_entry.is_some() {
                ChangeType::NewInDest
            } else {
                return Ok(None);
            }
        }
    };

    Ok(Some(Change {
        path: path.to_path_buf(),
        change_type,
        source_entry: source_entry.cloned(),
        dest_entry: dest_entry.cloned(),
    }))
}

/// Check if file was modified compared to prior state
fn is_modified(entry: &FileEntry, prior: &SyncState) -> bool {
    // Size change = definitely modified
    if entry.size != prior.size {
        return true;
    }

    // Mtime change = likely modified
    entry.modified > prior.mtime
}

/// Check if two files have equal content
fn content_equal(source: &FileEntry, dest: &FileEntry) -> Result<bool> {
    // Fast path: size mismatch
    if source.size != dest.size {
        return Ok(false);
    }

    // For now, assume equal if sizes match
    // In future: compare checksums if available
    // This is conservative (may miss some conflicts) but safe

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    fn make_file_entry(path: &str, size: u64, mtime_secs_ago: u64) -> FileEntry {
        FileEntry {
            path: Arc::new(PathBuf::from(path)),
            relative_path: Arc::new(PathBuf::from(path)),
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

    fn make_sync_state(path: &str, size: u64, mtime_secs_ago: u64, side: Side) -> SyncState {
        SyncState {
            path: PathBuf::from(path),
            side,
            mtime: SystemTime::now() - Duration::from_secs(mtime_secs_ago),
            size,
            checksum: None,
            last_sync: SystemTime::now(),
        }
    }

    #[test]
    fn test_no_changes() {
        let source = vec![make_file_entry("file.txt", 100, 60)];
        let dest = vec![make_file_entry("file.txt", 100, 60)];
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("file.txt"),
            (
                Some(make_sync_state("file.txt", 100, 60, Side::Source)),
                Some(make_sync_state("file.txt", 100, 60, Side::Dest)),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn test_new_in_source() {
        let source = vec![make_file_entry("new.txt", 100, 0)];
        let dest = vec![];
        let prior = HashMap::new();

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::NewInSource);
        assert_eq!(changes[0].path, PathBuf::from("new.txt"));
    }

    #[test]
    fn test_new_in_dest() {
        let source = vec![];
        let dest = vec![make_file_entry("new.txt", 100, 0)];
        let prior = HashMap::new();

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::NewInDest);
    }

    #[test]
    fn test_modified_in_source() {
        let source = vec![make_file_entry("file.txt", 200, 0)]; // Size changed
        let dest = vec![make_file_entry("file.txt", 100, 60)];
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("file.txt"),
            (
                Some(make_sync_state("file.txt", 100, 60, Side::Source)),
                Some(make_sync_state("file.txt", 100, 60, Side::Dest)),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::ModifiedInSource);
    }

    #[test]
    fn test_modified_in_dest() {
        let source = vec![make_file_entry("file.txt", 100, 60)];
        let dest = vec![make_file_entry("file.txt", 200, 0)]; // Size changed
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("file.txt"),
            (
                Some(make_sync_state("file.txt", 100, 60, Side::Source)),
                Some(make_sync_state("file.txt", 100, 60, Side::Dest)),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::ModifiedInDest);
    }

    #[test]
    fn test_modified_both_conflict() {
        let source = vec![make_file_entry("file.txt", 200, 0)];
        let dest = vec![make_file_entry("file.txt", 300, 0)]; // Different sizes
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("file.txt"),
            (
                Some(make_sync_state("file.txt", 100, 60, Side::Source)),
                Some(make_sync_state("file.txt", 100, 60, Side::Dest)),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::ModifiedBoth);
    }

    #[test]
    fn test_deleted_from_source() {
        let source = vec![];
        let dest = vec![make_file_entry("file.txt", 100, 60)];
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("file.txt"),
            (
                Some(make_sync_state("file.txt", 100, 60, Side::Source)),
                Some(make_sync_state("file.txt", 100, 60, Side::Dest)),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::DeletedFromSource);
    }

    #[test]
    fn test_deleted_from_dest() {
        let source = vec![make_file_entry("file.txt", 100, 60)];
        let dest = vec![];
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("file.txt"),
            (
                Some(make_sync_state("file.txt", 100, 60, Side::Source)),
                Some(make_sync_state("file.txt", 100, 60, Side::Dest)),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::DeletedFromDest);
    }

    #[test]
    fn test_modify_delete_conflict() {
        let source = vec![make_file_entry("file.txt", 200, 0)]; // Modified
        let dest = vec![]; // Deleted
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("file.txt"),
            (
                Some(make_sync_state("file.txt", 100, 60, Side::Source)),
                Some(make_sync_state("file.txt", 100, 60, Side::Dest)),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::ModifyDeleteConflict);
    }

    #[test]
    fn test_create_create_same_content() {
        // Same size = treated as same content (conservative)
        let source = vec![make_file_entry("file.txt", 100, 0)];
        let dest = vec![make_file_entry("file.txt", 100, 0)];
        let prior = HashMap::new();

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 0); // No conflict, content equal
    }

    #[test]
    fn test_create_create_different_content() {
        let source = vec![make_file_entry("file.txt", 100, 0)];
        let dest = vec![make_file_entry("file.txt", 200, 0)]; // Different size
        let prior = HashMap::new();

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, ChangeType::CreateCreateConflict);
    }

    #[test]
    fn test_multiple_changes() {
        let source = vec![
            make_file_entry("new.txt", 100, 0),
            make_file_entry("modified.txt", 200, 0),
        ];
        let dest = vec![
            make_file_entry("modified.txt", 100, 60),
            make_file_entry("deleted_from_source.txt", 100, 60),
        ];
        let mut prior = HashMap::new();
        prior.insert(
            PathBuf::from("modified.txt"),
            (
                Some(make_sync_state("modified.txt", 100, 60, Side::Source)),
                Some(make_sync_state("modified.txt", 100, 60, Side::Dest)),
            ),
        );
        prior.insert(
            PathBuf::from("deleted_from_source.txt"),
            (
                Some(make_sync_state(
                    "deleted_from_source.txt",
                    100,
                    60,
                    Side::Source,
                )),
                Some(make_sync_state(
                    "deleted_from_source.txt",
                    100,
                    60,
                    Side::Dest,
                )),
            ),
        );

        let changes = classify_changes(&source, &dest, &prior).unwrap();
        assert_eq!(changes.len(), 3);

        let change_types: Vec<_> = changes.iter().map(|c| c.change_type.clone()).collect();
        assert!(change_types.contains(&ChangeType::NewInSource));
        assert!(change_types.contains(&ChangeType::ModifiedInSource));
        assert!(change_types.contains(&ChangeType::DeletedFromSource));
    }
}
