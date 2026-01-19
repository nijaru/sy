# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2025-11-27 |
| Build   | 🟢 PASSING   | 2026-01-18 |

## Active Work

**2026-01-19: PR #13 Feature Review**

Branch: `feature/streaming-protocol-v2`

Streaming protocol phases 1-6 complete. Reviewed external PR #13 for useful features.

**Completed Task 1: GCS Transport**
- Implemented `GcsTransport` using `object_store`
- Added `gcs` feature flag
- Updated `TransportRouter` for `gs://` URLs

**Completed Task 2: S3 Testing**
- Verified `S3Transport` compilation and clippy
- Verified S3 integration tests skip correctly without env vars
- Actual MinIO testing skipped per user request

**Next:** Python bindings (P4) after v0.3.0 stability.

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
