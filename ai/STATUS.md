# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.2.0       | 2025-12-18 |
| Tests   | 620+ passing | 2025-11-27 |
| Build   | 🔴 FIXING    | 2026-01-18 |

## Active Work

**2026-01-18: Streaming Protocol - Cleanup in Progress**

Branch: `feature/streaming-protocol-v2`

Phases 1-6 complete:

- `src/streaming/` - Full implementation (protocol, channel, generator, sender, receiver, pipeline)
- Server integration done in `src/server/mod.rs`

**Current cleanup:**

- Removed `--protocol-v2` CLI flag (streaming is now THE protocol)
- Removed 775 lines of v1 code from `src/sync/server_mode.rs`
- Renamed `sync_v2_push`/`sync_v2_pull` to `sync_push`/`sync_pull`
- **Remaining:** Fix clippy errors, clean up `src/server/mod.rs` v1 code, update `src/transport/server.rs`

## Performance Summary (2025-12-18)

### Local (sy vs rsync)

| Scenario           | Initial     | Incremental | Delta       |
| ------------------ | ----------- | ----------- | ----------- |
| small_files (1000) | rsync 1.3x  | **sy 2.9x** | **sy 3.1x** |
| large_file (100MB) | **sy 44x**  | **sy 1.2x** | **sy 1.6x** |
| mixed (505)        | **sy 2.3x** | **sy 2.5x** | **sy 2.4x** |
| source_code (5000) | rsync 1.2x  | **sy 3.2x** | **sy 3.4x** |

### SSH (Mac → Fedora via Tailscale)

| Scenario           | Initial     | Incremental | Delta       |
| ------------------ | ----------- | ----------- | ----------- |
| small_files (1000) | rsync 1.6x  | rsync 1.4x  | rsync 1.4x  |
| large_file (100MB) | **sy 4.1x** | rsync 1.3x  | rsync 1.4x  |
| mixed (505)        | **sy 2.1x** | rsync 1.4x  | **sy ~par** |
| source_code (5000) | rsync 1.3x  | rsync 1.4x  | rsync 1.4x  |

## Roadmap

### v0.3.0 (Streaming Protocol) - IN PROGRESS

Full protocol rewrite from request-response to rsync-style streaming.

**Design:** `ai/design/streaming-protocol-v0.3.0.md`

**Implementation Phases:**

1. [x] Protocol foundation (message types, channels)
2. [x] Generator (scanner integration)
3. [x] Sender (file reading, delta computation)
4. [x] Receiver (file writing)
5. [x] Pipeline (orchestration)
6. [x] Server integration (sy --server)
7. [ ] Cleanup (remove v1 code, fix clippy) - IN PROGRESS

**Targets:**

- SSH small_files: parity with rsync (from 1.6x slower)
- Time to first byte: <0.5s (from 2.5s)
- Memory (1M files): <500MB (from ~2GB)

### v0.2.1 (Bug Fixes + Cloud Storage)

**Critical Bug Fixes (done):**

- [x] Fix `content_equal()` data loss bug
- [x] Fix lock `expect()` panics
- [x] Fix SystemTime unwrap panic
- [x] Remove dead retry code

**Remaining:**

- [ ] Test and verify S3 transport
- [ ] Add GCS transport implementation

### Backlog

- [ ] SyncEngine builder pattern
- [ ] Issue #12 features (`--one-file-system`, SSH args)
- [ ] Incremental recursion (start transfer before scan)

## What Worked

- Streaming protocol unidirectional design
- Phases 1-6 implementation (protocol, channel, generator, sender, receiver, pipeline)
- Clean break from v1 (no backwards compat in 0.x.x)

## What Didn't Work

- Gemini broke down during v1 cleanup (stopped using tools)

## Feature Flags

| Flag  | Default  | Notes             |
| ----- | -------- | ----------------- |
| SSH   | Enabled  | ssh2 (libssh2)    |
| Watch | Disabled | File watching     |
| ACL   | Disabled | Linux: libacl-dev |
| S3    | Disabled | Experimental      |
