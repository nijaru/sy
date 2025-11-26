# TODO

## Current

No active tasks.

## Backlog

### High Priority
- [ ] Windows support (sparse files, ACLs, path edge cases)
- [ ] russh migration (SSH agent blocker)
- [ ] S3 bidirectional sync

### Performance Optimizations (SOTA Research)

| Optimization | Impact | Status |
|--------------|--------|--------|
| Batch destination scan | ~1000x fewer SSH round-trips | ✅ Done |
| Parallel planning | ~20x faster (fallback) | ✅ Done |
| Progress indicators | UX improvement | ✅ Done |
| io_uring for local I/O | 3-12x IOPS improvement | ⏳ Evaluate |
| FastCDC for S3 dedup | 10-20% better dedup ratio | ⏳ After S3 sync |
| Merkle tree integrity | O(log n) verification | ⏳ Nice-to-have |

### Already Implemented
- ✅ Parallel chunk transfer (v0.0.62)
- ✅ Streaming pipeline (v0.0.61)
- ✅ Adaptive compression
- ✅ COW awareness (APFS/BTRFS/XFS)
- ✅ Parallel directory scanning (v0.0.64)
- ✅ SSH connection pooling
- ✅ Rejected QUIC (TCP BBR better for fast networks)

### Low Priority
- [ ] SIMD optimization (if bottlenecks reappear)
- [ ] Bloom filters for chunking pre-filter (premature)
