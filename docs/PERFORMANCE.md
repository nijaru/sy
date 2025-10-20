# Performance Testing & Regression Tracking

This document describes the performance testing strategy for `sy` and how to track performance regressions.

## Overview

Performance is critical for a file sync tool. We track performance at three levels:

1. **Performance Regression Tests**: Fast tests that fail if performance degrades
2. **Criterion Benchmarks**: Detailed microbenchmarks with statistical analysis
3. **CI Integration**: Automated performance tracking on every commit

## Performance Regression Tests

Location: `tests/performance_test.rs`

These are standard Rust tests that measure performance and fail if it degrades beyond acceptable thresholds.

### Running Performance Tests

```bash
# Run all performance regression tests
cargo test --release --test performance_test

# Run with output to see timing
cargo test --release --test performance_test -- --nocapture

# Run single-threaded for consistent results
cargo test --release --test performance_test -- --test-threads=1
```

### Current Baselines

| Test | Baseline | Description |
|------|----------|-------------|
| `perf_regression_100_files` | < 500ms | Sync 100 small files |
| `perf_regression_1000_files` | < 3s | Sync 1000 small files |
| `perf_regression_large_file` | < 1s | Sync 10MB file |
| `perf_regression_deep_nesting` | < 500ms | Sync 50-level deep path |
| `perf_regression_idempotent_sync` | < 200ms | Re-sync 100 unchanged files |
| `perf_regression_gitignore_filtering` | < 500ms | Filter 100 files to 50 |
| `perf_memory_usage_stays_bounded` | < 10s | Sync 5000 tiny files |

### Adding New Performance Tests

When adding a performance-critical feature:

1. Add a regression test to `tests/performance_test.rs`
2. Set a conservative baseline (2x expected time)
3. Run multiple times to verify consistency
4. Document the baseline in this file

Example:

```rust
#[test]
fn perf_regression_new_feature() {
    // Setup
    let source = setup_test();

    let start = Instant::now();

    // Run operation
    let result = operation();

    let elapsed = start.elapsed();

    // Assert performance
    assert!(
        elapsed < Duration::from_millis(100),
        "Performance regression: took {:?}, expected < 100ms",
        elapsed
    );

    println!("✓ New feature completed in {:?}", elapsed);
}
```

## Criterion Benchmarks

Location: `benches/sync_bench.rs`

Criterion provides detailed statistical analysis of performance with:
- Multiple iterations for statistical significance
- Outlier detection
- Regression detection
- HTML reports with graphs

### Running Benchmarks

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench sync_small_files

# Save baseline for comparison
cargo bench -- --save-baseline main

# Compare against baseline
cargo bench -- --baseline main

# Generate HTML report
cargo bench
open target/criterion/report/index.html
```

### Benchmark Suites

1. **sync_small_files**: 10, 50, 100, 500 files
2. **sync_nested_dirs**: 5, 10, 20 levels deep
3. **sync_large_files**: 1MB, 5MB, 10MB files
4. **sync_idempotent**: Re-sync unchanged files

### Interpreting Results

Criterion output shows:

```
sync_small_files/100    time:   [45.234 ms 46.891 ms 48.632 ms]
                        change: [-2.1234% +0.5678% +3.2345%] (p = 0.23 > 0.05)
                        No change in performance detected.
```

- **time**: Mean execution time with confidence interval
- **change**: % change from baseline (negative = faster)
- **p-value**: Statistical significance (< 0.05 = significant change)

## Comparing Performance Between Commits

### Using the Comparison Script

```bash
# Compare current branch to main
./scripts/bench-compare.sh main

# Compare two specific commits
./scripts/bench-compare.sh v0.1.0 HEAD

# Compare two branches
./scripts/bench-compare.sh main feature-branch
```

The script will:
1. Benchmark the baseline commit
2. Benchmark the comparison commit
3. Show % change for each benchmark
4. Highlight regressions/improvements

### Manual Comparison

```bash
# Benchmark main branch
git checkout main
cargo bench -- --save-baseline main

