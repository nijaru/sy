# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2026-01-19 |
| Build   | PASSING      | 2026-01-19 |

## Active Work

**2026-01-19: Performance Optimizations - Planned**

Branch: `feature/streaming-protocol-v2`

**Completed:**

- Streaming protocol v2 complete
- All security fixes, tests passing, benchmarks validated
- Ready for merge to main

**Next: Quick Win Optimizations**

See `ai/design/optimizations.md` for implementation details.

## Roadmap

### v0.3.0 (Streaming Protocol) — READY FOR MERGE

Cross-platform sync works. Benchmarks validated.

### Backlog

| Priority | Task                                            | Notes                          |
| -------- | ----------------------------------------------- | ------------------------------ |
| P2       | Message batching                                | ~5ms, low effort               |
| P2       | Dir mtime cache                                 | Variable impact, medium effort |
| P3       | Daemon mode (deferred - streaming reduces need) | ~30ms, high effort             |
| P4       | Python bindings (not implemented)               | maturin/pyo3                   |

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
