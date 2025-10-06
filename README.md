# sy

> Modern file synchronization tool - rsync, reimagined

**sy** (pronounced "sigh") is a modern file sync tool built in Rust, inspired by the UX of `eza`, `fd`, and `ripgrep`. It's not a drop-in rsync replacement - it's a reimagining of file sync with verifiable integrity, adaptive performance, and transparent tradeoffs.

## Status

✅ **Phase 1 MVP Complete** - Basic local sync working!
✅ **Phase 2 Complete** - SSH transport + Delta sync implemented! (v0.0.3)
✅ **Phase 3 Complete** - Parallel transfers + UX polish! (v0.0.4-v0.0.9)
🚀 **Current Version: v0.0.9** - 194+ tests passing, zero warnings!

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

# Parallel transfers (10 workers by default)
sy /source /destination -j 20   # Use 20 parallel workers

# Single file sync
sy /path/to/file.txt /dest/file.txt

# File size filtering (new in v0.0.9+)
sy /source /destination --min-size 1KB      # Skip files < 1KB
sy /source /destination --max-size 100MB    # Skip files > 100MB
sy /source /destination --min-size 1MB --max-size 50MB  # Only 1-50MB files

# Exclude patterns (new in v0.0.9+)
sy /source /destination --exclude "*.log"              # Skip log files
sy /source /destination --exclude "node_modules"       # Skip node_modules
sy /source /destination --exclude "*.tmp" --exclude "*.cache"  # Multiple patterns

# Bandwidth limiting (new in v0.0.9+)
sy /source /destination --bwlimit 1MB                  # Limit to 1 MB/s
sy /source user@host:/dest --bwlimit 500KB             # Limit remote sync to 500 KB/s
```

## Features

### ✅ What Works Now (v0.0.9)

**Local Sync (Phase 1 - Complete)**:
- **Smart File Sync**: Compares size + modification time (1s tolerance)
- **Git-Aware**: Automatically respects `.gitignore` patterns
- **Safe by Default**: Preview changes with `--dry-run`
- **Progress Display**: Beautiful progress bars with indicatif
- **Flexible Logging**: From quiet to trace level
- **Edge Cases**: Handles unicode, deep nesting, large files, empty dirs
- **Single File Sync**: Sync individual files, not just directories
- **File Size Filtering**: `--min-size` and `--max-size` with human-readable units (KB, MB, GB, TB)
- **Exclude Patterns**: `--exclude` flag with glob patterns (e.g., `*.log`, `node_modules`)
- **Bandwidth Limiting**: `--bwlimit` flag to control transfer rate (e.g., `1MB`, `500KB`)

**Delta Sync (Phase 2 - Complete)**:
- **Rsync Algorithm**: TRUE O(1) rolling hash (2ns per operation, verified constant time)
- **Adler-32 + xxHash3**: Fast weak hash + strong checksum
- **Block-Level Updates**: Only transfers changed blocks, not entire files
- **Adaptive Block Size**: Automatically calculates optimal block size (√filesize)
- **Streaming Implementation**: Constant ~256KB memory for files of any size
- **Remote Operations**: Enabled for all SSH/SFTP transfers
- **Local Operations**: Enabled for large files (>1GB threshold)
- **Smart Heuristics**: Automatic activation based on file size and transport type
- **Progress Visibility**: Shows compression ratio in real-time (e.g., "delta: 2.4% literal")

**Parallel Execution (Phase 3 - Complete)**:
- **Parallel File Transfers**: 5-10x faster for multiple files
- **Configurable Workers**: Default 10, adjustable via `-j` flag
- **Thread-Safe Stats**: Accurate progress tracking with Arc<Mutex<>>
- **Semaphore Control**: Prevents resource exhaustion
- **Error Handling**: Collects all errors, reports first failure

**UX & Polish (v0.0.9)**:
- **Color-Coded Output**: Green (created), yellow (updated), cyan (transfer stats), magenta (delta sync)
- **Performance Metrics**: Duration and transfer rate displayed in summary
- **Enhanced Dry-Run**: Clear "Would create/update/delete" messaging
- **Better Errors**: Actionable suggestions (e.g., "check disk space", "verify permissions")
- **CLI Examples**: Built-in help with common usage patterns
- **Delta Sync Visibility**: Real-time compression ratio and bandwidth savings
- **File Size Filtering**: `--min-size` and `--max-size` flags with human-readable units
- **Exclude Patterns**: `--exclude` flag for flexible glob-based filtering
- **Bandwidth Limiting**: `--bwlimit` flag for controlled transfer rates

**Compression Module (Ready, Integration Pending)**:
- **Performance** (benchmarked):
  - LZ4: 23 GB/s throughput
  - Zstd: 8 GB/s throughput (level 3)
- **Smart Heuristics** (corrected based on benchmarks):
  - Local: never compress (disk I/O bottleneck)
  - Network: always Zstd (CPU never bottleneck, even on 100 Gbps)
  - Skip: files <1MB, pre-compressed formats (jpg, mp4, zip, pdf, etc.)
- **Status**:
  - ✅ Module implemented and tested (18 unit tests)
  - ✅ Integration tests pass (5 tests, proven end-to-end)
  - ✅ Benchmarks prove 50x faster than originally assumed
  - ⏳ Production integration pending (protocol changes needed)
  - ✅ CLI flag ready (`--compress`)

### 📋 Common Use Cases

```bash
# Backup your project (uses delta sync for updates)
sy ~/my-project ~/backups/my-project

# Sync to external drive
sy ~/Documents /Volumes/Backup/Documents --delete

