# Phase 5: Verification Enhancements Design

**Date**: 2025-10-21
**Status**: Design
**Version**: v0.0.35 (target)

## Overview

Enhance sy's integrity verification with pre-transfer checksums, persistent checksum database, and verify-only mode.

## Motivation

**Current Limitations:**
1. **Wasted Bandwidth**: Checksums computed during/after transfer - if verification fails, bandwidth already spent
2. **No Bit Rot Detection**: Can't detect file corruption when mtime unchanged
3. **Slow `--checksum` Mode**: No caching means recomputing checksums every sync (like rsync)
4. **No Audit Mode**: Can't verify existing files without syncing

**User Impact:**
- Large file transfers fail verification → wasted time and bandwidth
- Corruption detection requires full re-sync
- `--checksum` mode too slow for regular use
- No way to audit backup integrity

## Design

### 1. Pre-Transfer Checksums

**Goal**: Compute checksums before transfer to detect identical files and save bandwidth.

**Implementation:**

```rust
// New field in SyncTask
pub struct SyncTask {
    // ... existing fields ...
    pub source_checksum: Option<Checksum>,  // Pre-computed source checksum
    pub dest_checksum: Option<Checksum>,    // Pre-computed dest checksum (if exists)
}
```

**CLI Integration:**
- `--checksum` / `-c`: Always compare checksums (rsync compatibility)
- Behavior: Compute checksums in planning phase, skip transfer if match

**Algorithm:**
1. **Scanning Phase**: Collect file metadata (existing)
2. **Checksum Phase** (new, if `--checksum` enabled):
   - For each file in scan results:
     - Compute source checksum
     - If dest exists, compute dest checksum
     - Store in SyncTask
3. **Planning Phase** (enhanced):
   - Compare checksums if available
   - Skip transfer if checksums match
   - Mark as "needs transfer" if checksums differ

**Transport Coordination:**
- **Local→Local**: Both checksums computed locally (fast)
- **Local→Remote**: Source local, request dest checksum via SSH
- **Remote→Local**: Request source checksum via SSH, dest local
- **Remote→Remote**: Request both checksums via SSH

**Protocol Extension:**
Add `sy-remote` command for checksum computation:
```bash
sy-remote checksum /path/to/file [--type fast|cryptographic]
# Output: <hex checksum>
```

**Benefits:**
- ✅ Save bandwidth for identical files (mtime changed but content same)
- ✅ Detect bit rot (content changed but mtime same)
- ✅ Better progress reporting (know transfer size before starting)
- ✅ Dry-run shows checksum comparison

**Tradeoffs:**
- ⚠️ Adds checksum computation time before transfer (mitigated by using Fast mode)
- ⚠️ Remote checksums require round-trip SSH calls (parallelize)

**Performance:**
- xxHash3 (Fast mode): ~15 GB/s (minimal overhead)
- BLAKE3 (Cryptographic): ~3 GB/s (noticeable overhead)
- Parallelize checksum computation across files

---

### 2. Checksum Database

**Goal**: Persistent storage of checksums to avoid recomputation on every sync.

**Implementation:**

```rust
// New module: src/sync/checksumdb.rs

use rusqlite::{Connection, params};

pub struct ChecksumDatabase {
    conn: Connection,
}

impl ChecksumDatabase {
    pub fn open(path: &Path) -> Result<Self>;

    /// Get cached checksum if file unchanged (mtime + size match)
    pub fn get_checksum(&self, path: &Path, mtime: SystemTime, size: u64) -> Result<Option<Checksum>>;

    /// Store checksum after successful transfer
    pub fn store_checksum(&self, path: &Path, mtime: SystemTime, size: u64, checksum: Checksum) -> Result<()>;

    /// Clear all cached checksums
    pub fn clear(&self) -> Result<()>;

    /// Remove checksums for files that no longer exist
    pub fn prune(&self, existing_files: &HashSet<PathBuf>) -> Result<()>;
}
```

