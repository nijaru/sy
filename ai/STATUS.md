# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2026-01-19 |
| Build   | 🟢 PASSING   | 2026-01-19 |

## Active Work

**2026-01-19: Streaming Protocol Complete**

Branch: `feature/streaming-protocol-v2`

**v1 Protocol Removed** — Clean streaming-only codebase:

- Deleted `server/handler.rs` (693 lines v1 handlers)
- Deleted `server/protocol.rs` (993 lines v1 types)
- Rewrote `server/mod.rs` (562→210 lines, streaming only)
- Simplified `transport/server.rs` (634→87 lines, connection only)
- Net removal: ~2600 lines of dead code

**Completed:**

- GCS transport (`object_store` based)
- S3 transport verified
- Streaming protocol phases 1-6
- v1 code removal

**Ready for:** v0.3.0 release testing

## Roadmap

### v0.3.0 (Streaming Protocol) — READY FOR TESTING

All implementation complete. One clean protocol, no backwards compat.

**Targets:**

- SSH small_files: parity with rsync
- Time to first byte: <0.5s
- Memory (1M files): <500MB

### Backlog

| Priority | Task                                            |
| -------- | ----------------------------------------------- |
| P3       | Daemon mode (deferred - streaming reduces need) |
| P4       | Python bindings                                 |
| —        | SyncEngine builder pattern                      |
| —        | Issue #12 features                              |

## Performance

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
