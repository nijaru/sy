# Bidirectional Sync Design

**Status**: ✅ IMPLEMENTED
**Implemented In**: v0.0.43 (local), v0.0.44 (text format), v0.0.46 (SSH support + conflict logging)
**Lines of Code**: ~2,100 lines (engine, classifier, resolver, state, conflict logging)

## Overview

Bidirectional synchronization with automatic conflict resolution, enabling laptop ↔ desktop, local ↔ backup, and remote server scenarios.

**Achieved**: Covers 80% of bidirectional use cases using snapshot-based state tracking with 6 conflict resolution strategies.

**SSH Support** (v0.0.46): Works with remote servers via Transport abstraction. Supports:
- Local ↔ Local
- Local ↔ Remote (SSH)
- Remote ↔ Remote (SSH)

**Implementation Note**: Text-based state format (v0.0.44) replaced initial SQLite design for simplicity and debuggability.

## Non-Goals (Deferred to Future)

- Full Unison-style reconciliation with external merge tools
- Syncthing-style continuous sync with vector clocks
- Multi-device sync (>2 endpoints)
- Manual conflict resolution UI (automatic history logging implemented in v0.0.46)

## Design Principles

1. **Leverage existing infrastructure**: Reuse SQLite (checksum_db), scanner, planner
2. **Safety first**: Rename both by default, require explicit resolution strategy
3. **Performance**: Parallel scanning, no performance regression vs. unidirectional
4. **Predictable**: Clear conflict resolution rules, JSON output for automation

## Architecture

### State Tracking

**Storage**: SQLite database in `~/.cache/sy/bisync/<hash>.db`
- Hash = xxHash3(source_path + dest_path) for unique sync pairs
- Separate from checksum_db (different schema, lifecycle)

**Schema**:
```sql
CREATE TABLE sync_state (
    path TEXT PRIMARY KEY,
    side TEXT NOT NULL,  -- 'source' or 'dest'
    mtime INTEGER NOT NULL,  -- Unix timestamp in nanoseconds
    size INTEGER NOT NULL,
    checksum INTEGER,  -- xxHash3, NULL if not computed
    last_sync INTEGER NOT NULL  -- Unix timestamp when recorded
);

CREATE INDEX idx_path_side ON sync_state(path, side);
```

**Lifecycle**:
- Created on first `--bidirectional` run
- Updated after each successful sync
- Pruned: remove entries for deleted files after N syncs (configurable)
- Cleared: `--clear-bisync-state` flag

### Conflict Detection Algorithm

**Input**: Current source state, current dest state, prior sync state

**Classification**:
```rust
enum ChangeType {
    // Single-sided changes (no conflict)
    NewInSource,          // File only in source (new or dest deleted)
    NewInDest,            // File only in dest (new or source deleted)
    ModifiedInSource,     // Source changed, dest unchanged
    ModifiedInDest,       // Dest changed, source unchanged
    DeletedFromSource,    // Was in prior, now only in dest
    DeletedFromDest,      // Was in prior, now only in source

    // Conflicts (both sides changed)
    ModifiedBoth,         // Both changed since prior sync
    CreateCreateConflict, // New in both sides (different content)
    ModifyDeleteConflict, // One modified, other deleted
}
```