**Schema:**
```sql
CREATE TABLE checksums (
    path TEXT PRIMARY KEY,
    mtime_secs INTEGER NOT NULL,
    mtime_nanos INTEGER NOT NULL,
    size INTEGER NOT NULL,
    checksum_type TEXT NOT NULL,  -- 'fast' or 'cryptographic'
    checksum BLOB NOT NULL,
    updated_at INTEGER NOT NULL  -- Unix timestamp of when checksum was computed
);

CREATE INDEX idx_updated_at ON checksums(updated_at);
```

**CLI Flags:**
- `--checksum-db`: Enable checksum database (default: disabled)
- `--no-checksum-db`: Explicitly disable
- `--clear-checksum-db`: Clear database and rebuild
- `--prune-checksum-db`: Remove entries for deleted files

**Location:**
- Destination directory: `.sy-checksums.db`
- Separate from directory cache (`.sy-dir-cache.json`)
- SQLite for reliability and query flexibility

**Integration Points:**

1. **During Scanning**:
   ```rust
   // If --checksum-db enabled and file unchanged (mtime + size match)
   if let Some(cached) = checksumdb.get_checksum(path, mtime, size)? {
       task.source_checksum = Some(cached);
       // Skip checksum computation
   }
   ```

2. **After Transfer**:
   ```rust
   // Store computed checksum
   checksumdb.store_checksum(path, mtime, size, checksum)?;
   ```

3. **Pruning** (optional, on `--prune-checksum-db`):
   ```rust
   // Remove entries for files that no longer exist
   let existing: HashSet<_> = scan_results.iter().map(|f| f.path).collect();
   checksumdb.prune(&existing)?;
   ```

**Benefits:**
- ✅ Dramatically faster `--checksum` mode (only compute checksums for changed files)
- ✅ Historical integrity data (detect when file was last verified)
- ✅ Incremental verification (verify subset of files per run)
- ✅ Bit rot detection over time

**Tradeoffs:**
- ⚠️ Adds SQLite dependency
- ⚠️ Disk space for database (~40 bytes per file + checksum size)
- ⚠️ Database can become stale if files modified outside sy

**Performance:**
- SQLite query: <1ms per file
- Database size: ~100 bytes per file (path + metadata + checksum)
- Example: 100K files = ~10MB database

---

### 3. --verify-only Mode

**Goal**: Verify existing files without syncing (audit mode).

**Implementation:**

```rust
// CLI flag
#[arg(long)]
pub verify_only: bool,
```

**Behavior:**

1. **Scan both source and destination** (existing logic)
2. **Compute checksums for all files** (both source and dest)
3. **Compare and report** (no transfers):
   - ✅ **Identical**: Checksums match
   - ⚠️ **Different**: Checksums differ (corruption or modification)
   - ❌ **Missing from dest**: File only in source
   - ❌ **Missing from source**: File only in dest (if `--delete` not set)
4. **Exit code**:
   - `0`: All files identical
   - `1`: Any differences found

**Output Format:**

**Human-readable:**
```
Verifying files...

✅ Identical:           1,234 files (10.5 GB)
⚠️  Different:              5 files (1.2 MB)
❌ Missing from dest:      12 files (3.4 MB)
❌ Extra in dest:           3 files (500 KB)

Verification summary:
  Total files checked: 1,254
  Matches: 1,234 (98.4%)
  Differences: 20 (1.6%)

Exit code: 1 (differences found)
```

**JSON output** (`--json --verify-only`):
```json
{"type":"verify_start","total_files":1254}
{"type":"identical","path":"/src/file1.txt","size":1024,"checksum":"abc123..."}
{"type":"different","path":"/src/file2.txt","source_checksum":"def456...","dest_checksum":"789ghi..."}
{"type":"missing_dest","path":"/src/file3.txt","size":2048}
{"type":"extra_dest","path":"/dest/file4.txt","size":512}
{"type":"verify_summary","identical":1234,"different":5,"missing_dest":12,"extra_dest":3,"exit_code":1}
```

