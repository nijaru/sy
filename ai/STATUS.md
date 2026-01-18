# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2025-11-27 |
| Build   | 🟢 PASSING   | 2025-12-18 |

## Performance Summary (2025-12-18)

### Local (sy vs rsync)

| Scenario           | Initial     | Incremental | Delta       |
| ------------------ | ----------- | ----------- | ----------- |
| small_files (1000) | rsync 1.3x  | **sy 2.9x** | **sy 3.1x** |
| large_file (100MB) | **sy 44x**  | **sy 1.2x** | **sy 1.6x** |
| mixed (505)        | **sy 2.3x** | **sy 2.5x** | **sy 2.4x** |
| source_code (5000) | rsync 1.2x  | **sy 3.2x** | **sy 3.4x** |

### SSH (Mac → Fedora via Tailscale) - After pipelining (2025-12-18)

| Scenario           | Initial     | Incremental | Delta       |
| ------------------ | ----------- | ----------- | ----------- |
| small_files (1000) | rsync 1.6x  | rsync 1.4x  | rsync 1.4x  |
| large_file (100MB) | **sy 4.1x** | rsync 1.3x  | rsync 1.4x  |
| mixed (505)        | **sy 2.1x** | rsync 1.4x  | **sy ~par** |
| source_code (5000) | rsync 1.3x  | rsync 1.4x  | rsync 1.4x  |

### Key Findings

1. **Local incremental/delta**: sy wins massively (2.9-3.4x faster)
2. **Local large files**: sy wins 44x on initial (COW/clonefile on APFS)
3. **Local initial many files**: rsync wins 1.2-1.3x (parallelism overhead)
4. **SSH initial**: sy wins for bulk transfers (2-4x), except many small files
5. **SSH incremental/delta**: Still ~1.3-1.4x slower (inherent protocol overhead)

## Active Work

**2026-01-18: Streaming Protocol v2 - Phase 1 Complete**

Branch: `feature/streaming-protocol-v2`

Phase 1 done:

- `src/streaming/protocol.rs` - 16 message types, 1340 lines
- `src/streaming/channel.rs` - FileJob, DestIndex, channel types
- `src/streaming/mod.rs` - Public API
- 18 new tests passing

**Implementation plan:** `ai/design/streaming-implementation-plan.md` (detailed step-by-step for any AI agent)

v0.2.1 bug fixes are on `fix/v0.2.1-bugs` branch.

## Roadmap

### v0.2.1 (Bug Fixes + Cloud Storage) - IN PROGRESS

**Critical Bug Fixes (done):**

- [x] Fix `content_equal()` data loss bug - compare mtime when sizes match
- [x] Fix lock `expect()` panics - recover from poisoned locks
- [x] Fix SystemTime unwrap panic in conflict resolution
- [x] Remove dead retry code (retry.rs:107-121)

**Code Quality (done):**

- [x] Consolidate duplicated `format_bytes` (3 copies → 1 in resource.rs)
- [x] S3: AWS env vars, http:// endpoints, path scanning fix
- [x] GCS: URL parsing (`gs://`) in SyncPath

**Remaining:**

- [ ] Test and verify S3 transport functionality
- [ ] Add GCS transport implementation (use `object_store` crate)
- [ ] Fix sparse file lseek error handling

### v0.3.0 (Streaming Protocol Rewrite) - IN PROGRESS

Full protocol rewrite from request-response to rsync-style streaming.

**Design:** `ai/design/streaming-protocol-v0.3.0.md`
**Implementation plan:** `ai/design/streaming-implementation-plan.md`

**Implementation Phases:**

1. [x] Protocol foundation (message types, channels) - DONE
2. [ ] Generator (scanner integration)
3. [ ] Sender (file reading, delta computation)
4. [ ] Receiver (file writing)
5. [ ] Integration (pipeline orchestration)
6. [ ] Server integration (sy --server v2)
7. [ ] Client integration (SSH transport)

**Targets:**

- SSH small_files: parity with rsync (from 1.6x slower)
- Time to first byte: <0.5s (from 2.5s)
- Memory (1M files): <500MB (from ~2GB)

### v0.4.0 (Code Quality)

- [ ] SyncEngine builder pattern (35 params → builder)
- [ ] Split sync_file_with_delta (475 lines → helpers)
- [ ] Incremental recursion (start transfer before scan)

### Backlog

- [ ] Issue #12 features (`--one-file-system`, SSH args)
- [ ] UX: Suppress stack traces on user errors
- [ ] Incremental recursion (start transfer before scan)
- [ ] Stream-level compression after HELLO

## What Worked

- Bidirectional server mode (74f7c35): Push + pull over SSH
- Delta sync: 2-3x faster than rsync locally
- Large file throughput: 7x faster than rsync locally
- Protocol fix (66d05d5): Always send MKDIR_BATCH
- Benchmark infrastructure: JSONL tracking, automated comparison
- Delta pipelining: Batch CHECKSUM_REQ/RESP, parallel delta computation, batch DELTA_DATA/FILE_DONE
- Server-side: Rayon parallel checksums, concurrent request handling with channels, batched flushes
- Checkpoint default 10→100: Reduced resume state overhead for initial sync
- Verification default flip: `--verify` now opt-in, sy matches rsync speed by default

## What Didn't Work

- SSH incremental: 1.3-1.5x slower than rsync (protocol/network latency, not CPU)
- Server-side parallelism: Implemented but didn't close gap - bottleneck is latency, not processing
- UX: Stack traces shown on normal validation errors

## Recent Releases

### v0.2.0 (2025-12-18)

- **Breaking:** `--verify` now opt-in (was default)
- **Breaking:** Removed `--mode` flag (use `--verify` instead)
- Performance: sy now ~10% faster than rsync on small files
- Simplified verification to single `--verify` flag (xxHash3)

### v0.1.2 (2025-11-27)

- Bidirectional server mode (push + pull)
- Delta sync 2x faster than rsync
- Removed ~300 lines dead bulk_transfer code

### v0.1.1 (2025-11-26)

- Batch destination scanning (~1000x fewer SSH round-trips)
- Planning phase: 90 min → 30 sec for 531K files

### v0.1.0 (2025-11-25)

- Breaking: rsync-compatible defaults
- New flags: `--gitignore`, `--exclude-vcs`, `-u/--update`

## Feature Flags

| Flag  | Default  | Notes             |
| ----- | -------- | ----------------- |
| SSH   | Enabled  | ssh2 (libssh2)    |
| Watch | Disabled | File watching     |
| ACL   | Disabled | Linux: libacl-dev |
| S3    | Disabled | Experimental      |