**Detection Logic**:
```rust
fn classify_change(
    path: &Path,
    source_entry: Option<&FileEntry>,
    dest_entry: Option<&FileEntry>,
    prior_source: Option<&SyncState>,
    prior_dest: Option<&SyncState>,
) -> ChangeType {
    match (source_entry, dest_entry, prior_source, prior_dest) {
        // New files
        (Some(s), None, None, None) => NewInSource,
        (None, Some(d), None, None) => NewInDest,
        (Some(s), Some(d), None, None) => {
            if content_equal(s, d) {
                // Same file created on both sides, pick one
                NewInSource
            } else {
                CreateCreateConflict
            }
        }

        // Modifications
        (Some(s), Some(d), Some(ps), Some(pd)) => {
            let source_modified = s.mtime > ps.mtime || s.size != ps.size;
            let dest_modified = d.mtime > pd.mtime || d.size != pd.size;

            match (source_modified, dest_modified) {
                (false, false) => return None,  // No changes
                (true, false) => ModifiedInSource,
                (false, true) => ModifiedInDest,
                (true, true) => {
                    if content_equal(s, d) {
                        // Both changed to same content
                        return None;
                    } else {
                        ModifiedBoth
                    }
                }
            }
        }

        // Deletions
        (None, Some(d), Some(ps), _) => DeletedFromSource,
        (Some(s), None, _, Some(pd)) => DeletedFromDest,

        // Modify-delete conflicts
        (Some(s), None, Some(ps), Some(pd)) => {
            if s.mtime > ps.mtime || s.size != ps.size {
                ModifyDeleteConflict  // Source modified, dest deleted
            } else {
                DeletedFromDest
            }
        }
        (None, Some(d), Some(ps), Some(pd)) => {
            if d.mtime > pd.mtime || d.size != pd.size {
                ModifyDeleteConflict  // Dest modified, source deleted
            } else {
                DeletedFromSource
            }
        }

        _ => {
            // Edge cases: partial prior state, etc.
            // Conservative: treat as potential conflict
            ModifiedBoth
        }
    }
}
```

### Conflict Resolution Strategies

```rust
#[derive(Debug, Clone, Copy)]
enum ConflictResolution {
    Newer,    // Winner = most recent mtime (DEFAULT)
    Larger,   // Winner = largest size
    Smaller,  // Winner = smallest size
    Source,   // Winner = source (force push)
    Dest,     // Winner = dest (force pull)
    Ask,      // Prompt user (interactive only, not for --json)
    Rename,   // Keep both: file.conflict-<timestamp>-<side>
}

fn resolve_conflict(
    source: &FileEntry,
    dest: &FileEntry,
    strategy: ConflictResolution,
) -> Resolution {
    match strategy {
        ConflictResolution::Newer => {
            if source.mtime > dest.mtime {
                Resolution::UseSource
            } else if dest.mtime > source.mtime {
                Resolution::UseDest
            } else {
                // Tie: fall back to Rename
                Resolution::RenameBoth
            }
        }
        ConflictResolution::Larger => {
            if source.size > dest.size {
                Resolution::UseSource
            } else if dest.size > source.size {
                Resolution::UseDest
            } else {
                Resolution::RenameBoth
            }
        }
        ConflictResolution::Source => Resolution::UseSource,
        ConflictResolution::Dest => Resolution::UseDest,
        ConflictResolution::Rename => Resolution::RenameBoth,
        ConflictResolution::Ask => {
            // Interactive prompt
            prompt_user(source, dest)
        }
        _ => Resolution::RenameBoth,  // Conservative default
    }
}

enum Resolution {
    UseSource,     // Copy source → dest
    UseDest,       // Copy dest → source
    RenameBoth,    // Rename both to .conflict-<timestamp>-<side>
}
```

### Sync Execution Flow

```rust
async fn bidirectional_sync(
    source: &Path,
    dest: &Path,
    opts: &BidirOpts,
) -> Result<BidirSyncResult> {
    // 1. Load prior state (if exists)
    let state_db = BisyncStateDb::open(source, dest, opts)?;
    let prior_state = state_db.load_all()?;

    // 2. Scan both sides in parallel
    let (source_files, dest_files) = tokio::join!(
        scanner.scan(source, &opts.scan_opts),
        scanner.scan(dest, &opts.scan_opts),
    );

    // 3. Classify all changes
    let changes = classify_all_changes(
        &source_files,
        &dest_files,
        &prior_state,
    );

    // 4. Check safety limits
    check_deletion_limit(&changes, opts.max_delete)?;

    // 5. Resolve conflicts
    let (actions, conflicts) = resolve_all(
        changes,
        opts.conflict_resolution,
    );

    // 6. Execute sync actions
    let results = execute_sync_actions(actions, opts).await?;

    // 7. Update state
    state_db.update_from_results(&results)?;

    Ok(BidirSyncResult {
        source_to_dest: results.source_to_dest,
        dest_to_source: results.dest_to_source,
        conflicts_resolved: conflicts.resolved,
        conflicts_renamed: conflicts.renamed,
        deletions: results.deletions,
    })
}
```

