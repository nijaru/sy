# TODO

## Current

- [ ] **sy --server mode** - Custom wire protocol for SSH (see `ai/design/server-mode.md`)
  - Design: Complete (395 lines, payload formats, error handling)
  - **Phase 1 (MVP)**: ✅ Complete
    - [x] Protocol: HELLO, FILE_LIST/ACK, FILE_DATA, FILE_DONE
    - [x] Server handler with destination scanning
    - [x] Client-side pipelined transfers
    - [x] Default for local→remote SSH
    - **Result**: ~3.65s vs rsync ~3.25-4.89s (rsync parity!)
  - **Phase 2**: ✅ Complete
    - [x] MKDIR_BATCH - batched directory creation
    - [x] SYMLINK_BATCH - batched symlink creation
    - [x] Protocol flags (is_dir, is_symlink, is_hardlink, has_xattrs)
    - [x] Proper stats (dirs_created, symlinks_created)
    - [x] 12 unit tests for protocol/handler
  - **Phase 3** (Next): Delta sync - **KEY GAP vs rsync**
    - [ ] CHECKSUM_REQ/RESP messages (rolling checksums)
    - [ ] DELTA_DATA encoding (send only changed blocks)
    - [ ] Wire up existing delta code from `src/sync/transfer.rs`
    - **Why**: rsync wins partial updates (0.37s vs 0.46s) because it sends deltas
  - Phase 4: Compression, progress, resume
  - Target: v0.2.0

## Backlog

### High Priority
- [ ] Fix tilde (`~`) expansion in SSH paths (General issue, not just server mode)
- [ ] Windows support (sparse files, ACLs, path edge cases)
- [ ] russh migration (SSH agent blocker)
- [ ] S3 bidirectional sync
- [ ] Improve SSH integration testing (see Testing Gap Analysis below)

### Testing Gap Analysis (v0.1.2 bugs discovered)

| Bug | Fix Status | Test Status |
|-----|------------|-------------|
| `ln -s` fails if exists (SSH) | ✅ Fixed | Tests added |
| `ln -s` not overwriting (local) | ✅ Fixed | Tests added |
| Checkpoint fails for SSH | ✅ Fixed | `dest_is_remote` flag added |
| Verification fails SSH | ✅ Fixed | Unit tests added |
| Symlinks not detected in scan | ✅ Fixed | Unit tests exist |
| Symlink overwrite (Update action) | ✅ Fixed | 5 tests added |

**Remaining gaps:**
- SSH tests require real SSH agent, so ignored in CI
- [ ] Add mock SSH server or parameterized tests
- [ ] CI job with SSH tests enabled (scheduled, not PR blocking)

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