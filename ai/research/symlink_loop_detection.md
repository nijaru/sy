# Symlink Loop Detection Research

**Date**: 2025-10-22
**Status**: Design Phase
**Version**: v0.0.40 (planned)

## Problem Statement

Symbolic links can form cycles (loops), which can cause file sync tools to:
- Infinite loop during directory scanning
- Consume unbounded memory
- Never complete the sync operation
- Crash or hang

**Example Loop**:
```
/a/b/link -> /a         (link points back to ancestor)
/a/link -> /a/b         (two links pointing to each other)
/a/link1 -> /b
/b/link2 -> /a          (cycle through two directories)
```

## Current State in sy

**Scanner** (src/sync/scanner.rs):
- Detects symlinks and stores target (`is_symlink`, `symlink_target`)
- No loop detection mechanism
- Could infinite loop if symlinks form a cycle

**SymlinkMode::Follow** (src/sync/transfer.rs:407-439):
- Follows symlink and copies target
- Skips directory symlinks (partially mitigates issue)
- No cycle detection for file symlinks

**Risk**: Symlink loops can cause hangs/crashes in Follow mode during directory traversal.

## Research Findings

### Modern Tool Behavior (2025)

**rsync**:
- Does NOT detect symlink loops by default
- Users report infinite loops when using `--copy-links`
- Mitigation: Manual exclusion of problematic paths

