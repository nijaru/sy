# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.3.0       | 2026-01-19 |
| Tests   | 620+ passing | 2026-01-19 |
| Build   | PASSING      | 2026-01-19 |

## Active Work

**2026-01-19: v0.3.0 Released**

- Streaming protocol v2 merged to main
- All security fixes, tests passing, benchmarks validated
- Message batching for DEST_FILE_ENTRY (64KB batches)
- GCS transport support

## Roadmap

### v0.3.0 (Streaming Protocol) — RELEASED

Cross-platform sync works. Benchmarks validated.

### Backlog

| Priority | Task                                            | Notes                            |
| -------- | ----------------------------------------------- | -------------------------------- |
| P3       | Daemon mode (deferred - streaming reduces need) | ~30ms, high effort               |
| P3       | Dir mtime cache (deferred)                      | Fundamental issues - see design/ |
| P4       | Python bindings (not implemented)               | maturin/pyo3                     |

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

### SSH Sync (with message batching)

| Scenario    | Files | Initial     | Incremental | Delta       |
| ----------- | ----- | ----------- | ----------- | ----------- |
| source_code | 5000  | 2.2x faster | ~parity     | ~parity     |
| large_file  | 1     | 1.2x faster | 1.5x slower | 1.5x slower |
| mixed       | 505   | ~parity     | 1.4x slower | 1.4x slower |
| small_files | 1000  | 1.4x slower | 1.3x slower | 1.4x slower |
| deep_dirs   | 100   | 1.4x slower | 1.3x slower | 1.3x slower |

**Key insight:** sy excels with many small files (source_code scenario) due to pipelined transfers + message batching. rsync has edge on incremental SSH updates.

### Optimizations Applied

| Optimization     | Status   | Notes                                  |
| ---------------- | -------- | -------------------------------------- |
| Message batching | Done     | 64KB batches for DEST_FILE_ENTRY       |
| Dir mtime cache  | Deferred | Content changes don't update dir mtime |
| Daemon mode      | Deferred | ~30ms gain, high effort                |

The ~50ms gap vs rsync is fixed overhead (server spawn + protocol). Daemon mode would close most of this gap.

## Feature Flags

| Flag  | Default  | Notes          |
| ----- | -------- | -------------- |
| SSH   | Enabled  | ssh2 (libssh2) |
| S3    | Disabled | object_store   |
| GCS   | Disabled | object_store   |
| ACL   | Disabled | libacl-dev     |
| Watch | Disabled | notify         |
