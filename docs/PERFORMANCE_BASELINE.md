# Performance Baseline - sy v0.0.2 vs rsync

**Date**: 2025-10-02
**Version**: v0.0.2 (streaming + checksums)
**Test Environment**: Mac M1 → Fedora (Tailscale WireGuard tunnel)

## Benchmark Results

| Test Scenario | sy | rsync | Ratio | Analysis |
|---------------|-----|-------|-------|----------|
| **Initial sync (100 x 10KB)** | 19.3s | 0.38s | **50x slower** | SFTP per-file overhead |
| **Idempotent (no changes)** | 20.7s | 0.21s | **98x slower** | Still scans + checks every file |
| **Update (1 file changed)** | 20.9s | 0.20s | **100x slower** | No delta sync + overhead |
| **Large file (100MB)** | 6.3s | 2.8s | **2.3x slower** | SFTP overhead, but reasonable |
| **Large file update (1MB changed)** | 6.6s | 0.9s | **7.3x slower** | No delta sync - expected |

## Root Causes

### 1. Per-File SFTP Overhead (~240ms/file)

**Measurement**: Single 5-byte file takes 0.242s to transfer

**Why**: Each file requires:
- SFTP open file
- Write data
- Close file
- Set metadata (mtime)

Each operation is a round-trip over SSH. For 100 files = ~24 seconds.

**rsync advantage**: Custom binary protocol that pipelines operations and batches small files.

### 2. No Delta Sync

**Impact**: Test 5 shows 7.3x slower for large file updates
- sy transfers entire 100MB
- rsync transfers ~1MB delta using rolling hash algorithm

**Expected behavior** - not implemented yet.

### 3. Sequential Transfers

**Impact**: Can't saturate high-bandwidth links
- Transferring one file at a time
- Network latency kills throughput on WAN

**rsync limitation**: rsync is also single-threaded

## What's Working Well

✅ **Correctness**: MD5 checksums match perfectly
✅ **Streaming**: 100MB files transfer without OOM
✅ **Checksums**: xxHash3 calculated for all transfers
✅ **Large files**: Reasonable performance (2.3x slower, not 50x)

## Performance Roadmap

### v0.0.3 - Delta Sync (Highest Impact)
**Goal**: Match rsync on large file updates

- Implement rsync algorithm (rolling hash + block diff)
- Adler-32 for rolling hash
- xxHash3 for block checksums
- **Expected improvement**: Test 5 from 6.6s → ~1s (7x faster)

### v0.0.4 - Parallel Transfers (High Impact)
**Goal**: Reduce many-file overhead

- Transfer multiple files concurrently
- Adaptive concurrency based on file sizes
- **Expected improvement**: Test 1 from 19.3s → ~2-5s (4-10x faster)

### v0.0.5 - Custom Binary Protocol (Medium Impact)
**Goal**: Eliminate SFTP overhead

- Direct SSH channel communication
- Pipeline operations (don't wait for responses)
- Batch small files
- **Expected improvement**: Test 1 from 2-5s → ~0.5s (matches rsync)

### v0.1.0 - Compression (Conditional Impact)
**Goal**: Optimize for slow networks

- Adaptive compression (zstd/LZ4)
- Only on slow links (<100 MB/s)
- **Expected improvement**: 2-3x on text over WAN

## Current Bottlenecks (Prioritized)

1. **No delta sync** - 7x slower on updates (fix in v0.0.3)
2. **SFTP per-file overhead** - 50x slower on many files (fix in v0.0.4)
3. **SFTP protocol** - 2.3x slower on large files (fix in v0.0.5)

## Comparison Philosophy

**Goal**: Not to beat rsync on every metric, but to:
1. ✅ Match correctness (with better verification)
2. ⏳ Match performance on common scenarios
3. ✅ Exceed on UX and safety
4. ⏳ Exceed on parallel/modern hardware

**Acceptable gaps** (for now):
- 2-3x slower on large single files (SFTP vs custom protocol)
- Higher per-file overhead until parallel implementation

**Unacceptable gaps**:
- No delta sync (must fix soon)
- 50x slower on many files (fix with parallelism)

## Next Steps

1. ✅ Baseline established
2. → Implement delta sync (v0.0.3)
3. → Implement parallel transfers (v0.0.4)
4. → Benchmark again and measure improvements
5. → Consider custom protocol if SFTP remains bottleneck

## Test Commands

```bash
# Run benchmark
bash /tmp/sy_rsync_benchmark.sh

# Profile single file
time sy /tmp/single-src remote:/tmp/dest

# Profile many files
time sy /tmp/100-files remote:/tmp/dest

# Compare with rsync
time rsync -az /tmp/src/ remote:/tmp/dest/
```

## Conclusion

**v0.0.2 Status**: Correct but slow

- ✅ Streaming works (no OOM)
- ✅ Checksums work (verified integrity)
- ❌ 50-100x slower on many small files
- ❌ No delta sync (7x slower on updates)

**Priority**: Implement delta sync first (bigger win than parallelism for typical use cases)