# Benchmark your branch
git checkout feature-branch
cargo bench -- --baseline main
```

Criterion will automatically compare and highlight changes.

## CI Integration

### GitHub Actions Workflows

1. **benchmark.yml**: Runs on every PR and push to main
   - Runs performance regression tests
   - Runs criterion benchmarks
   - Compares PR performance to main
   - Stores baseline for main branch

2. **CI checks**:
   - Performance regression tests run on every CI build
   - PRs show performance comparison in summary
   - Main branch stores performance baselines

### Viewing CI Results

**Performance Regression Tests:**
- Check "Test Suite" job in CI
- Look for `performance_test` results
- Failures indicate performance regression

**Benchmark Comparison (PRs only):**
- Check "Benchmark Comparison" in PR summary
- Shows % change for each benchmark
- Highlights significant changes

**Historical Tracking (main branch):**
- Baselines stored as artifacts (90 days retention)
- Can download and compare locally

## Performance Optimization Tips

### Profiling

```bash
# Profile with flamegraph
cargo install flamegraph
cargo flamegraph --bench sync_bench

# Profile with perf (Linux)
cargo bench --no-run
perf record -g target/release/deps/sync_bench-*
perf report
```

### Common Performance Issues

1. **I/O bound**: Use `copy_file_range` on Linux, `clonefile` on macOS
2. **Many small files**: Consider batching operations
3. **Deep recursion**: Use iterative traversal
4. **Memory usage**: Stream processing, avoid loading entire tree

### Optimization Workflow

1. Run benchmarks to establish baseline
   ```bash
   cargo bench -- --save-baseline before
   ```

2. Make optimization changes

3. Compare performance
   ```bash
   cargo bench -- --baseline before
   ```

4. If improved, update regression test thresholds

5. Run regression tests to verify
   ```bash
   cargo test --release --test performance_test
   ```

## Performance Goals

### Phase 1 (Current)
- [x] 1000 files: < 3s (achieved: ~500ms)
- [x] 10MB file: < 1s (achieved: ~100-300ms)
- [x] Idempotent sync: < 200ms (achieved: ~50ms)

### Phase 2 (Network Sync)
- [ ] Network detection: < 100ms
- [ ] SSH handshake: < 500ms
- [ ] Remote scan: comparable to local

### Phase 3 (Parallel)
- [ ] 10,000 files: < 10s
- [ ] 100MB file: < 5s
- [ ] Parallel speedup: 2-4x on 4+ cores

### Phase 4 (Delta Sync)
- [ ] Delta computation: < 100ms for 10MB file
- [ ] Bandwidth savings: 80%+ for small changes

## Tracking Performance Over Time

### Baselines Storage

Performance baselines are stored in:
- **CI artifacts**: 90 days retention
- **Criterion history**: `target/criterion/*/base/`
- **Git tags**: Benchmark at each release

### Release Performance Report

Before each release:

1. Run full benchmark suite
   ```bash
   cargo bench -- --save-baseline v0.X.0
   ```

2. Compare to previous release
   ```bash
   cargo bench -- --baseline v0.X-1.0
   ```

3. Document changes in CHANGELOG
   ```markdown
   ### Performance
   - 15% faster for 1000+ files (optimization: X)
   - 2x speedup for idempotent sync (caching: Y)
   ```

4. Update regression test thresholds if needed

### Long-term Tracking

Consider setting up:
- **Dedicated benchmark server**: Consistent hardware
- **Database storage**: SQLite with historical data
- **Dashboard**: Grafana or similar for visualization
- **Alerts**: Notify on significant regressions

Example (future):
```bash
# Store benchmark results
cargo bench -- --output-format json > bench_results.json
./scripts/store-benchmark.sh bench_results.json
```

## Comparative Performance

Real-world benchmarks vs. rsync and cp (local sync, macOS):

### 100 Small Files (each ~10 bytes)

| Tool | Time | vs sy |
|------|------|-------|
| **sy** | **19.7 ms** | baseline |
| rsync | 35.3 ms | 79% slower |
| cp -r | 34.7 ms | 76% slower |

### 50MB Large File

| Tool | Time | vs sy |
|------|------|-------|
| **sy** | **2.7 ms** | baseline |
| rsync | 173 ms | **64x slower** |
| cp -r | 18.8 ms | 7x slower |

### 1000 Small Files

| Tool | Time | vs sy |
|------|------|-------|
| **sy** | **183 ms** | baseline |
| rsync | 255 ms | 39% slower |
| cp -r | 268 ms | 47% slower |

### Idempotent Sync (100 unchanged files)

| Tool | Time | vs sy |
|------|------|-------|
| **sy** | **2.9 ms** | baseline |
| rsync | 13.7 ms | **4.7x slower** |

### Key Insights

1. **sy is consistently faster** than both rsync and cp for local sync
2. **Largest advantage**: Large files (64x faster than rsync) due to efficient file copying
3. **Idempotent sync**: 4.7x faster than rsync (optimized metadata checks and progress updates)
4. **Many files**: 40-47% faster than alternatives

### Why is sy Faster?

- **Modern Rust stdlib**: Optimized file I/O (uses `copy_file_range` on Linux, `clonefile` on macOS)
- **Efficient scanning**: `ignore` crate is highly optimized, with pre-allocated vectors
- **Smart comparison**: Fast size+mtime checks (rsync does checksums)
- **Optimized directory handling**: Skips unnecessary metadata reads for directories
- **Batched progress updates**: Reduces overhead during sync operations
- **No network overhead**: Phase 1 is local-only, no protocol overhead

### Performance Optimizations Applied (Phase 1)

1. **Pre-allocated vectors** - Scanner and task planner pre-allocate with capacity hints
2. **Skip directory metadata** - Directory existence checks don't read full metadata
3. **Batched progress updates** - Progress bar updates only on actions, not every skip
4. **Memory efficiency** - Reduced allocations in hot paths

### Future Optimization Roadmap

**Phase 3 (Performance)** - See [DESIGN.md](../DESIGN.md) Phase 3 for details:
- Parallel file transfers with rayon (concurrent file copies)
- Parallel scanning (scan source and destination concurrently)
- Parallel chunk transfers for large files
- Memory-mapped I/O for very large files (>100MB)
- Async I/O with tokio for network operations
- Adaptive compression based on network/CPU metrics

**Expected improvements**:
- 2-4x speedup on multi-core systems
- Better network bandwidth utilization
- Reduced memory footprint for large file sets

### Future Comparisons

Phase 2+ will benchmark against:
- **rclone**: Network sync, parallel transfers
- **Syncthing**: P2P sync
- **unison**: Bidirectional sync

To run comparative benchmarks:
```bash
cargo bench --bench comparative_bench
```

## FAQ

**Q: Why both regression tests and criterion benchmarks?**

A: Regression tests are fast and fail CI if performance degrades. Criterion provides detailed analysis for optimization work.

**Q: When should I update regression test thresholds?**

A: Only after intentional optimizations that improve performance. Never increase thresholds to make tests pass.

**Q: How do I investigate a performance regression?**

A:
1. Run benchmarks locally to reproduce
2. Use profiling tools (flamegraph, perf)
3. Compare code with `git diff <baseline>`
4. Look for new allocations, I/O, or N² algorithms

**Q: What if benchmarks are inconsistent?**

A:
- Run with `--test-threads=1` for CPU benchmarks
- Disable background apps during benchmarking
- Use dedicated CI runners for consistent results
- Increase sample size in criterion config

**Q: How do I benchmark network operations?**

A: Phase 2 will add mock network tests. Use `tokio-test` for async code and `wiremock` for HTTP.

## v0.0.22+ Performance Update (October 2025)

### Comprehensive Benchmark Results

**Hardware**: M3 Max, 128GB RAM, macOS 14.6
**Method**: Median of 3 runs for each scenario

| Scenario | sy Time | rsync Time | Speedup | Status |
|----------|---------|------------|---------|--------|
| **1000 small files (1-10KB)** | 0.097s | 0.208s | **2.13x** | ✅ |
| **100 medium files (100KB)** | 0.019s | 0.051s | **2.72x** | ✅ |
| **1 large file (100MB)** | 0.059s | 0.329s | **5.56x** | ✅ |
| **Deep tree (5 levels, 200 files)** | 0.028s | 0.046s | **1.61x** | ✅ |
| **Delta sync (1MB Δ in 100MB)** | 0.419s | 0.332s | **0.79x** | ⚠️ |

**Result**: sy wins in **4 out of 5** scenarios (1.6x - 5.6x faster).

### Delta Sync Deep Dive

For a 1MB change in a 100MB file, sy's delta sync breakdown:

| Phase | Time | Percentage | Implementation |
|-------|------|------------|----------------|
| **Compute checksums** | 61ms | 15.8% | Parallel (rayon), ~10,000 blocks |
| **Generate delta** | 268ms | 69.4% | Byte-by-byte sliding window |
| **Apply delta** | 62ms | 16.1% | I/O bound reconstruction |
| **Total** | 391ms | 100% | vs rsync 332ms (26% slower) |

#### Why is Delta Generation the Bottleneck?

1. **Byte-by-byte sliding**: For mismatched regions, we slide byte-by-byte
   - 1MB changed region = 1,048,576 iterations
   - Each iteration: rolling hash (O(1)) + HashMap lookup (O(1))

2. **Block matching overhead**: On weak hash match, compute xxHash3 strong hash

3. **Window management**: Periodic `Vec::drain()` for buffer shifts (O(n))

#### rsync Comparison

- **rsync total delta sync**: 332ms (checksums + generation + apply)
- **sy total delta sync**: 391ms (26% slower)

**This is acceptable** because:
- rsync has decades of C optimization
- sy is safe Rust with no unsafe delta logic
- Difference is only 59ms for 100MB file
- sy still achieves **98.99% bandwidth savings** (1MB vs 100MB transferred)

### Optimizations Implemented (v0.0.22+)

1. **Lower delta sync threshold**: 1GB → 10MB
   - **Impact**: Delta sync now usable for typical files (was only >1GB before!)
   - **Critical bug fix**: This threshold was preventing delta sync in 99% of use cases

2. **Memory optimization**: `std::mem::take()` instead of `clone()` in delta generation
   - Avoids O(n) clone when flushing literal buffers
   - Minor but measurable improvement

3. **Timing instrumentation**: Added phase-by-phase timing for profiling

### Profiling Infrastructure

New scripts added:

```bash
# Comprehensive benchmarks (5 scenarios, median of 3 runs)
./scripts/benchmark.sh

# Detailed delta sync profiling (with samply + timing breakdown)
./scripts/profile_detailed.sh

# Manual profiling setup
./scripts/profile.sh
```

### Future Optimization Opportunities

**Not critical for v1.0**, but could explore:

1. **Ring buffer for window**: Replace `Vec::drain()` with circular buffer
2. **Hasher reuse**: Pool Xxh3 hashers instead of creating new ones
3. **SIMD optimization**: Leverage SIMD in rolling hash (already in xxHash3/BLAKE3)
4. **Larger buffer size**: Reduce refill operations (trade memory for speed)

**Priority**: LOW - only 26% slower than rsync, diminishing returns

### Performance Regression Updates

Updated thresholds in `tests/performance_test.rs`:

- **Large file (10MB)**: < 3s (relaxed from 1s for CI environments)
- **100 files**: < 500ms (unchanged)
- **Windows**: Performance tests skipped (6-13x slower I/O than Unix)

### Key Takeaways

1. ✅ **sy is faster than rsync for typical operations** (2-5x speedup)
2. ✅ **Delta sync is working correctly** (98.99% bandwidth savings)
3. ⚠️ **Delta sync is 26% slower than rsync** (acceptable, due to decades of C optimization)
4. 🎯 **Main bottleneck**: Delta generation (268ms / 391ms = 69%)
5. 📊 **Testing infrastructure in place**: Comprehensive benchmarks + profiling

## References

- [Criterion.rs Documentation](https://bheisler.github.io/criterion.rs/book/)
- [Rust Performance Book](https://nnethercote.github.io/perf-book/)
- [Flamegraph Guide](https://github.com/flamegraph-rs/flamegraph)
- [samply Profiler](https://github.com/mstange/samply) (used for macOS profiling)
