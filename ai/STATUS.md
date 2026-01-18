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

**2026-01-18: Gemini handoff prepared for v0.2.1 bug fixes**

Handoff document: `ai/prompts/gemini-handoff.md`

**Critical bugs to fix (v0.2.1)**:

1. `content_equal()` compares size only → data loss risk (`bisync/classifier.rs:226`)
2. Lock `expect()` panics → crash mid-sync (`transport/ssh.rs:172,225,240,248`)
3. SystemTime unwrap panic (`bisync/resolver.rs:231`)
4. Dead retry code (`retry.rs:107-121`)

**Review completed** - see `ai/review/` for detailed findings.

**PR #13 status**: AI-generated, quality unverified. Do not merge or cherry-pick. Use as reference only if needed.

**Branches cleaned**: All old branches deleted, only `main` remains.

**Previous work (2025-12-18)**:

- Pipelined delta checksum requests (sy-09r)
- Server-side parallelism: rayon parallel checksums, concurrent request handling, batched flushes

**Investigation result**: Server-side optimizations didn't close SSH incremental gap. Bottleneck is network latency, not CPU.

**SSH performance options** (v0.3.0):

1. Daemon mode - eliminates 2.5s startup overhead
2. Adaptive pipeline depth - adjust based on RTT
3. Zero-copy with Arc<[u8]> - stop cloning file data
4. Stream-level compression

**Benchmark tracking**: `scripts/benchmark.py` records to `benchmarks/history.jsonl`

**Community request**: [Issue #12](https://github.com/nijaru/sy/issues/12) - `--one-file-system`, SSH args, `--numeric-ids`

## Roadmap

### v0.2.1 (Critical Bug Fixes)

- [ ] Fix `content_equal()` data loss bug - compare mtime when sizes match
- [ ] Fix lock `expect()` panics - recover from poisoned locks
- [ ] Fix SystemTime unwrap panic in conflict resolution
- [ ] Remove dead retry code (retry.rs:107-121)

### v0.2.2 (Cloud Storage + Code Quality)

- [ ] Test and verify S3 transport functionality
- [ ] Add GCS transport (use `object_store` crate, same pattern as S3)
- [ ] Consolidate duplicated `format_bytes` (3 copies → 1)
- [ ] Fix sparse file lseek error handling

### v0.3.0 (SSH Performance)

- [ ] `Arc<[u8]>` zero-copy in server_mode.rs (-50% memory)
- [ ] Deeper pipelining (8→64) (+20-30% throughput)
- [ ] Adaptive pipeline depth based on RTT (+20-50% WAN)
- [ ] Daemon mode - eliminates 2.5s startup overhead (3.5x repeated)

### v0.4.0 (Code Quality)

- [ ] SyncEngine builder pattern (35 params → builder)
- [ ] Split sync_file_with_delta (475 lines → helpers)
- [ ] Ring buffer for delta window (-30% CPU)
- [ ] Pre-allocate protocol buffer (-20% allocations)

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
