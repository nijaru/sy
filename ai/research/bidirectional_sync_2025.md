# Bidirectional Sync Research (2025)

**Date**: 2025-10-23
**Purpose**: Design bidirectional sync for sy based on state-of-the-art approaches

## Summary

Bidirectional sync requires:
1. **State tracking**: Persistent listings from prior run to detect changes
2. **Conflict detection**: Identify files modified on both sides
3. **Resolution strategies**: Automatic (newest/largest/priority) or manual (rename both)
4. **Safety mechanisms**: Deletion limits, equality checks, graceful degradation

## Tools Analyzed

### Unison (Traditional Approach)
**Strengths:**
- Mature conflict detection (saves state on both copies)
- Multiple resolution modes: `prefer newer/older`, `force root`, `copyonconflict`
- External merge tool integration (emacs, ediff)

**Weaknesses:**
- Manual intervention required for conflicts by default
- Less automated than modern tools

**Key Insight**: Prefer newer is most common automated strategy

### Syncthing (Continuous Sync)
**Approach:**
- Detects conflicts, renames to `.sync-conflict-<date>-<time>-<modifiedBy>.<ext>`
- Tie-breaker: If mtime equal, larger device ID wins
- No built-in merge tools (community scripts exist)

**Strengths:**
- Simple conflict handling (rename and propagate)
- Works well for continuous sync

**Weaknesses:**
- No automatic resolution options
- Conflicts accumulate without manual cleanup

**Key Insight**: Renaming conflicts is safest default for preventing data loss

### rclone bisync (Modern State-of-the-Art)
**Implementation** (documented at rclone.org/bisync/):

**State Tracking:**
- Stores `.lst` files with (path, size, mtime, checksum) from prior run
- Compare current state vs. stored listings to classify: new/newer/older/deleted
- Enables recovery from interruptions without full resync

**Conflict Resolution** (`--conflict-resolve`):
- `none` (default): Rename both to `.conflict1`, `.conflict2` (numbered)
- `newer`: Winner = most recent mtime; loser renamed
- `larger`/`smaller`: Winner = size-based
- `path1`/`path2`: One side has unconditional priority

**Loser Handling** (`--conflict-loser`):
- `num`: Auto-number conflicts (`.conflict1`, `.conflict2`, ...)
- `pathname`: Rename by source (`.path1`, `.path2`)
- `delete`: Remove loser (only when winner determinable)

**Safety Features:**
- Content equality check before declaring conflict (skip if identical)
- `--max-delete` percentage threshold prevents cascading deletions
- `--recover` mode: Retry failed ops using backup listings
- Lock files prevent concurrent runs

**Comparison Methods** (`--compare`):
- `size` (fast, least accurate)
- `modtime` (default, good balance)
- `checksum` (slowest, most accurate)
- Graceful degradation if method unsupported by remote

**Status (2024-2025):**
- Feature is in beta, marked as "advanced command, use with care"
- Some reliability issues reported in forums (March 2024)
- Still being refined

**Key Insights:**
- Snapshot-based state tracking is simpler than vector clocks
- Multiple resolution strategies needed (no one-size-fits-all)
- Safety > convenience (rename both by default)
- Equality check reduces false conflicts

## Comparison Matrix

| Feature | Unison | Syncthing | rclone bisync |
|---------|--------|-----------|---------------|
| **State tracking** | Both sides | Device sync state | `.lst` files |
| **Default resolution** | Manual | Rename (older) | Rename both |
| **Auto strategies** | prefer/force | None | newer/larger/priority |
| **Merge tools** | Yes (external) | No | No |
| **Safety limit** | Manual review | None | `--max-delete` |
| **Recovery** | Re-scan | Re-scan | `--recover` |
| **Maturity** | Very mature (20+ yrs) | Mature (10+ yrs) | Beta (2-3 yrs) |

## Design Recommendations for sy

### Phase 1: Simple Bidirectional (Newest-Wins)
**Rationale**: 80% use case, minimal complexity

