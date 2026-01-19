# sy - Modern File Synchronization

Fast, modern file synchronization tool written in Rust - a reimagining of rsync with adaptive performance and verifiable integrity.

## Quick Start

**For AI agents:**

1. Read `ai/STATUS.md` (current state)
2. Check tasks: `tk ls` (task tracker)
3. Reference `ai/DESIGN.md` for architecture
4. Reference `ai/DECISIONS.md` for rationale

**Organization patterns**: [agent-contexts](https://github.com/nijaru/agent-contexts)

## Project Overview

| Attribute | Value                                |
| --------- | ------------------------------------ |
| Language  | Rust (edition 2021)                  |
| Version   | v0.3.0 (2026-01-19)                  |
| Tests     | 620+ passing, 12 ignored (SSH agent) |
| License   | MIT                                  |

**Key Features**: Bidirectional server mode, delta sync, parallel transfers, SSH, sparse files, S3/GCS (experimental)

## Structure

```
sy/
├── AGENTS.md              # AI entry point
├── CLAUDE.md → AGENTS.md  # Symlink
├── ai/                    # AI working context
│   ├── STATUS.md         # Current state (read FIRST)
│   ├── DESIGN.md         # System architecture
│   ├── DECISIONS.md      # Architectural decisions
│   ├── research/         # External research
│   ├── design/           # Component specs
│   └── tmp/              # Temporary (gitignored)
├── src/                   # Rust source
├── tests/                 # Integration tests
└── benches/               # Benchmarks
```

## AI Context Organization

**Session files** (read every session, <500 lines each):

- `ai/STATUS.md` — Current state, metrics, blockers
- `ai/DESIGN.md` — System architecture, components
- `ai/DECISIONS.md` — Architectural decisions

**Reference files** (loaded on demand):

- `ai/research/` — External research findings
- `ai/design/` — Component specifications
- `ai/tmp/` — Temporary artifacts (gitignored)

**Task tracking**: `tk ls` (persisted in `.tasks/`)

## Commands

```bash
# Build
cargo build
cargo build --release

# Test
cargo test
cargo clippy -- -D warnings

# Run
cargo run -- /source /dest [OPTIONS]
./target/release/sy /source /dest

# Format
cargo fmt
```

## Verification Steps

| Check  | Command                       | Requirement   |
| ------ | ----------------------------- | ------------- |
| Build  | `cargo build`                 | Zero errors   |
| Tests  | `cargo test`                  | All pass      |
| Lint   | `cargo clippy -- -D warnings` | Zero warnings |
| Format | `cargo fmt --check`           | No changes    |

## Code Standards

| Aspect         | Standard                                                     |
| -------------- | ------------------------------------------------------------ |
| Commit format  | `type: description` (feat, fix, docs, refactor, test, chore) |
| Comments       | WHY not WHAT                                                 |
| Error handling | `anyhow` (CLI), `thiserror` (library)                        |
| AI attribution | Remove "Generated with Claude"                               |

## Codebase Quirks

| Area         | Knowledge                    | Why                                                       |
| ------------ | ---------------------------- | --------------------------------------------------------- |
| Hashing      | xxHash3 ≠ rolling hash       | Cannot replace Adler-32 in delta sync. Different purposes |
| Networking   | QUIC slower on fast networks | 45% regression >600 Mbps. Use TCP+BBR                     |
| Compression  | Overhead on >4Gbps           | CPU bottleneck. Never compress local sync                 |
| Filesystems  | COW + hard links conflict    | Hard links MUST use in-place strategy (nlink > 1)         |
| Sparse files | FS-dependent SEEK_HOLE       | Not all FSes support. Graceful fallback                   |
| SSH          | Server mode tilde expansion  | `sy --server` receives literal `~`. Must expand manually  |
| SSH          | russh migration blocked      | SSH agent requires ~300 LOC custom protocol               |
| S3           | 5MB multipart minimum        | Small files use simple put                                |

## Performance

| Scenario     | Result                             |
| ------------ | ---------------------------------- |
| Local→Local  | 2-11x faster than rsync            |
| Delta sync   | 2x faster than rsync               |
| COW strategy | 5-9x faster on APFS/BTRFS/XFS      |
| Server mode  | Bidirectional, pipelined transfers |

## Dependencies

| Crate               | Purpose                  |
| ------------------- | ------------------------ |
| tokio               | Async runtime            |
| clap                | CLI parsing              |
| ssh2                | SSH/SFTP (C bindings)    |
| xxhash-rust, blake3 | Hashing                  |
| zstd, lz4-flex      | Compression              |
| fjall               | LSM-tree database        |
| object_store        | Cloud storage (optional) |

## Claude Code

| Feature  | Details                       |
| -------- | ----------------------------- |
| Commands | `.claude/commands/release.md` |
| MCP      | None                          |
| Hooks    | None                          |

## Current Focus

See `ai/STATUS.md` for current state, `ai/DESIGN.md` for architecture.

---

**Version**: v0.3.0 | **Updated**: 2026-01-19 | **Follows**: [agent-contexts](https://github.com/nijaru/agent-contexts)
