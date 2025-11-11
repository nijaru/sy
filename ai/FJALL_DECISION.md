# Fjall vs Rusqlite Decision

**Date**: 2025-11-11  
**Status**: DECIDED - Keep fjall  
**Branch**: perf/checksumdb-comparison

## Question

After evaluating pure Rust dependency migrations (fjall, object_store, russh), we questioned whether fjall was worth keeping if we weren't pursuing a "pure Rust" philosophy.

## Benchmark Results

Created `benches/checksumdb_bench.rs` to measure write and read performance on 1,000 checksums:

**Fjall (LSM-tree)**:
- Write: 340.17 ms
- Read: 256.66 ms

**Rusqlite (SQLite)**:
- Write: 533.54 ms (56.8% SLOWER than fjall)
- Read: 5.75 ms (97.8% FASTER than fjall)

## Analysis

- **Write-heavy workload**: Checksum cache is written during sync, read only when metadata matches (rare)
- **Fjall wins overall**: LSM-trees are optimized for sequential writes; SQLite's advantage in reads doesn't matter here
- **The gap is significant**: 56% slower writes is material for large syncs

## Decision

**Keep fjall**. The write performance advantage is real and measurable. SQLite's read speed is not a bottleneck because:
1. Database is queried only when file metadata (mtime + size) matches cached entry
2. In real workflows, this is a relatively rare path
3. Network/disk I/O dominates, not database reads

## Bonus

This validates that pure Rust migrations can deliver real benefits beyond philosophy:
- fjall (56% faster writes) ✅
- object_store (multi-cloud support, simpler API) ✅
- russh (seamless SSH + pure Rust) ❌ - performance not critical, but agent auth blocker remains

## Next

Move forward with v0.0.58 using fjall + object_store + ssh2 (not russh).