**Implementation:**
```rust
// Store state in SQLite (reuse existing checksum_db infrastructure)
struct SyncState {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
    checksum: Option<u64>, // xxHash3, optional for speed
}

enum ConflictResolution {
    Newer,    // Winner = most recent mtime (default)
    Larger,   // Winner = largest size
    Smaller,  // Winner = smallest size
    Source,   // Winner = source path (force push)
    Dest,     // Winner = dest path (force pull)
    Ask,      // Prompt user (interactive mode)
    Rename,   // Keep both: file.conflict-<timestamp>-<side>
}
```

**Algorithm:**
1. Load prior state from DB (if exists)
2. Scan both sides (parallel)
3. Classify changes:
   - `new_source`: File exists only in source now (new or dest deleted)
   - `new_dest`: File exists only in dest now (new or source deleted)
   - `modified_source`: Source mtime > prior source mtime
   - `modified_dest`: Dest mtime > prior dest mtime
   - `conflict`: Both modified since prior run
4. Apply resolution strategy for conflicts
5. Sync non-conflicting changes bidirectionally
6. Store new state in DB

**CLI:**
```bash
# Default: newest-wins
sy /local user@host:/remote --bidirectional

# Explicit strategy
sy /local /backup --bidirectional --conflict-resolve newer
sy /local /backup --bidirectional --conflict-resolve rename

# Interactive mode
sy /local /backup --bidirectional --conflict-resolve ask

# Safety limits
sy /local /backup --bidirectional --max-delete 10%
```

**Safety Features:**
- `--max-delete` threshold (abort if >N% deletions detected)
- `--dry-run` shows what would change
- Content equality check before declaring conflict
- State DB stored in `~/.cache/sy/bisync/` with locking

**JSON Output:**
```json
{
  "conflicts": [
    {
      "path": "file.txt",
      "source_mtime": "2025-10-23T10:00:00Z",
      "dest_mtime": "2025-10-23T10:05:00Z",
      "resolution": "newer",
      "winner": "dest"
    }
  ],
  "changes": {
    "source_to_dest": 10,
    "dest_to_source": 5,
    "conflicts_resolved": 2,
    "conflicts_renamed": 0
  }
}
```

### Phase 2: Advanced Features (Future)
**Deferred to v0.1.0+:**
- External merge tools (git-style 3-way merge)
- Vector clocks (for true distributed sync)
- Watch mode integration (continuous bidirectional sync)
- Conflict history tracking

## Complexity Analysis

**Unison approach** (vector clocks, replicas, reconciliation):
- **Complexity**: HIGH (3000+ lines of OCaml for conflict logic)
- **Benefit**: Handles complex multi-device scenarios

**Syncthing approach** (block exchange protocol, device IDs):
- **Complexity**: VERY HIGH (continuous sync, P2P protocol)
- **Benefit**: Real-time sync across many devices

**rclone bisync approach** (snapshot comparison):
- **Complexity**: MEDIUM (~1500 lines for bisync.go)
- **Benefit**: Simple state model, good for batch sync

**sy newest-wins approach** (proposed):
- **Complexity**: LOW (~500 lines estimated)
- **Benefit**: Covers 80% of use cases, leverages existing infrastructure

## Decision: Start with Newest-Wins

**Rationale:**
1. **Existing infrastructure**: sy already has SQLite (checksum_db), just add state table
2. **Common use case**: Laptop ↔ Desktop sync, newest always wins
3. **Low risk**: Can upgrade to full Unison-style later if needed
4. **Fast implementation**: Reuse existing scanner, planner, sync engine

**Tradeoffs:**
- Won't handle complex 3-device scenarios (defer to Unison/Syncthing)
- No merge tools initially (can add later)
- Requires running sy on schedule (not continuous like Syncthing)

**Success metrics:**
- ✅ Detects conflicts accurately (no false positives from equality check)
- ✅ Newest-wins strategy works >95% of time
- ✅ Safety limits prevent data loss
- ✅ Performance similar to unidirectional sync (parallel scanning)

## References

- Unison: https://www.cis.upenn.edu/~bcpierce/unison/
- Syncthing docs: https://docs.syncthing.net/users/syncing.html
- rclone bisync: https://rclone.org/bisync/
- File sync notes: https://helpful.knobs-dials.com/index.php/File_synchronization_notes
