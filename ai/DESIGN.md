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

| Component | Purpose | Status |
|-----------|---------|--------|
| sync/scanner | Directory traversal, parallel scanning | Stable |
| sync/strategy | Planner: compare source/dest, decide actions | Stable |
| sync/transfer | File copy, delta sync, checksums | Stable |
| sync/server_mode | Binary protocol for SSH (push/pull) | Stable |
| transport/local | Local filesystem operations | Stable |
| transport/ssh | SFTP via ssh2 (C bindings) | Stable |
| transport/server | Server protocol client | Stable |
| transport/s3 | AWS S3 via object_store | Experimental |
| server/ | `sy --server` handler | Stable |
| integrity/ | xxHash3, BLAKE3, Adler-32 | Stable |
| compress/ | zstd, lz4 compression | Stable |
| filter/ | Gitignore, rsync patterns | Stable |

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

| Decision | Choice | Why |
|----------|--------|-----|
| Hashing | xxHash3 + BLAKE3 | Speed + security |
| Compression | zstd adaptive | Best ratio/speed tradeoff |
| SSH | ssh2 (libssh2) | Mature, SSH agent works |
| Protocol | Custom binary | Pipelined, delta-aware |
| Database | fjall (LSM) | Pure Rust, embedded |

## Component Details

→ See ai/design/ for detailed specs:
- `server-mode.md` — Binary protocol specification