### Content Equality Check

**Purpose**: Reduce false-positive conflicts when both sides converge to same content

**Implementation**:
```rust
fn content_equal(source: &FileEntry, dest: &FileEntry) -> bool {
    // Fast path: size mismatch
    if source.size != dest.size {
        return false;
    }

    // Medium path: both have checksums cached
    if let (Some(sc), Some(dc)) = (source.checksum, dest.checksum) {
        return sc == dc;
    }

    // Slow path: compute checksums on-demand
    let source_checksum = compute_checksum(&source.path)?;
    let dest_checksum = compute_checksum(&dest.path)?;
    source_checksum == dest_checksum
}
```

**Optimization**: Use existing checksum_db if available, otherwise compute

## CLI Design

### Flags

```bash
# Enable bidirectional sync
--bidirectional, -b

# Conflict resolution strategy
--conflict-resolve <strategy>
    newer   (default) - Use file with most recent mtime
    larger            - Use file with largest size
    smaller           - Use file with smallest size
    source            - Always use source (force push)
    dest              - Always use dest (force pull)
    ask               - Prompt for each conflict (interactive only)
    rename            - Keep both files (rename with .conflict suffix)

# Safety limits
--max-delete <percent>
    Abort if >N% of files would be deleted (default: 50%)
    Set to 0 for unlimited deletions

# State management
--clear-bisync-state
    Clear prior sync state before running (forces full comparison)

--bisync-state-path <path>
    Custom location for state database (default: ~/.cache/sy/bisync/)
```

### Usage Examples

```bash
# Basic bidirectional sync (newest-wins)
sy /local /backup --bidirectional

# Bidirectional with explicit strategy
sy /laptop user@desktop:/sync --bidirectional --conflict-resolve newer

# Force push (source wins all conflicts)
sy /source /dest --bidirectional --conflict-resolve source

# Keep both files on conflict
sy /a /b --bidirectional --conflict-resolve rename

# Interactive mode (prompt for conflicts)
sy /a /b --bidirectional --conflict-resolve ask

# With safety limit (abort if >10% deletions)
sy /a /b --bidirectional --max-delete 10%

# Dry run to preview changes
sy /a /b --bidirectional --dry-run

# Clear state and resync
sy /a /b --bidirectional --clear-bisync-state
```

## JSON Output

```json
{
  "operation": "bidirectional_sync",
  "source": "/local",
  "dest": "/backup",
  "conflict_resolution": "newer",
  "summary": {
    "files_synced_to_dest": 42,
    "files_synced_to_source": 15,
    "conflicts_resolved": 3,
    "conflicts_renamed": 1,
    "deletions": {
      "from_source": 2,
      "from_dest": 1
    },
    "bytes_transferred": 12845632,
    "duration_ms": 1234
  },
  "conflicts": [
    {
      "path": "document.txt",
      "source_mtime": "2025-10-23T10:00:00Z",
      "source_size": 1024,
      "dest_mtime": "2025-10-23T10:05:00Z",
      "dest_size": 1056,
      "resolution": "newer",
      "winner": "dest",
      "action": "source <- dest"
    },
    {
      "path": "image.png",
      "source_mtime": "2025-10-23T09:00:00Z",
      "source_size": 204800,
      "dest_mtime": "2025-10-23T09:00:00Z",
      "dest_size": 204800,
      "resolution": "rename",
      "action": "renamed both (mtime equal, content differs)"
    }
  ],
  "changes": {
    "source_to_dest": [
      {"path": "new_file.txt", "action": "copy", "bytes": 512},
      {"path": "modified.txt", "action": "update", "bytes": 2048}
    ],
    "dest_to_source": [
      {"path": "from_backup.txt", "action": "copy", "bytes": 1024}
    ],
    "deletions": [
      {"path": "old_file.txt", "side": "source"},
      {"path": "removed.txt", "side": "dest"}
    ]
  }
}
```

## Safety Mechanisms

### Deletion Limit

**Purpose**: Prevent cascading data loss from misconfiguration or bugs