**Use Cases:**
- **Backup verification**: `sy /backup /original --verify-only`
- **Bit rot detection**: Periodic audits without touching files
- **Integrity auditing**: CI/CD verification of deployed files
- **Differential analysis**: Find what changed between snapshots

**Integration with Other Flags:**

- ✅ `--checksum-db`: Use cached checksums to speed up verification
- ✅ `--mode verify`: Use BLAKE3 for cryptographic guarantees
- ✅ `--mode fast`: Use size+mtime only (fast but less reliable)
- ✅ `--filter`, `--exclude`: Apply filters to verification scope
- ✅ `--dry-run`: Show what would be verified (no-op)
- ❌ `--delete`, `--resume`, `--watch`: Incompatible (error)

**Benefits:**
- ✅ Audit file integrity without risk of modification
- ✅ Detect bit rot over time
- ✅ Scriptable (JSON output + exit codes)
- ✅ Fast with `--checksum-db`

**Tradeoffs:**
- ⚠️ Slower than size+mtime comparison (must compute checksums)
- ⚠️ Reads all file content (I/O intensive)

---

## Implementation Plan

### Phase 5a: Pre-Transfer Checksums (v0.0.35)

**Tasks:**
1. Add `source_checksum` and `dest_checksum` to `SyncTask`
2. Implement checksum computation phase in scanner
3. Add `--checksum` CLI flag (rsync compatibility)
4. Extend `sy-remote` with `checksum` command
5. Update planner to skip transfer if checksums match
6. Update dry-run to show checksum status
7. Add tests for pre-transfer checksum comparison

**Estimated Time**: 1-2 days

**Testing:**
- Local→Local with matching checksums (skip transfer)
- Local→Local with different checksums (transfer)
- Remote checksum computation via SSH
- Parallel checksum computation performance

---

### Phase 5b: Checksum Database (v0.0.36)

**Tasks:**
1. Add `rusqlite` dependency
2. Implement `ChecksumDatabase` in `src/sync/checksumdb.rs`
3. Add CLI flags: `--checksum-db`, `--clear-checksum-db`, `--prune-checksum-db`
4. Integrate with scanner (load cached checksums)
5. Integrate with transfer (store checksums after success)
6. Add pruning logic for deleted files
7. Add tests for database operations and caching

**Estimated Time**: 2-3 days

**Testing:**
- Cache hit/miss scenarios
- Database pruning correctness
- Stale cache handling (mtime/size changed)
- Database corruption recovery

---

### Phase 5c: Verify-Only Mode (v0.0.37)

**Tasks:**
1. Add `--verify-only` CLI flag
2. Implement verification-only logic (scan, checksum, compare, report)
3. Add verification result types (identical, different, missing, extra)
4. Implement human-readable and JSON output
5. Add exit code handling
6. Validate flag compatibility (error on `--delete`, etc.)
7. Add comprehensive tests for verification scenarios

**Estimated Time**: 2-3 days

**Testing:**
- Identical files (exit 0)
- Different files (exit 1)
- Missing files (exit 1)
- JSON output format
- Integration with `--checksum-db`

---

## Dependencies

**New Crate:**
- `rusqlite = "0.30"` (SQLite database)

**Existing:**
- `xxhash-rust` (Fast checksums)
- `blake3` (Cryptographic checksums)
- All integrity infrastructure already exists

---

## Performance Considerations

### Pre-Transfer Checksums
- **Overhead**: xxHash3 @ ~15 GB/s → minimal for SSDs
- **Mitigation**: Parallel checksum computation (rayon)
- **Best case**: Save 100% of transfer time for identical files
- **Worst case**: Add ~5% overhead for files needing transfer

### Checksum Database
- **Database overhead**: <1ms per query (SQLite is fast)
- **Disk space**: ~100 bytes per file
- **Best case**: 100x speedup for `--checksum` re-syncs
- **Worst case**: Minimal overhead when not using `--checksum-db`

