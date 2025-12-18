# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.1.2       | 2025-11-27 |
| Tests   | 620+ passing | 2025-11-27 |
| Build   | 🟢 PASSING   | 2025-12-18 |

## Performance Summary (2025-12-18)

### Local (sy vs rsync)

| Scenario           | Initial    | Incremental | Delta       |
| ------------------ | ---------- | ----------- | ----------- |
| small_files (1000) | rsync 1.6x | **sy 3x**   | **sy 3x**   |
| large_file (100MB) | **sy 7x**  | **sy 1.5x** | **sy 1.1x** |
| source_code (5000) | rsync 2.1x | **sy 3.5x** | **sy 3.6x** |

### SSH (Mac → Fedora via Tailscale) - After pipelining (2025-12-18)

| Scenario           | Initial     | Incremental | Delta       |
| ------------------ | ----------- | ----------- | ----------- |
| small_files (1000) | rsync 1.6x  | rsync 1.4x  | rsync 1.4x  |
| large_file (100MB) | **sy 4.1x** | rsync 1.3x  | rsync 1.4x  |
| mixed (505)        | **sy 2.1x** | rsync 1.4x  | **sy ~par** |
| source_code (5000) | rsync 1.3x  | rsync 1.4x  | rsync 1.4x  |

### Key Findings

1. **Local incremental/delta**: sy wins massively (3-3.6x faster)
2. **Local large files**: sy wins 7x on initial
3. **SSH initial**: sy wins for bulk transfers (2-4x), except many small files
4. **SSH incremental/delta**: Still ~1.3-1.4x slower (inherent protocol overhead)
5. **Pipelining impact**: mixed/delta improved from 1.44x slower to ~par with rsync

## Active Work

**Completed**:

- Pipelined delta checksum requests (sy-09r)
- Server-side parallelism (2025-12-18): rayon parallel checksums, concurrent request handling, batched flushes

**Investigation result**: Server-side optimizations implemented but didn't close SSH incremental/delta gap. The ~1.3-1.4x slowdown is inherent to protocol/network latency, not server-side CPU processing. Benchmarks confirmed performance unchanged after server parallelism.

**Remaining options**:

1. Stream-level compression (may help with large deltas)
2. Accept the gap - sy wins locally (3x) and on bulk SSH transfers (2-4x)

**Benchmark tracking**: `scripts/benchmark.py` records to `benchmarks/history.jsonl`

**Community request**: [Issue #12](https://github.com/nijaru/sy/issues/12) - `--one-file-system`, SSH args, `--numeric-ids`

## Roadmap

### v0.2.0 (SSH Performance)

- [x] Pipeline delta checksum requests (P0) - done
- [x] Parallelize delta computation in batches - done
- [x] Server-side parallelism - done (didn't close gap, inherent latency)
- [ ] Stream-level compression after HELLO (P1)

### v0.3.0 (UX Polish)

- [ ] Suppress stack traces on user errors
- [ ] Fix quiet mode (suppress all logging)
- [ ] Document resume-enabled default

### Backlog

- [ ] Issue #12 features (`--one-file-system`, SSH args)
- [ ] russh migration (pure Rust SSH)
- [ ] S3 bidirectional sync
- [ ] Windows support

## What Worked

- Bidirectional server mode (74f7c35): Push + pull over SSH
- Delta sync: 2-3x faster than rsync locally
- Large file throughput: 7x faster than rsync locally
- Protocol fix (66d05d5): Always send MKDIR_BATCH
- Benchmark infrastructure: JSONL tracking, automated comparison
- Delta pipelining: Batch CHECKSUM_REQ/RESP, parallel delta computation, batch DELTA_DATA/FILE_DONE
- Server-side: Rayon parallel checksums, concurrent request handling with channels, batched flushes

## What Didn't Work

- SSH incremental: 1.3-1.5x slower than rsync (protocol/network latency, not CPU)
- Server-side parallelism: Implemented but didn't close gap - bottleneck is latency, not processing
- Initial sync for many small files: rsync wins by 1.4-1.5x
- UX: Stack traces shown on normal validation errors

## Recent Releases

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
