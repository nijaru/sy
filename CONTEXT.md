# Session Context - 2026-01-18

## Current State

| Item         | Value                                                 |
| ------------ | ----------------------------------------------------- |
| Branch       | `fix/v0.2.1-bugs`                                     |
| Last Release | v0.2.0 (2025-12-18)                                   |
| Build        | Passing (`cargo check`, `cargo test`, `cargo clippy`) |
| Commits      | 2 new (943537b, e4887f7)                              |

## What Was Done This Session

1. **Code Review** - Found 1 ERROR (router.rs), 2 WARNs (ssh.rs)
2. **Bug Fixes** - Fixed all review issues
3. **Performance Analysis** - Identified request-response as fundamental bottleneck
4. **Streaming Protocol Design** - Full rewrite planned + reviewed + fixed

## Key Decision

**Full protocol rewrite** from request-response to rsync-style streaming.

Request-response has inherent latency floor. Even depth-64 pipelining on 50ms RTT with 10K files = 7.8s pure waiting. Streaming eliminates this.

## Streaming Protocol v2 (Ready for Implementation)

See `ai/design/streaming-protocol-v0.3.0.md` (800+ lines, fully specified).

**Two-Phase Design:**

1. **Initial Exchange** - Receiver streams DEST_FILE_ENTRY with checksums
2. **Streaming Transfer** - Pure unidirectional flow, no round-trips

**Architecture:**

- Three Tokio tasks: Generator → Sender → Receiver
- Bounded channels for backpressure
- TCP handles flow control

**Key Messages:**

- DEST_FILE_ENTRY (0x04) - Dest metadata + checksums for delta
- FILE_ENTRY (0x02) - Source metadata (with inode for hard links)
- DATA (0x06) - File content or delta ops
- DELETE (0x08) - Files to remove

**Targets:**

- SSH small_files: parity with rsync (from 1.6x slower)
- Time to first byte: <0.5s (from 2.5s)
- Memory (1M files): <500MB (from ~2GB)

## Tasks

```
tk-ofsp | p1 | Implement streaming protocol v2
tk-dpw8 | p2 | Add GCS transport using object_store
tk-cb9z | p2 | Test and verify S3 transport functionality
tk-bapt | p3 | Implement daemon mode for SSH
```

## Key Files

| File                                     | Purpose                          |
| ---------------------------------------- | -------------------------------- |
| `ai/design/streaming-protocol-v0.3.0.md` | Full protocol design (800 lines) |
| `ai/STATUS.md`                           | Current state, roadmap           |
| `ai/DECISIONS.md`                        | Rewrite decision rationale       |

## Next Steps

1. `/compact` to clear context
2. Create new branch for streaming implementation
3. Start Phase 1: Protocol foundation (message types, channels)
