# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2026-01-19 |
| Build   | PASSING      | 2026-01-19 |

## Active Work

**2026-01-19: Streaming Protocol - Complete**

Branch: `feature/streaming-protocol-v2`

**Completed:**

- Fixed security issues (path traversal, frame size limits, symlink validation, delta bounds)
- Fixed runtime panic (`blocking_send` in async context → unbounded channels)
- Fixed pull mode directory creation
- Fixed skip-unchanged files (was re-transferring all files)
- Cross-platform tests pass (macOS ↔ Fedora)
- Benchmarks run with accurate data
- Features verified: bidirectional sync, watch mode, resume
- Code cleanup: removed 8 redundant scripts
- README updated with accurate benchmark claims

**Ready for review/merge.**

## Roadmap

### v0.3.0 (Streaming Protocol) — READY FOR MERGE

Cross-platform sync works. Benchmarks validated.

### Backlog

| Priority | Task                                            |
| -------- | ----------------------------------------------- |
| P3       | Daemon mode (deferred - streaming reduces need) |
| P4       | Python bindings (not implemented)               |

## Performance

**Benchmarked 2026-01-19** (M3 Max → Fedora via Tailscale)

### Local Sync

| Scenario    | Files | Initial     | Incremental | Delta       |
| ----------- | ----- | ----------- | ----------- | ----------- |
| source_code | 5000  | 1.3x faster | 3.5x faster | 3.6x faster |
| large_file  | 1     | 42x faster  | 1.4x faster | 1.6x faster |
| mixed       | 505   | 2.2x faster | 2.4x faster | 2.4x faster |
| small_files | 1000  | 1.1x slower | 3.1x faster | 2.9x faster |
| deep_dirs   | 100   | 1.1x faster | 1.7x faster | 1.6x faster |

### SSH Sync

| Scenario    | Files | Initial     | Incremental | Delta       |
| ----------- | ----- | ----------- | ----------- | ----------- |
| source_code | 5000  | 2.0x faster | ~parity     | ~parity     |
| large_file  | 1     | 1.1x faster | 1.6x slower | 1.7x slower |
| mixed       | 505   | ~parity     | 1.5x slower | 1.4x slower |
| small_files | 1000  | 1.5x slower | 1.3x slower | 1.3x slower |
| deep_dirs   | 100   | 1.3x slower | 1.4x slower | 1.3x slower |

**Key insight:** sy excels with many small files (source_code scenario) due to pipelined transfers. rsync has edge on larger files and incremental SSH updates.

### Optimization Opportunities (Not Implemented)

| Optimization     | Impact       | Effort | Notes                             |
| ---------------- | ------------ | ------ | --------------------------------- |
| Daemon mode      | High (~30ms) | High   | Keep `sy --server` running        |
| Message batching | Low (~5ms)   | Low    | Batch DEST_FILE_ENTRY messages    |
| Dir mtime cache  | Medium       | Medium | Skip unchanged directory subtrees |

The ~50ms gap vs rsync is fixed overhead (server spawn + protocol). Daemon mode would close most of this gap.

## Feature Flags

| Flag  | Default  | Notes          |
| ----- | -------- | -------------- |
| SSH   | Enabled  | ssh2 (libssh2) |
| S3    | Disabled | object_store   |
| GCS   | Disabled | object_store   |
| ACL   | Disabled | libacl-dev     |
| Watch | Disabled | notify         |
