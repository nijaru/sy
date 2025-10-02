# sy

> Modern file synchronization tool - rsync, reimagined

**sy** (pronounced "sigh") is a modern file sync tool built in Rust, inspired by the UX of `eza`, `fd`, and `ripgrep`. It's not a drop-in rsync replacement - it's a reimagining of file sync with verifiable integrity, adaptive performance, and transparent tradeoffs.

## Status

✅ **Phase 1 MVP Complete** - Basic local sync working!

[![CI](https://github.com/nijaru/sy/workflows/CI/badge.svg)](https://github.com/nijaru/sy/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

See [DESIGN.md](DESIGN.md) for comprehensive technical design (2,400+ lines of detailed specifications).

## Installation

### From Source (Recommended for now)

```bash
# Clone the repository
git clone https://github.com/nijaru/sy.git
cd sy

# Build and install
cargo install --path .

# Verify installation
sy --version
```

### Requirements

- Rust 1.70+ (for development)
- Git (for .gitignore support)

## Quick Start

```bash
# Basic sync
sy /source /destination

# Preview changes (dry-run)
sy /source /destination --dry-run

# Mirror mode (delete extra files in destination)
sy /source /destination --delete

# Quiet mode (only show errors)
sy /source /destination --quiet

# Verbose logging
sy /source /destination -v      # Debug level
sy /source /destination -vv     # Trace level
```

## Features (Phase 1)

### ✅ What Works Now

- **Smart File Sync**: Compares size + modification time (1s tolerance)
- **Git-Aware**: Automatically respects `.gitignore` patterns
- **Safe by Default**: Preview changes with `--dry-run`
- **Progress Display**: Beautiful progress bars with indicatif
- **Flexible Logging**: From quiet to trace level
- **Edge Cases**: Handles unicode, deep nesting, large files, empty dirs

### 📋 Common Use Cases

```bash
# Backup your project
sy ~/my-project ~/backups/my-project

# Sync to external drive
sy ~/Documents /Volumes/Backup/Documents --delete

# Preview what would change
sy ~/src ~/dest --dry-run

# Sync with detailed logging
sy ~/src ~/dest -vv
```

## Vision

**The Problem**: rsync is single-threaded, has confusing flags, and doesn't verify integrity end-to-end. Modern tools like rclone are faster but complex. We can do better.

**The Goal**: A file sync tool that:
- ✅ Auto-detects network conditions and optimizes accordingly
- ✅ Verifies integrity with multi-layer checksums
- ✅ Has beautiful progress display and helpful errors
- ✅ Works great out of the box with smart defaults
- ✅ Scales from a few files to millions

## Key Features (Planned)

### Adaptive Performance
```bash
# Auto-detects: Local? LAN? WAN? Optimizes for each
sy ~/src /backup                    # Local: max parallelism, no compression
sy ~/src server:/backup             # LAN: parallel + minimal delta
sy ~/src remote:/backup             # WAN: compression + delta + BBR
```

### Verifiable Integrity
```bash
# Multiple verification modes
sy ~/src remote:/dst                # Fast: xxHash3 block checksums
sy ~/src remote:/dst --verify       # Cryptographic: BLAKE3 end-to-end
sy ~/src remote:/dst --paranoid     # Maximum: multiple passes + comparison reads
```

### Beautiful UX
```
Syncing ~/src → remote:/dest

[████████████████████----] 75% | 15.2 GB/s | ETA 12s
  ├─ config.json ✓
  ├─ database.db ⣾ (chunk 45/128, 156 MB/s)
  └─ videos/large.mp4 ⏸ (queued)

Files: 1,234 total | 892 synced | 312 skipped | 30 queued
```

### Smart Defaults
- Auto-detects gitignore patterns in repositories
- Refuses to delete >50% of destination (safety check)
- Warns about file descriptor limits before hitting them
- Detects sparse files and transfers efficiently
- Handles cross-platform filename conflicts

## Comparison

| Feature | rsync | rclone | sy |
|---------|-------|--------|-----|
| Parallel file transfers | ❌ | ✅ | ✅ |
| Parallel chunk transfers | ❌ | ✅ | ✅ |
| Delta sync | ✅ | ❌ | ✅ |
| End-to-end checksums | ❌ | ✅ | ✅ |
| Adaptive compression | ❌ | ❌ | ✅ |
| Network auto-detection | ❌ | ❌ | ✅ |
| Modern UX | ❌ | ⚠️ | ✅ |
| Config files | ❌ | ✅ | ✅ |

## Example Usage

```bash
# Basic sync
sy ./src remote:/dest

# Preview changes (dry-run)
sy ./src remote:/dest --dry-run

# Mirror (delete files not in source)
sy ./src remote:/dest --delete

# Fast LAN transfer
sy ./src nas:/backup --mode lan

# WAN with compression
sy ./src server:/backup --mode wan

# Maximum verification
sy ./important remote:/backup --paranoid

# Use saved profile
sy backup-home  # Uses ~/.config/sy/config.toml
```

## Design Highlights

### Reliability: Multi-Layer Defense
- **Layer 1**: TCP checksums (99.99% detection)
- **Layer 2**: xxHash3 per-block (fast corruption detection)
- **Layer 3**: BLAKE3 end-to-end (cryptographic verification)
- **Layer 4**: Optional multiple passes + comparison reads

Research shows 5% of 100 Gbps transfers have corruption TCP doesn't detect. We verify at multiple layers.

### Performance: Adaptive Strategies
Different scenarios need different approaches:
- **Local**: Maximum parallelism, kernel optimizations (copy_file_range, clonefile)
- **LAN**: Parallel transfers, selective delta, minimal compression
- **WAN**: Delta sync, adaptive compression, BBR congestion control

### Scale: Millions of Files
- Stream processing (no loading entire tree into RAM)
- Bloom filters for efficient deletion
- State caching for incremental syncs
- Parallel directory traversal

See [DESIGN.md](DESIGN.md) for full technical details.

## Design Complete! ✅

The design phase is finished with comprehensive specifications for:

1. **Core Architecture** - Parallel sync, delta algorithm, integrity verification
2. **Edge Cases** - 8 major categories (symlinks, sparse files, cross-platform, etc.)
3. **Advanced Features** - Filters, bandwidth limiting, progress UI, SSH integration
4. **Error Handling** - Threshold-based with categorization and reporting
5. **Testing Strategy** - Unit, integration, property, and stress tests
6. **Implementation Roadmap** - 10 phases from MVP to v1.0

Total design document: **2,400+ lines** of detailed specifications, code examples, and rationale.

## Implementation Roadmap

### ✅ Phase 1: MVP (v0.1.0) - COMPLETE
- ✅ Basic local sync
- ✅ File comparison (size + mtime)
- ✅ Full file copy
- ✅ Simple progress display
- ✅ .gitignore support
- ✅ Dry-run and delete modes
- ✅ Comprehensive test suite (31 tests: unit, integration, property-based)

### Phase 2: Network Sync (v0.2.0)
- SSH transport
- SFTP fallback
- Network detection
- SSH config integration

### Phase 3: Performance (v0.3.0)
- Parallel file transfers
- Parallel chunk transfers
- Adaptive compression
- Progress UI at scale

### Phase 4: Delta Sync (v0.4.0)
- Rsync algorithm (Adler-32)
- Block signatures
- Delta computation
- Resume support

### Phase 5: Reliability (v0.5.0)
- Multi-layer checksums
- Verification modes
- Atomic operations
- Crash recovery

### Phases 6-10
- Edge cases & advanced features
- Extreme scale optimization
- UX polish
- Testing & documentation
- v1.0 release

## Testing

Phase 1 includes comprehensive testing at multiple levels:

```bash
# Run all tests
cargo test

# Run specific test suites
cargo test --lib          # Unit tests only
cargo test --test integration_test  # Integration tests
cargo test --test property_test     # Property-based tests

# Run with output
cargo test -- --nocapture
```

**Test Coverage (31 tests total):**
- **Unit Tests (15)**: Core module functionality, CLI validation, error handling
- **Integration Tests (11)**: End-to-end sync scenarios, edge cases
- **Property-Based Tests (5)**: Invariants that always hold (idempotency, completeness)

## Contributing

Design phase is complete. Implementation starting soon!

Interested in contributing? Areas we'll need help with:
- Rsync algorithm implementation
- SSH protocol optimization
- Cross-platform testing
- Performance benchmarking
- Documentation

## License

MIT

## Acknowledgments

Inspired by:
- **rsync** - The algorithm that started it all
- **rclone** - Proof that parallel transfers work
- **eza**, **fd**, **ripgrep** - Beautiful UX in Rust CLI tools
- **Syncthing** - Block-based integrity model

Research that informed the design:
- **Jeff Geerling** (2025) - rclone vs rsync benchmarks
- **ACM 2024** - "QUIC is not Quick Enough over Fast Internet"
- **ScienceDirect 2021** - File transfer corruption studies
- **Multiple papers** - rsync algorithm analysis, hash performance, compression strategies

---

**Next Steps**: Begin Phase 1 implementation (MVP - basic local sync)

**Questions?** See [DESIGN.md](DESIGN.md) for comprehensive technical details.