### Verify-Only Mode
- **I/O**: Must read all file content (same as sync)
- **Mitigation**: Use `--checksum-db` to skip unchanged files
- **Parallelism**: Already have parallel file processing

---

## Security Considerations

### Checksum Database
- **Tampering**: Attacker with file access can modify both file and database
  - Not a security boundary (use BLAKE3 + `--mode verify` for cryptographic guarantees)
  - Database is optimization, not security feature
- **Stale data**: If file modified outside sy, database becomes stale
  - Mitigated by mtime+size check before using cached checksum

### Verify-Only Mode
- **TOCTOU**: File could change between checksum and verification
  - Document limitation (use `--mode paranoid` for atomic verification)
- **False positives**: Size+mtime can give false "identical" (use `--checksum` for accuracy)

---

## Alternatives Considered

### 1. Store Checksums in Directory Cache
**Rejected**: Directory cache is for metadata (mtime, size, is_dir). Checksums are optional and potentially expensive. Separate database allows:
- Independent caching strategies
- SQLite for complex queries (e.g., "find all files verified before date X")
- Optional feature (--checksum-db flag)

### 2. JSON Database Instead of SQLite
**Rejected**: JSON doesn't scale to 100K+ files:
- Linear scan for lookups (slow)
- Must load entire database into memory
- No ACID guarantees
SQLite provides:
- O(log n) lookups via B-tree index
- Transactions and durability
- Minimal memory footprint

### 3. Compute Checksums On-Demand (No Database)
**Rejected**: rsync's `--checksum` mode is unusably slow for large datasets because it recomputes all checksums every sync. Database enables:
- Fast re-syncs with `--checksum` (only compute for changed files)
- Historical integrity tracking
- Incremental verification

---

## Success Metrics

### Pre-Transfer Checksums (v0.0.35)
- ✅ Skip transfer for identical files (mtime changed but content same)
- ✅ Detect bit rot (mtime same but content different)
- ✅ Dry-run shows checksum comparison

### Checksum Database (v0.0.36)
- ✅ `--checksum` mode 10-100x faster on re-syncs
- ✅ Database handles 100K+ files efficiently
- ✅ Pruning keeps database size bounded

### Verify-Only Mode (v0.0.37)
- ✅ Audit file integrity without modification
- ✅ Scriptable (JSON + exit codes)
- ✅ Fast with `--checksum-db` integration

---

## Documentation Updates

1. **README.md**:
   - Add Phase 5 features to feature list
   - Add examples for `--checksum`, `--checksum-db`, `--verify-only`
   - Update verification section

2. **TROUBLESHOOTING.md**:
   - Add section on checksum database (location, clearing, pruning)
   - Add verify-only mode examples

3. **CLI Help** (`--help`):
   - Document new flags with examples
   - Clarify verification modes

---

## Questions for User

1. **Checksum Database Default**: Should `--checksum-db` be opt-in or opt-out?
   - **Recommendation**: Opt-in (default OFF) for v0.0.36, consider opt-out (default ON) for v1.0 after testing

2. **Database Location**: Always `.sy-checksums.db` in destination, or configurable?
   - **Recommendation**: Fixed location (simpler), add `--checksum-db-path` in v1.0+ if requested

3. **Pruning Strategy**: Auto-prune on every sync, or manual `--prune-checksum-db` only?
   - **Recommendation**: Manual only (explicit, predictable)

4. **Checksum Type in Database**: Store both Fast and Cryptographic, or only one?
   - **Recommendation**: Store both if computed (allows switching verification modes)

---

## References

- **rsync `--checksum`**: Always compares checksums, slow (no caching)
- **rclone `--checksum`**: Similar to rsync, no persistent cache
- **git reflog**: Inspiration for historical integrity tracking
- **SQLite performance**: <https://www.sqlite.org/speed.html>

---

**Next Steps:**
1. Review design with user
2. Implement Phase 5a (Pre-Transfer Checksums)
3. Test and iterate
4. Proceed to Phase 5b and 5c
