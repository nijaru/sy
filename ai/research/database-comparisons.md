# Database Comparisons for sy

## fjall vs rusqlite (Checksumdb Write-Heavy Workload)

**Date**: 2025-11-10  
**Benchmark**: benches/checksumdb_bench.rs (1,000 checksums)

### Results

| Operation | fjall | rusqlite | Ratio |
|-----------|-------|----------|-------|
| Write 1K checksums | 340.17 ms | 533.54 ms | 0.637x (rusqlite 56.8% slower) |
| Read 1K checksums | 256.66 ms | 5.75 ms | 44.6x (rusqlite faster) |

### Analysis

**Checksum cache is write-heavy**:
- Written during sync (every file transferred)
- Read only when file metadata (mtime + size) matches cached entry
- In real workflows, this is a relatively rare path (network/disk I/O dominates)

**fjall wins overall**:
- LSM-trees are optimized for sequential writes
- SQLite's read advantage (44.6x) doesn't matter because reads are infrequent
- 56% write improvement is measurable for large syncs (millions of files)

### Conclusion

Keep fjall for checksumdb. The write performance advantage is real and measurable for production workloads, while read performance isn't a bottleneck.

---

## fjall vs seerdb (Research LSM Comparison)

**Date**: 2025-11-11  
**Branch**: feat/seerdb-evaluation  
**Benchmark**: benches/seerdb_comparison_bench.rs (1K operations, stable and nightly Rust)

### Results

| Operation | fjall | seerdb | Speedup |
|-----------|-------|--------|---------|
| Write 1K | 328-342 ms | 18.0-18.4 ms | 18.2x |
| Read 1K | 256-258 ms | 6.3-8.5 ms | 30-43x |

### Why seerdb is so much faster

1. **Learned indexes (ALEX)**: Replaces binary search with adaptive learned model
2. **Workload-aware compaction (Dostoevsky)**: Optimizes strategy based on read/write ratio
3. **Key-value separation (WiscKey)**: Separates keys from values to reduce write amplification
4. **Lock-free structures**: Concurrent skiplist memtable with atomic swap for non-blocking reads
5. **SIMD optimizations**: Hardware-accelerated key comparisons (std::simd)
6. **Modern dependencies**: foldhash (2x faster than xxhash on small keys), jemalloc, lz4_flex

seerdb implements 2018-2024 research papers vs fjall's production-focused 2024 approach.

### Why rejected despite massive speedup

1. **Nightly-only**:
   - Requires `#![feature(portable_simd)]`
   - Unstable std::simd API can break between nightly releases
   - Creates deployment/CI complexity
   - Incompatible with stable toolchains

2. **Not production-ready**:
   - README: "Experimental, not recommended for production use"
   - Checksumdb is durability-critical (data loss = entire re-hash of sync)
   - No proven track record in production systems

3. **Workload mismatch**:
   - seerdb's advantages shine at 100K+ ops (research benchmarks)
   - Checksumdb: typical sync has ~10K checksums, operations on-demand
   - 18ms per 1K writes still passes through network/disk bottlenecks
   - Real sync performance limited by I/O, not checksumdb latency
   - Typical sync: 18ms overhead is <0.1% impact

### Benchmarking note

Created `benches/seerdb_comparison_bench.rs` with feature flag `seerdb-bench` to:
- Run without seerdb on stable (fjall only)
- Run with seerdb on nightly (both)
- Keep evaluation separate from main codebase

Run with seerdb:
```bash
rustup override set nightly
cargo bench --features seerdb-bench --bench seerdb_comparison_bench
```

Run fjall-only (no nightly needed):
```bash
cargo bench --bench seerdb_comparison_bench
```

### Future consideration

If sy ever supports multi-TB syncs with millions of files:
- Performance profile might change (billions of checksumdb queries)
- Consider optional `checksumdb-seerdb` feature for nightly builds
- Document clearly: "Experimental, faster checksumdb on nightly Rust"
- Revisit when seerdb reaches 0.1.0+ with production track record

---

## russh vs ssh2-rs (SSH Implementation)

**Date**: 2025-11-11  
**Status**: russh rejected, ssh2-rs selected

### Analysis

| Aspect | russh (pure Rust) | ssh2-rs (sys binding) |
|--------|-------------------|----------------------|
| Pure Rust | ✅ Yes | ❌ No (OpenSSH binding) |
| SSH agent auth | ❌ Blocker: needs custom protocol | ✅ Full support |
| Performance | Unknown (newer code) | ✅ Proven (mature) |
| Maintenance | Moderate | ✅ OpenSSL project |

### SSH Agent Blocker

russh doesn't support SSH agent authentication out-of-the-box. Implementation requires:
- Challenge-response handshake protocol (custom 200-300 LOC)
- Manual signing of challenge responses
- Testing with real SSH agents (ssh-agent, GPG agent, KMS agents)

This is a hard architectural requirement for sy:
- Users need seamless auth without password prompts
- SSH agent is the standard solution (works with hardware keys, KMS services)
- Pure Rust appeal doesn't justify reimplementing SSH agent protocol

### Conclusion

Use ssh2-rs (sys binding to OpenSSL). Performance and compatibility are proven. SSH implementation is critical infrastructure—better to use battle-tested code than pure Rust if it sacrifices security or functionality.

---

## Decision Summary

| Database | Decision | Reason |
|----------|----------|--------|
| **fjall** | KEEP | 56% faster than SQLite on writes, production-ready, stable |
| **seerdb** | REJECT | Nightly-only, experimental, workload mismatch |
| **russh** | REJECT | SSH agent auth blocker, pure Rust not worth architectural compromise |
| **object_store** | KEEP (optional) | Multi-cloud support, cleaner than vendor-specific APIs |

All evaluations judge on **performance, functionality, and production viability**—not ideology.
