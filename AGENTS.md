# sy - Modern File Synchronization

*AI development context for the sy project*

## Quick Start

**For AI agents starting work:**
1. Load `@AGENTS.md` (this file)
2. Check `ai/TODO.md` for active tasks
3. Check `ai/STATUS.md` for current project state
4. Reference `ai/DECISIONS.md` for architectural context
5. See `DESIGN.md` for comprehensive technical design

**Organization patterns**: Follow [@external/agent-contexts/PRACTICES.md](https://github.com/nijaru/agent-contexts)

## Project Overview

**sy** (pronounced "sigh") is a fast, modern file synchronization tool written in Rust - a reimagining of rsync with adaptive performance, verifiable integrity, and beautiful UX.

- **Language**: Rust (edition 2021)
- **Status**: v0.0.34, Phase 2 in progress
- **Performance**: 1.3x - 8.8x faster than rsync
- **Tests**: 314 passing
- **License**: MIT

## Project Structure

```
sy/
├── AGENTS.md              # This file (AI entry point)
├── ai/                    # AI working context
│   ├── TODO.md           # Active tasks and priorities
│   ├── STATUS.md         # Current project state
│   ├── DECISIONS.md      # Architectural decisions
│   └── RESEARCH.md       # Research findings
├── docs/                  # Project documentation
│   ├── DESIGN.md         # Comprehensive technical design (2,400 lines)
│   ├── MODERNIZATION_ROADMAP.md  # Implementation phases
│   ├── PERFORMANCE.md    # Performance analysis
│   ├── EVALUATION_*.md   # Version evaluations
│   └── *_SUPPORT.md      # Platform-specific docs
├── src/                   # Rust source code
│   ├── main.rs
│   ├── sync/             # Sync orchestration
│   ├── transport/        # SSH/SFTP/local transports
│   ├── integrity/        # Hash functions (xxHash3, BLAKE3)
│   ├── compress/         # zstd/lz4 compression
│   ├── filter/           # Gitignore/rsync patterns
│   ├── perf.rs           # Performance monitoring
│   └── ...
├── tests/                 # Integration tests
├── benches/               # Performance benchmarks
├── README.md              # User-facing overview
├── DESIGN.md → docs/      # Symlink for easy access
├── CONTRIBUTING.md        # Contributor guidelines
└── .claude/CLAUDE.md      # Legacy AI context (references this file)
```

## Key Documents

Read these in order for architectural understanding:

1. **ai/STATUS.md** - Current state, what's implemented, what worked/didn't
2. **ai/DECISIONS.md** - Key architectural decisions with rationale
3. **DESIGN.md** - Comprehensive technical design (2,400 lines)
   - Hash functions (line 79-128)
   - Transport protocols (line 252-322)
   - Compression (line 143-181)
   - Edge cases (line 548-1036)
4. **docs/MODERNIZATION_ROADMAP.md** - Implementation phases
5. **ai/TODO.md** - Active work and backlog

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

**Active**: Documentation reorganization (ai/ directory structure)

**Next**: Phase 5 verification enhancements
- Pre-transfer checksums
- Verification database
- --verify-only mode

See `ai/TODO.md` for detailed task list.

## Known Issues & Gotchas

1. **xxHash3 is NOT a rolling hash**
   - Cannot replace Adler-32 in delta sync algorithm
   - Different purposes: xxHash3 for blocks, Adler-32 for rolling window

2. **QUIC is slower on fast networks**
   - 45% performance regression on >600 Mbps
   - Use TCP with BBR instead

3. **Compression overhead**
   - CPU bottleneck on >4Gbps connections
   - Never compress local sync

4. **COW and hard links**
   - Hard links MUST use in-place strategy
   - COW cloning breaks link semantics (nlink > 1)

5. **Sparse file support**
   - Filesystem-dependent (not all FSes support SEEK_HOLE/SEEK_DATA)
   - Tests verify correctness, log whether sparseness preserved

## Testing Strategy

- **Unit tests**: Hash correctness, compression, filter matching
- **Integration tests**: Full sync scenarios, resume, metadata
- **Property tests**: Idempotence, compression roundtrip
- **Stress tests**: Millions of files, huge sparse files
- **Benchmarks**: Hash speed, compression, parallel vs sequential

All tests must pass before merge: `cargo test && cargo clippy -- -D warnings`

## Performance Notes

- **Local→Local**: 1.3x - 8.8x faster than rsync
- **Delta sync**: ~4x faster (320 MB/s vs 84 MB/s)
- **COW strategy**: 5-9x faster on APFS/BTRFS/XFS
- **Parallel transfers**: Scales well with concurrent operations

See `docs/PERFORMANCE.md` for detailed benchmarks.

## Dependencies & Architecture

**Key Crates**:
- `tokio` - Async runtime
- `clap` - CLI parsing
- `russh` / `russh-sftp` - SSH/SFTP
- `xxhash-rust`, `blake3` - Hashing
- `zstd`, `lz4-flex` - Compression
- `indicatif` - Progress bars
- `walkdir`, `ignore` - Directory traversal

**Architecture**: See DESIGN.md:2095-2144 for complete dependency rationale.

## Multi-Session Handoff

**Before ending session**:
1. Update `ai/TODO.md` with progress
2. Update `ai/STATUS.md` with current state
3. Document discoveries in `ai/RESEARCH.md`
4. Record decisions in `ai/DECISIONS.md`

**Starting new session**:
1. Load this AGENTS.md
2. Check `ai/TODO.md` for active work
3. Check `ai/STATUS.md` for current state
4. Continue from documented state

## Quick Reference

**Find information about**:
- Hashing → DESIGN.md:79-128
- Transport → DESIGN.md:252-322
- Compression → DESIGN.md:143-181
- Filters → DESIGN.md:1059-1129
- Edge cases → DESIGN.md:548-1036
- Current status → ai/STATUS.md
- Active tasks → ai/TODO.md
- Past decisions → ai/DECISIONS.md
- Research findings → ai/RESEARCH.md

---

**Version**: v0.0.34 (Last updated: 2025-10-21)
