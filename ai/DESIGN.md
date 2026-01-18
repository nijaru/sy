# System Design

## Overview

sy is a file synchronization tool with adaptive strategies for different environments (local, LAN, WAN, cloud).

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         CLI (main.rs)                        │
├─────────────────────────────────────────────────────────────┤
│                      Sync Engine (sync/)                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────────┐ │
│  │ Scanner  │→│ Strategy │→│ Transfer │→│ Server Mode │ │
│  └──────────┘  └──────────┘  └──────────┘  └─────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                    Transport Layer (transport/)              │
│  ┌───────┐  ┌──────┐  ┌────────┐  ┌────┐  ┌────────────┐  │
│  │ Local │  │ SSH  │  │ Server │  │ S3 │  │ DualTransport│ │
│  └───────┘  └──────┘  └────────┘  └────┘  └────────────┘  │
├─────────────────────────────────────────────────────────────┤
│                     Support Modules                          │
│  ┌───────────┐  ┌──────────┐  ┌────────┐  ┌─────────────┐  │
│  │ Integrity │  │ Compress │  │ Filter │  │   Resume    │  │
│  │ (hashing) │  │ (zstd)   │  │(gitignore)│ │(checkpoints)│ │
│  └───────────┘  └──────────┘  └────────┘  └─────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## Components

| Component        | Purpose                                      | Status       |
| ---------------- | -------------------------------------------- | ------------ |
| sync/scanner     | Directory traversal, parallel scanning       | Stable       |
| sync/strategy    | Planner: compare source/dest, decide actions | Stable       |
| sync/transfer    | File copy, delta sync, checksums             | Stable       |
| sync/server_mode | Binary protocol for SSH (push/pull)          | Stable       |
| transport/local  | Local filesystem operations                  | Stable       |
| transport/ssh    | SFTP via ssh2 (C bindings)                   | Stable       |
| transport/server | Server protocol client                       | Stable       |
| transport/s3     | AWS S3 via object_store                      | Experimental |
| server/          | `sy --server` handler                        | Stable       |
| integrity/       | xxHash3, BLAKE3, Adler-32                    | Stable       |
| compress/        | zstd, lz4 compression                        | Stable       |
| filter/          | Gitignore, rsync patterns                    | Stable       |

## Data Flow

**Local → Remote (Server Push):**

1. Scanner enumerates source files
2. Strategy compares with destination (via server)
3. Server mode streams files over binary protocol
4. Delta sync for large files (checksums → deltas)

**Remote → Local (Server Pull):**

1. Client connects, sends HELLO with PULL flag
2. Server scans source, sends MKDIR_BATCH → FILE_LIST
3. Client compares with local, sends decisions
4. Server streams FILE_DATA for requested files

## Key Design Decisions

→ See DECISIONS.md for rationale

| Decision    | Choice           | Why                       |
| ----------- | ---------------- | ------------------------- |
| Hashing     | xxHash3 + BLAKE3 | Speed + security          |
| Compression | zstd adaptive    | Best ratio/speed tradeoff |
| SSH         | ssh2 (libssh2)   | Mature, SSH agent works   |
| Protocol    | Custom binary    | Pipelined, delta-aware    |
| Database    | fjall (LSM)      | Pure Rust, embedded       |

## Component Details

→ See ai/design/ for detailed specs:

- `server-mode.md` — Binary protocol specification

---

## SSH Performance Analysis (2026-01-18)

### Current State

| Scenario           | sy vs rsync           |
| ------------------ | --------------------- |
| SSH initial (bulk) | **sy 2-4x faster**    |
| SSH incremental    | rsync 1.3-1.4x faster |

### Why rsync Wins on Incremental

rsync's architecture is fundamentally different:

```
rsync (streaming):
Generator ──────► Sender ──────► Receiver
    │                │                │
    └── no waiting ──┴── no waiting ──┘

sy (request-response):
Client ◄────────► Server
    │                │
    └── round-trip ──┘
```

**rsync's advantages:**

1. **Zero round-trips after start** - fire-and-forget messages
2. **Incremental recursion** - transfer starts before scan completes
3. **No packet framing** - pure streaming, no per-message overhead
4. **30 years of optimization**

**sy's limitations:**

1. **Request-response model** - inherent latency per operation
2. **Fixed pipeline depth (8)** - doesn't adapt to RTT
3. **Full scan before transfer** - latency to first byte
4. **2.5s startup overhead** - spawning `sy --server`

### Options to Improve

| Option                       | Impact                | Effort    | Protocol Break |
| ---------------------------- | --------------------- | --------- | -------------- |
| Daemon mode                  | High (repeated syncs) | Medium    | No             |
| Deeper pipelining (8→64)     | Medium                | Low       | No             |
| Incremental recursion        | High                  | High      | Partial        |
| Streaming model (like rsync) | High                  | Very High | Yes            |

### Recommended Path

1. **Daemon mode** - 3.5x faster for repeated syncs (from PR #13)
2. **Adaptive pipeline** - adjust depth based on measured RTT
3. **Incremental recursion** - start transfer before scan completes

Full streaming model would require protocol rewrite.

→ See ai/research/rsync-ssh-performance.md for detailed analysis

---

## Known Issues (2026-01-18)

From codebase review - see ai/review/ for details.

### Critical

| Issue                                  | Location                 | Risk           |
| -------------------------------------- | ------------------------ | -------------- |
| `content_equal()` size-only comparison | bisync/classifier.rs:226 | Data loss      |
| Lock `expect()` panics                 | transport/ssh.rs         | Crash mid-sync |

### Performance

| Issue                              | Location           | Impact         |
| ---------------------------------- | ------------------ | -------------- |
| `data.clone()` copies file content | server_mode.rs:185 | Memory         |
| Fixed pipeline depth (8)           | server_mode.rs     | SSH throughput |
| Vec drain in delta window          | generator.rs:89    | CPU            |

### Code Quality

| Issue                        | Location                       |
| ---------------------------- | ------------------------------ |
| `format_bytes` duplicated 3x | error.rs, resource.rs, main.rs |
| `SyncEngine::new` 35 params  | sync/mod.rs                    |
| 102 `#[allow(dead_code)]`    | various                        |