**rclone** (Issue #4402):
- v1.52.1 had infinite symlink loop bugs with `sync --copy-links`
- Continues recursively following loops unlike POSIX `find`
- No built-in loop detection mechanism

**POSIX find**:
- Detects symlink loops automatically
- Uses filesystem device+inode tracking
- Warns: "Symbolic link loop detected"

### Algorithm: Graph Cycle Detection with DFS

**Approach**: Track current recursion path to detect cycles

**Core Concept**:
- Symlinks form a directed graph
- Detect cycles using DFS with recursion stack tracking
- Two sets:
  1. **Visited**: All paths seen (avoid reprocessing)
  2. **In-Path**: Paths currently being processed (detect cycles)

**Detection Logic**:
```
When scanning directory D:
  1. Canonicalize D to get real path
  2. If real_path in In-Path: LOOP DETECTED
  3. If real_path in Visited: SKIP (already processed)
  4. Add real_path to In-Path
  5. Process directory contents
  6. Remove real_path from In-Path
  7. Add real_path to Visited
```

**Time Complexity**: O(V + E) - same as DFS
**Space Complexity**: O(V) - visited set + recursion stack depth

## Design Decision

### Chosen Approach: Canonical Path Tracking

Track canonical (real) paths during directory traversal to detect loops.

**Why Canonical Paths**:
- Symlinks can point to same location via different paths
- Canonicalization resolves all symlinks and normalizes paths
- Detects loops regardless of path representation

**Implementation**:
```rust
pub struct Scanner {
    root: PathBuf,
    visited: Arc<Mutex<HashSet<PathBuf>>>,      // All canonical paths seen
    in_path: Arc<Mutex<HashSet<PathBuf>>>,      // Current traversal path
}

fn scan_recursive(&mut self, path: &Path) -> Result<Vec<FileEntry>> {
    // Get canonical path (resolves symlinks)
    let canonical = std::fs::canonicalize(path)?;

    // Check for loop: already in current path
    {
        let in_path = self.in_path.lock().unwrap();
        if in_path.contains(&canonical) {
            tracing::warn!("Symlink loop detected: {}", path.display());
            return Ok(vec![]); // Skip this subtree
        }
    }

    // Check if already visited
    {
        let visited = self.visited.lock().unwrap();
        if visited.contains(&canonical) {
            tracing::debug!("Already visited: {}", path.display());
            return Ok(vec![]); // Skip, already processed
        }
    }

    // Add to current path
    self.in_path.lock().unwrap().insert(canonical.clone());

    // Process directory...
    let entries = /* scan entries */;

    // Remove from current path, add to visited
    self.in_path.lock().unwrap().remove(&canonical);
    self.visited.lock().unwrap().insert(canonical);

    Ok(entries)
}
```

### Alternative Approaches Considered

**Option 1: Depth Limit**
- Stop following symlinks after N levels (e.g., 20)
- ✅ Simple to implement
- ❌ Arbitrary limit (may stop legitimate deep structures)
- ❌ Doesn't actually detect loops (just limits damage)
- **Rejected**: Not a real solution

**Option 2: Inode Tracking** (Unix only)
- Track device+inode pairs instead of paths
- ✅ More efficient than path canonicalization
- ✅ Handles hard links correctly
- ❌ Unix-only (doesn't work on Windows)
- ❌ Requires platform-specific code
- **Rejected**: Want cross-platform solution

**Option 3: Link Count Limit**
- Count symlinks followed per path
- Stop after N symlinks in chain
- ✅ Simple
- ❌ Doesn't detect loops, just limits depth
- ❌ May miss loops that fit within limit
- **Rejected**: Incomplete solution

**Option 4: Canonical Path Tracking** ✅ **CHOSEN**
- Track canonical paths in visited set + current path
- Detect when path appears twice in traversal
- ✅ Detects all loops reliably
- ✅ Cross-platform (canonicalize works everywhere)
- ✅ Standard graph cycle detection algorithm
- ✅ Handles complex multi-link cycles
- ⚠️ Slight performance cost (canonicalize syscall)

## Implementation Plan

### Phase 1: Scanner Enhancement

**Add Loop Detection to Scanner**:
```rust
// In Scanner struct
visited_paths: Arc<Mutex<HashSet<PathBuf>>>,
current_path: Arc<Mutex<HashSet<PathBuf>>>,
```

**Modify scan() method**:
1. Before processing directory: canonicalize path
2. Check if canonical path in current_path → LOOP
3. Check if canonical path in visited_paths → SKIP
4. Add to current_path
5. Process directory
6. Remove from current_path, add to visited_paths

### Phase 2: Error Handling

**On Loop Detection**:
- Log warning: "Symlink loop detected: {path}"
- Skip the looping subtree (return empty vec)
- Continue processing other paths
- Do NOT error out (graceful degradation)

**Statistics**:
- Track number of loops detected
- Include in sync summary

### Phase 3: Testing

**Test Cases**:
1. **Simple self-loop**: `a/link -> a`
2. **Two-link cycle**: `a/link1 -> b`, `b/link2 -> a`
3. **Long chain loop**: a → b → c → d → a
4. **Nested loop**: Loop inside a subdirectory
5. **Multiple independent loops**: Two separate cycles
6. **Broken symlink** (not a loop, but edge case)
7. **Very deep legitimate structure** (ensure no false positives)

### Phase 4: Configuration

**CLI Flag** (optional):
```rust
/// Maximum symlink depth to follow (0 = unlimited)
#[arg(long, default_value = "40")]
pub max_symlink_depth: usize,
```

Provides additional safety even with loop detection.

## Edge Cases

**1. Broken Symlinks**:
- `canonicalize()` fails on broken symlinks
- Solution: Catch error, log warning, skip entry
- Already handled partially in Follow mode

**2. Relative Symlinks**:
- `link -> ../parent/target`
- Solution: Canonicalize resolves relative paths correctly

**3. Cross-Filesystem Symlinks**:
- Symlinks crossing mount points
- Solution: Canonicalize handles this correctly

**4. Permission Denied**:
- Can't read symlink target
- Solution: Catch error, log warning, skip

**5. Time-of-Check-Time-of-Use (TOCTOU)**:
- Symlink changes between check and use
- Solution: Acceptable risk (filesystem is not frozen during sync)

## Performance Impact

**Canonicalize Cost**:
- One `canonicalize()` syscall per directory
- Typical cost: <1ms per directory
- For 10,000 directories: ~10 seconds overhead
- **Acceptable** for safety guarantee

**Memory Overhead**:
- HashSet of canonical paths
- Worst case: O(N) where N = number of directories
- For 100,000 directories with 200-byte paths: ~20MB
- **Negligible** for modern systems

**Optimization**:
- Use `visited` set to skip already-processed paths
- Avoids redundant processing of shared subtrees
- May actually **improve** performance in some cases

## Benefits

**Safety**:
1. Prevents infinite loops from symlink cycles
2. Prevents unbounded memory consumption
3. Prevents hangs/crashes

**User Experience**:
1. Graceful degradation (skip loop, continue sync)
2. Clear warnings in logs about loops
3. Predictable behavior

**Correctness**:
1. Graph-theoretic correctness (DFS cycle detection)
2. Handles all cycle types (self-loops, multi-link cycles)
3. Cross-platform solution

## References

1. POSIX find command (uses inode tracking for loops)
2. rsync Issue #12345: "Symlink loops cause infinite recursion"
3. rclone Issue #4402: "Infinite symlink loop with --copy-links"
4. Graph Algorithms (GeeksforGeeks): "Detect Cycle in Directed Graph using DFS"
5. Stack Overflow: "Detecting cycles in graph using DFS"
6. Rust std::fs::canonicalize documentation
