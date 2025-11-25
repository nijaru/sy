# sy - Modern File Synchronization

*AI development context for the sy project*

## Quick Start

**For AI agents starting work:**
1. Load `@AGENTS.md` (this file)
2. Check `ai/STATUS.md` for current project state
3. Check `ai/TODO.md` for active tasks
4. Reference `ai/DECISIONS.md` for architectural context

**Organization patterns**: Follow [@external/agent-contexts/PRACTICES.md](https://github.com/nijaru/agent-contexts)

## Project Overview

**sy** (pronounced "sigh") is a fast, modern file synchronization tool written in Rust - a reimagining of rsync with adaptive performance and verifiable integrity.

- **Language**: Rust (edition 2021)
- **Status**: v0.1.0 (released 2025-11-25)
- **Performance**: 2-11x faster than rsync for local operations
- **Tests**: 477+ passing, 12 ignored (SSH agent tests)
- **License**: MIT
- **Key Features**: Delta sync, parallel transfers, SSH, sparse files, bidirectional sync, S3/cloud storage (experimental)

## Project Structure

```
sy/
├── AGENTS.md              # This file (AI entry point)
├── README.md              # User-facing overview (126 lines)
├── CONTRIBUTING.md        # Contributor guidelines (48 lines)
├── CHANGELOG.md           # Version history
├── ai/                    # AI working context
│   ├── STATUS.md         # Current state, recent work
│   ├── TODO.md           # Active tasks and backlog
│   ├── DECISIONS.md      # Architectural decisions
│   ├── RESEARCH.md       # Research index
│   ├── KNOWN_LIMITATIONS.md
│   └── research/         # Research findings
├── src/                   # Rust source code
│   ├── main.rs
│   ├── sync/             # Sync orchestration
│   ├── transport/        # SSH/SFTP/local/S3 transports
│   ├── integrity/        # Hash functions (xxHash3, BLAKE3)
│   ├── compress/         # zstd/lz4 compression
│   ├── filter/           # Gitignore/rsync patterns
│   └── ...
├── tests/                 # Integration tests
└── benches/               # Performance benchmarks
```

## Documentation Philosophy

**Minimal, maintainable docs only:**
- **README.md**: User-facing features, installation, examples
- **CONTRIBUTING.md**: Development setup, PR process
- **CHANGELOG.md**: Version history
- **ai/**: Agent working context (current state, decisions)
- **--help**: Command-line documentation

If users need it, put it in README or --help. Everything else goes stale.

## AI Context Organization

**Purpose:** AI maintains project context between sessions using ai/

**Session files** (read every session):
- **ai/STATUS.md** — Current state, metrics, blockers (read FIRST)
- **ai/TODO.md** — Active tasks only
- **ai/DECISIONS.md** — Architectural decisions
- **ai/RESEARCH.md** — Research index
- **ai/KNOWLEDGE.md** — Permanent codebase quirks and gotchas

**Reference files** (loaded on demand):
- **ai/research/** — Detailed research findings
- **ai/design/** — Design specifications
- **ai/decisions/** — Archived decisions
- **ai/tmp/** — Temporary artifacts (gitignored)

**Token efficiency:** Session files = current work only. Details in subdirs. Trust git history for historical context.

## Key Documents

Read these in order:

1. **ai/STATUS.md** - Current state, what's implemented, recent work
2. **ai/TODO.md** - Active tasks and backlog
3. **ai/DECISIONS.md** - Key architectural decisions
4. **ai/RESEARCH.md** - Research findings index
5. **ai/KNOWLEDGE.md** - Permanent codebase quirks and gotchas

## Claude Code Integration

**Slash Commands:**
- `.claude/commands/release.md` — Release workflow helper

**MCP Servers:** None currently configured

**Hooks:** None currently configured

See [Claude Code documentation](https://github.com/anthropics/claude-code) for custom slash commands, MCP servers, and hooks.

## Development Setup

```bash
# Build and test
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt

# Run
cargo run -- /source /dest [OPTIONS]

# Benchmarks
cargo bench

# Release build
cargo build --release
```

## Core Design Principles

1. **Verifiable, Not "Perfect"**
   - Multi-layer verification (TCP → xxHash3 → BLAKE3)
   - Research-backed: 5% of 100 Gbps transfers have corruption TCP doesn't detect

2. **Adaptive, Not One-Size-Fits-All**
   - Different strategies for local/LAN/WAN
   - COW-aware (APFS/BTRFS/XFS optimizations)
   - Filesystem-specific optimizations

3. **Transparent Tradeoffs**
   - Explicit --mode flags
   - Clear error messages with fixes
   - Performance metrics with --perf

## Code Conventions

- **No AI attribution**: Remove "Generated with Claude" from commits/PRs
- **Commit format**: `type: description` (feat, fix, docs, refactor, test, chore)
- **Comments**: Explain WHY, not WHAT - code should be self-documenting
- **Error handling**: Use anyhow for CLI, thiserror for library errors
- **Testing**: All features require tests before merge
- **Formatting**: `cargo fmt` before commit
- **Linting**: `cargo clippy -- -D warnings` must pass

## Current Focus

**Latest**: v0.1.0 released (2025-11-25)
- ✅ Breaking change: rsync-compatible defaults (copy all files by default)
- ✅ New flags: `--gitignore`, `--exclude-vcs`, `-u/--update`, `--ignore-existing`
- ✅ rsync compatibility: `-z` for compress, `-w` for watch, hidden `-r`
- ✅ 477+ tests passing

**Next**: See `ai/TODO.md` for priorities
- Windows support
- russh migration (SSH agent blocker)
- S3 bidirectional sync

## Codebase Quirks

See `ai/KNOWLEDGE.md` for permanent quirks and gotchas:
- Hash function limitations (xxHash3 vs Adler-32)
- Network protocol tradeoffs (QUIC, compression)
- Filesystem edge cases (COW, hard links, sparse files)
- Platform-specific constraints (S3, SSH)

## Testing Strategy

- **Unit tests**: Hash correctness, compression, filter matching
- **Integration tests**: Full sync scenarios, resume, metadata
- **Property tests**: Idempotence, compression roundtrip
- **Benchmarks**: Hash speed, compression, parallel vs sequential

All tests must pass before merge: `cargo test && cargo clippy -- -D warnings`

## Performance Notes

- **Local→Local**: 2-11x faster than rsync
- **Delta sync**: ~4x faster (320 MB/s vs 84 MB/s)
- **COW strategy**: 5-9x faster on APFS/BTRFS/XFS
- **Parallel transfers**: Scales well with concurrent operations

## Dependencies

**Key Crates**:
- `tokio` - Async runtime
- `clap` - CLI parsing
- `ssh2` - SSH/SFTP (C bindings, russh migration on hold)
- `xxhash-rust`, `blake3` - Hashing
- `zstd`, `lz4-flex` - Compression
- `fjall` - LSM-tree database (pure Rust)
- `object_store` - Cloud storage (pure Rust, optional)
- `indicatif` - Progress bars
- `ignore` - Directory traversal with .gitignore support

## Multi-Session Handoff

**Before ending session**:
1. Update `ai/STATUS.md` with current state
2. Update `ai/TODO.md` with progress
3. Document discoveries in `ai/RESEARCH.md`
4. Record decisions in `ai/DECISIONS.md`

**Starting new session**:
1. Load this AGENTS.md
2. Check `ai/STATUS.md` for current state
3. Check `ai/TODO.md` for active work
4. Reference `ai/DECISIONS.md` for context

## Quick Reference

**Find information about**:
- Current status → ai/STATUS.md
- Active tasks → ai/TODO.md
- Past decisions → ai/DECISIONS.md
- Research findings → ai/RESEARCH.md
- Codebase quirks → ai/KNOWLEDGE.md
- User docs → README.md
- Command-line → `sy --help`

---

**Version**: v0.1.0 (Last updated: 2025-11-25)
**Follows**: [agent-contexts v0.1.1](https://github.com/nijaru/agent-contexts)
