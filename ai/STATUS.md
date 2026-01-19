# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2026-01-19 |
| Build   | 🟢 PASSING   | 2026-01-19 |

## Active Work

**2026-01-19: Streaming Protocol - Security Fixes & Testing**

Branch: `feature/streaming-protocol-v2`

**Completed Today:**

- Fixed security issues (path traversal, frame size limits, symlink validation, delta bounds)
- Fixed runtime panic (`blocking_send` in async context → unbounded channels)
- Fixed pull mode directory creation
- Cross-platform tests pass (macOS ↔ Fedora)

**Next Steps:**

1. Run benchmarks (streaming vs rsync) - need data to validate design
2. Verify features work: bidirectional sync, watch mode, resume
3. Code cleanup: dead code, unused scripts/benchmarks
4. Update README with accurate benchmark data

**Decisions Made:**

- Use unbounded channels for async callbacks (bounded + blocking_send panics in tokio context)
- Protocol is just "streaming" in code (no v2 suffix) - branch name is for dev tracking only

## Roadmap

### v0.3.0 (Streaming Protocol) — TESTING IN PROGRESS

Cross-platform sync works. Need benchmark validation.

**Targets:**

- SSH small_files: parity with rsync
- Time to first byte: <0.5s
- Memory (1M files): <500MB

### Backlog

| Priority | Task                                            |
| -------- | ----------------------------------------------- |
| P3       | Daemon mode (deferred - streaming reduces need) |
| P4       | Python bindings (not implemented)               |

## Performance

**NEEDS UPDATED BENCHMARKS** - values below are pre-streaming protocol:

| Scenario           | sy vs rsync         |
| ------------------ | ------------------- |
| Local sync         | **sy 2-44x faster** |
| SSH initial (bulk) | **sy 2-4x faster**  |
| SSH incremental    | Target: parity      |

## Feature Flags

| Flag | Default  | Notes          |
| ---- | -------- | -------------- |
| SSH  | Enabled  | ssh2 (libssh2) |
| S3   | Disabled | object_store   |
| GCS  | Disabled | object_store   |
| ACL  | Disabled | libacl-dev     |