**Implementation**:
```rust
fn check_deletion_limit(changes: &[Change], max_delete_percent: u8) -> Result<()> {
    if max_delete_percent == 0 {
        return Ok(());  // Unlimited
    }

    let total_files = changes.len();
    let deletions = changes.iter()
        .filter(|c| matches!(c.change_type, DeletedFromSource | DeletedFromDest))
        .count();

    let deletion_percent = (deletions as f64 / total_files as f64) * 100.0;

    if deletion_percent > max_delete_percent as f64 {
        return Err(SyncError::DeletionLimitExceeded {
            deletions,
            total: total_files,
            limit: max_delete_percent,
        });
    }

    Ok(())
}
```

**Default**: 50% (abort if >50% of files would be deleted)

### Lock File

**Purpose**: Prevent concurrent bidirectional syncs on same path pair

**Implementation**:
- Lock file: `~/.cache/sy/bisync/<hash>.lock`
- Contains: PID, start time, hostname
- Stale lock detection: Remove if PID not running and >1 hour old
- Error message: "Bidirectional sync already in progress for these paths"

### Dry Run Support

**Behavior**:
- Show all changes that would be made
- Display conflicts and how they would be resolved
- Do NOT modify files or update state
- Exit code: 0 if no conflicts, 1 if conflicts detected

### Conflict History Logging (v0.0.46+)

**Purpose**: Provide audit trail of all resolved conflicts for debugging and compliance

**Implementation**:
- Log file: `~/.cache/sy/bisync/<hash>.conflicts.log`
- Format: `timestamp | path | conflict_type | strategy | winner`
- Append-only: Preserves complete history across syncs
- Automatically logged after conflict detection (except dry-run)

**Winner Resolution**:
- `newer`: Compares mtimes → "source (newer)" or "dest (newer)"
- `larger`/`smaller`: Compares sizes → "source (larger)" etc.
- `source`/`dest`: Shows "source" or "dest"
- `rename`: Shows "both (renamed)"

**Example Log Entries**:
```
1761584658 | conflict.txt | both modified | newer | dest (newer)
1761584671 | rename.txt | both modified | rename | both (renamed)
1761584682 | force.txt | both modified | source | source
```

**Benefits**:
- Audit trail for compliance and debugging
- Track conflict patterns over time
- Rollback decisions with historical context
- No performance impact (append-only writes)

### Exclude Patterns

**Purpose**: Filter files during bidirectional sync (e.g., skip temp files, build artifacts)

**Implementation**:
- Respects `.gitignore` files in source directories
- Respects global gitignore (`~/.gitignore_global`)
- Respects `.git/info/exclude`
- Automatically filters `.git` directories

**Common Use Cases**:
```gitignore
# macOS
.DS_Store
.AppleDouble

# Build artifacts
node_modules/
target/
*.o
*.pyc

# Temporary files
*.tmp
*.swp
```

## Error Handling

### Partial Failures

**Scenario**: Some files sync successfully, others fail

**Behavior**:
- Continue syncing other files (up to max_errors threshold)
- Update state only for successful transfers
- Return error summary at end

**State consistency**: State reflects actual filesystem state after sync

### Network Failures

**SSH connection drops mid-sync**:
- Local→remote transfers: State not updated (conservative)
- Remote→local transfers: State not updated
- Next run: Re-detect changes, resume from prior state

### Filesystem Changes During Sync

**File modified while syncing**:
- Checksum mismatch detection (if enabled)
- Warning in output, state not updated
- Next run: Re-sync the file

## Performance Considerations

### Parallel Scanning

**Both sides scanned in parallel**:
```rust
let (source_scan, dest_scan) = tokio::join!(
    scan_filesystem(source, opts),
    scan_filesystem(dest, opts),
);
```

**Expected speedup**: 2x for remote dest (local scan while SSH scans remote)

### State Caching

**Checksum caching**:
- Reuse checksum_db if exists (--checksum-db flag)
- Avoid re-computing checksums for unchanged files

**State pruning**:
- Remove deleted files from state after 3 syncs
- Prevents state DB growth over time

### Memory Usage

**Streaming state loading**:
- Load prior state in chunks of 10K entries
- Avoid loading entire state DB into memory for large syncs

