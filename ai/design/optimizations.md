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

**Impact:** Variable (5-50ms) | **Effort:** Medium | **Status:** DEFERRED

### Problem

Currently we stat every file during scan, even if the entire directory is unchanged.

### Why Deferred

After analysis, this optimization has fundamental issues:

1. **Dir mtime only reflects entry changes** - modifying file content does NOT update parent dir mtime
2. **Only detects adds/removes** - not content changes, which is the common case
3. **Complexity vs benefit is poor** - requires persistent state tracking, race condition handling
4. **Marginal benefit** - scanning 5000 files on SSD is ~100ms, optimization would save ~10-20ms at most

### What Would Be Needed

1. Persistent cache of last-sync dir mtimes
2. Protocol extension to compare dir mtimes
3. Careful handling of edge cases (file modified but dir mtime unchanged)

### Recommendation

Defer unless profiling shows scan overhead is the bottleneck. The message batching optimization addresses the main syscall overhead.

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
