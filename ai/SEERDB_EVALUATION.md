# seerdb vs fjall Evaluation

**Date**: 2025-11-11  
**Status**: EVALUATED - seerdb is significantly faster but requires nightly Rust  
**Branch**: feat/seerdb-evaluation

## Benchmark Results

Checksumdb workload: 1,000 write/read operations on cryptographic checksums (32-byte hash values)

**fjall (LSM-tree, 2.11.2)**:
- Write: 328-342 ms
- Read: 256-258 ms

**seerdb (Research-grade LSM, 0.0.0)**:
- Write: 18.0-18.4 ms (18.2x faster than fjall)
- Read: 6.3-8.5 ms (30-43x faster than fjall)

## Why seerdb is so much faster

1. **Learned indexes (ALEX)**: Replaces binary search with adaptive learned model for faster key lookups
2. **Workload-aware compaction (Dostoevsky)**: Optimizes compaction strategy based on read/write ratio
3. **Key-value separation (WiscKey)**: Separates small keys from large values to reduce write amplification
4. **Lock-free structures**: Concurrent skiplist memtable with arc-swap for non-blocking reads
5. **SIMD optimizations**: Hardware-accelerated key comparisons
6. **Modern dependencies**: foldhash (2x faster than xxhash on small keys), jemalloc, lz4_flex

seerdb is research-grade with 2018-2024 innovations, while fjall is a solid production-ready LSM (2024).

## Decision

**Cannot adopt seerdb for checksumdb integration:**

1. **Nightly-only**: Requires Rust nightly for `std::simd`. This creates:
   - Deployment complexity (nightly instability)
   - CI/release pipeline issues
   - Potential compatibility issues with stable toolchains
   
2. **Experimental status**: seerdb README states "Not recommended for production use"
   - No proven production track record
   - Checksum cache is durability-critical (data loss = re-hashing entire sync)
   
3. **Architectural mismatch**: seerdb's advantages are for large-scale workloads (100K+ ops). Checksumdb is small (~10K checksums per typical sync)
   - 18ms per 1K writes still passes through network/disk bottlenecks
   - Not a bottleneck in real-world sync performance

## Alternative: Keep fjall as primary, consider seerdb for optional high-scale cache

If sy ever supports multi-TB syncs with millions of files:
- Add optional `checksumdb-seerdb` feature flag
- Enable only on nightly builds
- Document: "Experimental, faster checksumdb on nightly builds"

For now: fjall is the right choice—production-ready, stable, sufficient performance for checksumdb's role.

## Bonus insight

This validates the evaluation framework: pure Rust doesn't automatically win on performance.
- seerdb (pure Rust, nightly, experimental) ✅ much faster, but not viable here
- fjall (pure Rust, stable, production) ✅ production-ready, good enough
- The decision depends on stability requirements, not ideology

## Benchmark Setup

Created `benches/seerdb_comparison_bench.rs` with feature flag `seerdb-bench` to:
- Run without seerdb on stable (fjall only)
- Run with seerdb on nightly (both)
- Keep comparison data separate from main codebase

Run with seerdb:
```bash
rustup override set nightly
cargo bench --features seerdb-bench --bench seerdb_comparison_bench
```

Run fjall-only (no nightly needed):
```bash
rustup override set stable
cargo bench --bench seerdb_comparison_bench
```