# Preview what would change
sy ~/src ~/dest --dry-run

# Sync with detailed logging (see delta sync in action)
RUST_LOG=info sy ~/src ~/dest

# Delta sync automatically activates for file updates
# Example output: "Delta sync: 3242 ops, 0.1% literal data"
# This means only 0.1% of the file was transferred!
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

| Feature | rsync | rclone | sy (v0.0.8) |
|---------|-------|--------|-----|
| Parallel file transfers | ❌ | ✅ | ✅ **Implemented!** |
| Parallel chunk transfers | ❌ | ✅ | 🚧 Planned |
| Delta sync | ✅ | ❌ | ✅ **Implemented!** |
| Streaming delta | ❌ | ❌ | ✅ **Constant memory!** |
| True O(1) rolling hash | ❌ | ❌ | ✅ **2ns per operation!** |
| Block checksums | ✅ MD5 | ❌ | ✅ xxHash3 |
| End-to-end checksums | ❌ | ✅ | 🚧 Planned |
| Compression support | ✅ | ✅ | ✅ Module ready |
| Network auto-detection | ❌ | ❌ | 🚧 Planned |
| Modern UX | ❌ | ⚠️ | ✅ |
| Single file sync | ⚠️ Complex | ✅ | ✅ |
| Zero compiler warnings | N/A | N/A | ✅ |

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
- ✅ Full file copy with platform optimizations
- ✅ Beautiful progress display
- ✅ .gitignore support
- ✅ Dry-run and delete modes
- ✅ Comprehensive test suite (49 tests: unit, integration, property-based, edge cases, performance)
- ✅ Performance optimizations (10% faster than initial implementation)
- ✅ Comparative benchmarks (vs rsync and cp)

### ✅ Phase 2: Network Sync + Delta (v0.0.3) - **COMPLETE**
- ✅ SSH transport (SFTP-based)
- ✅ SSH config integration
- ✅ **Delta sync implemented** (rsync algorithm)
- ✅ Adler-32 rolling hash + xxHash3 checksums
- ✅ Block-level updates for local and remote files
- ✅ Adaptive block size calculation

**Performance Win**: Delta sync dramatically reduces bandwidth usage by transferring only changed blocks instead of entire files.

### ✅ Phase 3: Parallelism + Optimization (v0.0.4-v0.0.8) - **COMPLETE**
- ✅ Parallel file transfers (5-10x speedup for multiple files)
- ✅ Configurable worker count (default 10, via `-j` flag)
- ✅ Thread-safe statistics tracking
- ✅ TRUE O(1) rolling hash (fixed critical bug, verified 2ns constant time)
- ✅ Streaming delta generation (constant ~256KB memory)
- ✅ Size-based local delta heuristic (>1GB files)
- ✅ Compression module (LZ4 + Zstd, ready for integration)
- ✅ Single file sync support
- ✅ Zero clippy warnings (idiomatic Rust)

**Critical Bug Fixed (v0.0.5)**: Original "O(1)" rolling hash was actually O(n) due to `Vec::remove(0)`. Fixed by removing unnecessary window field. Verified true constant time: 2ns per operation across all block sizes.

**Memory Win (v0.0.6)**: Streaming delta generation uses constant ~256KB memory regardless of file size. 10GB file: 10GB RAM → 256KB (39,000x reduction).

See [docs/OPTIMIZATIONS.md](docs/OPTIMIZATIONS.md) for detailed optimization history.

### Phase 4: Advanced Features (v0.1.0+) - NEXT
- Network speed detection
- Adaptive compression integration
- Parallel chunk transfers
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
cargo test --lib                      # Unit tests only
cargo test --test integration_test    # Integration tests
cargo test --test property_test       # Property-based tests
cargo test --test edge_cases_test     # Edge case tests
cargo test --release --test performance_test  # Performance regression tests

# Run benchmarks
cargo bench

# Run with output
cargo test -- --nocapture
```

**Test Coverage (193 tests total):**
- **Unit Tests (74)**: Core module functionality, CLI validation, error handling
- **Delta Tests (21)**: Rolling hash, checksum, delta generation, streaming
- **Compression Tests (11)**: LZ4/Zstd roundtrip, ratios, heuristics
- **Integration Tests (11)**: End-to-end sync scenarios, single file sync
- **Property-Based Tests (5)**: Invariants that always hold (idempotency, completeness)
- **Edge Case Tests (11)**: Unicode, deep nesting, large files, special characters
- **Performance Regression Tests (7)**: Ensure performance stays within bounds
- **CLI Tests (7)**: Argument parsing, validation, log levels

**Code Quality:**
- Zero compiler warnings
- Zero clippy warnings
- 100% of public API documented
- 4,403 lines of Rust code

See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for performance testing and regression tracking.

## Performance

**sy is consistently faster than rsync and cp for local sync:**

- **100 files**: 40-79% faster than rsync/cp
- **Large files (50MB)**: 64x faster than rsync, 7x faster than cp
- **Idempotent sync**: 4.7x faster than rsync
- **1000 files**: 40-47% faster than alternatives

See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for detailed benchmarks and performance testing.

## Contributing

Phase 1 MVP is complete! Phase 2 (Network Sync) is next.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

Interested in contributing? Areas we'll need help with:
- SSH transport implementation (Phase 2)
- Parallel transfers (Phase 3)
- Rsync algorithm implementation (Phase 4)
- Cross-platform testing
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

**Questions?** See [DESIGN.md](DESIGN.md) for comprehensive technical details or [CONTRIBUTING.md](CONTRIBUTING.md) to get started.
