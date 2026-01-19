# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2025-11-27 |
| Build   | 🟢 PASSING   | 2026-01-18 |

## Active Work

**2026-01-19: Streaming Protocol Complete**

Branch: `feature/streaming-protocol-v2`

Implementation phases 1-6 complete:

- `src/streaming/` — Full module (protocol, channel, generator, sender, receiver, pipeline)
- `src/server/mod.rs` — Server handler with v1/v2 dispatch

**Documentation cleanup done:**

- Removed session handoff files (CONTEXT.md, handoff.md)
- Removed gemini-specific prompts
- Consolidated performance analysis
- Updated DESIGN.md for streaming architecture

**Next:** Code cleanup (remove v1 code paths, fix clippy) — separate task

## Roadmap

### v0.3.0 (Streaming Protocol) — IN PROGRESS

Phases 1-6 complete. Remaining: code cleanup.

**Targets:**

- SSH small_files: parity with rsync
- Time to first byte: <0.5s
- Memory (1M files): <500MB

### v0.2.1 (Bug Fixes + Cloud Storage)

**Done:** All critical bug fixes merged
**Remaining:** S3/GCS testing

### Backlog

- SyncEngine builder pattern
- Issue #12 features
- Incremental recursion

## Performance

| Scenario           | sy vs rsync         |
| ------------------ | ------------------- |
| Local sync         | **sy 2-44x faster** |
| SSH initial (bulk) | **sy 2-4x faster**  |
| SSH incremental    | Target: parity      |

## Feature Flags

| Flag  | Default  | Notes          |
| ----- | -------- | -------------- |
| SSH   | Enabled  | ssh2 (libssh2) |
| Watch | Disabled | File watching  |
| ACL   | Disabled | libacl-dev     |
| S3    | Disabled | Experimental   |