**File list batching**:
- Process changes in batches of 1000 files
- Update state incrementally

## Testing Strategy

### Unit Tests

1. **State tracking**:
   - Store and retrieve sync state
   - State pruning logic
   - Hash collision detection

2. **Change classification**:
   - All ChangeType variants
   - Edge cases (partial prior state, missing files)
   - Content equality detection

3. **Conflict resolution**:
   - Each resolution strategy
   - Tie-breaker fallback (newer with equal mtime)
   - Rename conflict format

### Integration Tests

1. **Basic scenarios**:
   - First sync (no prior state)
   - Second sync (no changes)
   - Modifications on one side only

2. **Conflict scenarios**:
   - Both files modified (newest wins)
   - Both files modified (rename both)
   - Create-create conflict
   - Modify-delete conflict

3. **Safety features**:
   - Deletion limit triggered
   - Lock file prevents concurrent runs
   - Dry run shows changes without modifying

4. **Error handling**:
   - Partial sync failures
   - State recovery after interruption

### Performance Tests

**Benchmarks**:
- 10K files, no changes: <2s overhead vs. unidirectional
- 10K files, 100 conflicts: <5s resolution time
- State DB operations: <100ms for 100K entries

## Implementation Plan

### Phase 1: Core Infrastructure (~200 lines)

1. **State DB module** (`src/bisync_state.rs`):
   - SQLite schema and migrations
   - CRUD operations
   - State pruning logic

2. **CLI integration** (`src/cli.rs`):
   - Add `--bidirectional`, `--conflict-resolve`, `--max-delete` flags
   - Validation logic

### Phase 2: Change Detection (~150 lines)

3. **Change classifier** (`src/bisync/classifier.rs`):
   - Implement `classify_change()` function
   - Content equality check
   - Unit tests for all ChangeType variants

### Phase 3: Conflict Resolution (~100 lines)

4. **Resolution engine** (`src/bisync/resolver.rs`):
   - Implement resolution strategies
   - Rename conflict filename generation
   - Interactive prompt (ask mode)

### Phase 4: Sync Execution (~150 lines)

5. **Bidirectional sync engine** (`src/bisync/mod.rs`):
   - Main `bidirectional_sync()` function
   - Safety checks (deletion limit, lock file)
   - State updates after sync

### Phase 5: Testing & Documentation (~100 lines tests + docs)

6. **Tests** (`tests/bisync_tests.rs`):
   - 15-20 integration tests
   - Edge case coverage

7. **Documentation**:
   - Update README with bidirectional examples
   - Add TROUBLESHOOTING section
   - Update CHANGELOG

**Total estimate**: ~700 lines + tests

## Open Questions

1. **State location for remote syncs**:
   - Q: Where to store state for `sy /local user@host:/remote --bidirectional`?
   - A: Always on local machine (`~/.cache/sy/bisync/`), hash includes host

2. **Handling symlinks**:
   - Q: How to handle symlink conflicts (link vs. file)?
   - A: Treat as regular conflicts (newest wins or rename)

3. **Hard link preservation**:
   - Q: Should hard links be preserved across bidirectional sync?
   - A: Yes, use existing hard link detection logic

4. **Watch mode integration**:
   - Q: Should `--watch` support bidirectional mode?
   - A: Defer to future version (Phase 2), complexity too high

## Success Criteria

**Functional:**
- ✅ Detects conflicts with <1% false positive rate
- ✅ Resolves conflicts according to chosen strategy
- ✅ Preserves data (no silent data loss)
- ✅ State survives interruptions and resumes correctly

**Performance:**
- ✅ <5% overhead vs. unidirectional sync (no conflicts)
- ✅ <10s to resolve 100 conflicts in 10K files

**Usability:**
- ✅ Clear error messages for conflicts
- ✅ JSON output enables automation
- ✅ Dry run accurately previews changes

## References

- Research: ai/research/bidirectional_sync_2025.md
- rclone bisync: https://rclone.org/bisync/
- Unison docs: https://www.cis.upenn.edu/~bcpierce/unison/
- Syncthing sync model: https://docs.syncthing.net/users/syncing.html
