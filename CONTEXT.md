# Session Context - 2026-01-18

## Current State

| Item         | Value                                                 |
| ------------ | ----------------------------------------------------- |
| Branch       | `fix/v0.2.1-bugs`                                     |
| Last Release | v0.2.0 (2025-12-18)                                   |
| Build        | Passing (`cargo check`, `cargo test`, `cargo clippy`) |
| Uncommitted  | 12 files (bug fixes + design docs)                    |

## What Was Done This Session

1. **Code Review** - Found 1 ERROR (router.rs missing GCS match arms), 2 WARNs (ssh.rs unwrap)
2. **Bug Fixes** - Fixed all issues from review
3. **Performance Analysis** - Identified request-response as fundamental bottleneck
4. **Streaming Protocol Design** - Full rewrite planned (see ai/design/streaming-protocol-v0.3.0.md)

## Key Decision

**Full protocol rewrite authorized** - from request-response to rsync-style streaming.

Current architecture has inherent latency floor that optimizations cannot eliminate.
Even depth-64 pipelining on 50ms RTT with 10K files = 7.8 seconds of pure waiting.

## Uncommitted Changes

```
M CONTEXT.md
M Cargo.lock
M Cargo.toml           - gcp feature flag
M ai/STATUS.md         - streaming protocol roadmap
M ai/DECISIONS.md      - rewrite decision
M src/error.rs         - format_bytes consolidation
M src/main.rs          - format_bytes consolidation
M src/path.rs          - GCS URL parsing + is_gcs()
M src/perf.rs          - format_bytes consolidation
M src/transport/local.rs
M src/transport/router.rs - GCS match arms fixed
M src/transport/s3.rs  - Arc reuse fix
M src/transport/ssh.rs - unwrap → map_err
A ai/design/streaming-protocol-v0.3.0.md
A ai/design/performance-v0.3.0.md
```

## v0.3.0 Streaming Protocol (NEW)

Full rewrite from request-response to streaming. See `ai/design/streaming-protocol-v0.3.0.md`.

**Architecture:**

- Three Tokio tasks: Generator → Sender → Receiver
- Unidirectional flow, no ACKs in critical path
- Incremental file list streaming

**Phases (4 weeks):**

1. Protocol foundation (message types)
2. Generator (scanner integration)
3. Sender (file reading, delta)
4. Receiver (file writing)
5. Integration (SSH)
6. Delete + Resume
7. Polish

**Targets:**

- SSH small_files: parity with rsync
- Time to first byte: <0.5s (from 2.5s)
- Memory (1M files): <500MB (from ~2GB)

## Key Files

| File                                     | Purpose                           |
| ---------------------------------------- | --------------------------------- |
| `ai/design/streaming-protocol-v0.3.0.md` | Full protocol design (726 lines)  |
| `ai/STATUS.md`                           | Current state, roadmap            |
| `ai/DECISIONS.md`                        | Rewrite decision rationale        |
| `src/sync/server_mode.rs`                | Current protocol (to be replaced) |

## Tasks

```
tk-ofsp | p1 | Implement streaming protocol v2
tk-dpw8 | p2 | Add GCS transport using object_store
tk-cb9z | p2 | Test and verify S3 transport functionality
tk-bapt | p3 | Implement daemon mode for SSH
```

## Next Steps

1. Commit current changes (bug fixes + design)
2. Create new branch for streaming protocol
3. Start Phase 1: Protocol foundation
