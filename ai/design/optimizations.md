# Performance Optimizations

## Overview

After benchmarking (2026-01-19), sy has ~50ms fixed overhead vs rsync on SSH incremental syncs. This doc outlines quick-win optimizations.

## Optimization 1: Message Batching

**Impact:** ~5ms | **Effort:** Low

### Problem

Current `scan_dest()` flow:

```
for each file:
    encode DestFileEntry → channel.send() → writer.write_all()
```

For 5000 files = 5000 channel sends + 5000 write syscalls.

### Solution

Batch multiple encoded frames into a single buffer before sending:

```rust
// receiver.rs
const BATCH_SIZE: usize = 64 * 1024;  // 64KB batches

pub async fn scan_dest<F>(&self, mut on_entry: F) -> Result<(u64, u64)>
where
    F: FnMut(Bytes) -> Result<()>,
{
    let mut batch = BytesMut::with_capacity(BATCH_SIZE);

    for entry in entries {
        let encoded = dest_entry.encode();
        batch.extend_from_slice(&encoded);

        if batch.len() >= BATCH_SIZE {
            on_entry(batch.split().freeze())?;
        }
    }

    // Flush remaining
    if !batch.is_empty() {
        on_entry(batch.freeze())?;
    }

    // Send DEST_FILE_END (not batched)
    on_entry(end.encode())?;
}
```

### Files to Modify

1. `src/streaming/receiver.rs` - `scan_dest()` method

### Expected Impact

- 5000 files → ~80 batches (vs 5000 sends)
- Fewer syscalls, better TCP utilization
- ~5ms savings on file list exchange

## Optimization 2: Directory mtime Cache

**Impact:** Variable (5-50ms) | **Effort:** Medium

### Problem

Currently we stat every file during scan, even if the entire directory is unchanged.

### Solution

Track directory mtimes. If dir mtime unchanged since last sync, skip scanning that subtree.

```rust
// During Initial Exchange, server sends dir mtimes
// Client compares with local dir mtimes
// If match, skip children from scan
```

### Complexity

1. Dir mtime semantics vary by filesystem:
   - APFS/ext4: mtime updates when directory entries change
   - Some FSes: mtime may not update on nested changes

2. Requires tracking state between syncs (either in protocol or local cache)

3. Race condition: dir mtime could match but file changed right before sync

### Implementation Approach

Phase 1 (within protocol):

- Include dir mtime in DEST_FILE_ENTRY with DIR flag
- Generator checks if source dir mtime matches
- If match AND no new/deleted files expected, skip individual file checks

Phase 2 (persistent cache):

- Store last-sync dir mtimes locally
- Skip sending unchanged subtrees entirely

### Files to Modify

1. `src/streaming/protocol.rs` - Add dir mtime to DestFileEntry
2. `src/streaming/generator.rs` - Check dir mtimes before scanning
3. `src/sync/scanner.rs` - Add option to skip directories

### Deferred

This optimization is more complex than message batching. Consider after batching proves insufficient.

## Optimization 3: Daemon Mode

**Impact:** ~30ms | **Effort:** High

**Status:** Deferred - significant architecture change.

Keep `sy --server` running to eliminate spawn overhead. Would require:

- Daemon lifecycle management
- Connection multiplexing
- State management across syncs

## Recommendation

1. **Implement message batching first** - low effort, measurable impact
2. **Re-benchmark** to quantify gains
3. **Consider dir mtime cache** if more optimization needed
4. **Defer daemon mode** unless 30ms matters for specific use cases

## Measurements

Before optimization:

- SSH incremental (5000 files): sy ~parity with rsync
- SSH incremental (large file): rsync 1.6x faster

Target after batching:

- Reduce file list exchange by ~5ms
- Close parity gap slightly

---

**Created:** 2026-01-19
