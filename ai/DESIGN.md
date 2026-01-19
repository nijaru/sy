# System Design

## Overview

sy is a file synchronization tool with rsync-style streaming protocol for high performance over SSH.

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
│               Streaming Protocol (streaming/)                │
│  ┌───────────┐  ┌────────┐  ┌──────────┐  ┌──────────────┐ │
│  │ Generator │→│ Sender │→│ Receiver │  │   Pipeline   │ │
│  └───────────┘  └────────┘  └──────────┘  └──────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                    Transport Layer (transport/)              │
│  ┌───────┐  ┌──────┐  ┌────────┐  ┌────┐                   │
│  │ Local │  │ SSH  │  │ Server │  │ S3 │                   │
│  └───────┘  └──────┘  └────────┘  └────┘                   │
├─────────────────────────────────────────────────────────────┤
│                     Support Modules                          │
│  ┌───────────┐  ┌──────────┐  ┌────────┐  ┌─────────────┐  │
│  │ Integrity │  │ Compress │  │ Filter │  │   Resume    │  │
│  │ (hashing) │  │ (zstd)   │  │(gitignore)│ │(checkpoints)│ │
│  └───────────┘  └──────────┘  └────────┘  └─────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## Streaming Protocol

The streaming protocol replaces request-response with rsync-style unidirectional flow.

```
Push (local → remote):
┌───────────┐     ┌────────┐     ┌──────────┐
│ Generator │ ──► │ Sender │ ──► │ Receiver │
│(local scan)│     │ (data) │     │ (write)  │
└───────────┘     └────────┘     └──────────┘
      │                                 │
      └── FileJob channel ──────────────┘

Pull (remote → local):
┌───────────┐     ┌────────┐     ┌──────────┐
│ Generator │ ──► │ Sender │ ──► │ Receiver │
│(remote scan)│    │ (data) │     │(local write)│
└───────────┘     └────────┘     └──────────┘
```

**Two-phase design:**

1. **Initial Exchange** - Receiver streams DEST_FILE_ENTRY with checksums
2. **Streaming Transfer** - Pure unidirectional flow, no round-trips

**Message types:** Hello, FileEntry, DestFileEntry, Data, DataEnd, Mkdir, Symlink, Delete, Done, Error

## Components

| Component           | Purpose                                | Status       |
| ------------------- | -------------------------------------- | ------------ |
| streaming/          | Streaming protocol implementation      | Stable       |
| streaming/protocol  | Message types, wire format             | Stable       |
| streaming/generator | Directory scanner, FileJob producer    | Stable       |
| streaming/sender    | File reading, delta computation        | Stable       |
| streaming/receiver  | File writing, delta application        | Stable       |
| streaming/pipeline  | StreamingSync orchestration            | Stable       |
| sync/scanner        | Directory traversal, parallel scanning | Stable       |
| sync/strategy       | Planner: compare source/dest, decide   | Stable       |
| sync/transfer       | File copy, delta sync, checksums       | Stable       |
| sync/server_mode    | SSH sync entry points (push/pull)      | Stable       |
| transport/local     | Local filesystem operations            | Stable       |
| transport/ssh       | SFTP via ssh2 (C bindings)             | Stable       |
| transport/server    | Server protocol client                 | Stable       |
| transport/s3        | AWS S3 via object_store                | Experimental |
| server/             | `sy --server` handler                  | Stable       |
| integrity/          | xxHash3, BLAKE3, Adler-32              | Stable       |
| compress/           | zstd, lz4 compression                  | Stable       |
| filter/             | Gitignore, rsync patterns              | Stable       |

## Data Flow

**Local → Remote (Push):**

1. Client connects, sends Hello with options
2. Server sends DestFileEntry for existing files (with block checksums)
3. Generator scans source, compares with DestIndex
4. Sender streams FileEntry + Data for changed files
5. Sender computes deltas for existing files
6. Receiver writes files, sends Done

**Remote → Local (Pull):**

1. Client connects, sends Hello with PULL flag
2. Client sends DestFileEntry for local files
3. Server Generator scans, compares, produces FileJobs
4. Server Sender streams FileEntry + Data
5. Client Receiver writes files locally

## Key Design Decisions

→ See DECISIONS.md for rationale

| Decision    | Choice           | Why                        |
| ----------- | ---------------- | -------------------------- |
| Protocol    | Streaming        | No round-trips, rsync-like |
| Hashing     | xxHash3 + BLAKE3 | Speed + security           |
| Compression | zstd adaptive    | Best ratio/speed tradeoff  |
| SSH         | ssh2 (libssh2)   | Mature, SSH agent works    |
| Database    | fjall (LSM)      | Pure Rust, embedded        |

## Component Details

→ See ai/design/ for detailed specs:

- `streaming-protocol-v0.3.0.md` — Full protocol specification
- `streaming-implementation-plan.md` — Implementation guide
- `ssh-optimization.md` — SSH performance tuning

## Performance

| Scenario           | sy vs rsync         |
| ------------------ | ------------------- |
| Local sync         | **sy 2-44x faster** |
| SSH initial (bulk) | **sy 2-4x faster**  |
| SSH incremental    | Target: parity      |
| SSH small files    | Target: parity      |

**Streaming protocol advantages:**

- Zero round-trips after initial exchange
- Delta computation with block checksums
- Unidirectional message flow
- Pipelined file transfers

→ See ai/review/performance-analysis.md for detailed analysis
