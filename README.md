# sy

> Modern file synchronization tool - rsync, reimagined

**sy** (pronounced "sigh") is a modern file sync tool built in Rust, inspired by the UX of `eza`, `fd`, and `ripgrep`. It's not a drop-in rsync replacement - it's a reimagining of file sync with verifiable integrity, adaptive performance, and transparent tradeoffs.

## Why sy?

**sy is 2-11x faster than rsync** for local operations:
- ✅ **8.8x faster** than rsync for large files (50MB: 21ms vs 185ms)
- ✅ **60% faster** than rsync for many small files (100 files: 25ms vs 40ms)
- ✅ **2x faster** for idempotent syncs (no changes: 8ms vs 17ms)
- ✅ **11x faster** for real-world workloads (500 files: <10ms vs 110ms)

See [docs/BENCHMARK_RESULTS.md](docs/BENCHMARK_RESULTS.md) for detailed benchmarks.

## Status

✅ **Phase 1 MVP Complete** - Basic local sync working!
✅ **Phase 2 Complete** - SSH transport + Delta sync implemented! (v0.0.3)
✅ **Phase 3 Complete** - Parallel transfers + UX polish! (v0.0.4-v0.0.9)
✅ **Phase 3.5 Complete** - Full compression + parallel checksums! (v0.0.10)
✅ **Phase 4 Complete** - JSON output, config profiles, watch mode, resume support! (v0.0.11-v0.0.13)
✅ **Phase 5 Complete** - BLAKE3 verification, symlinks, sparse files, xattrs! (v0.0.14-v0.0.16)
✅ **Phase 6 Complete** - Hardlink & ACL preservation! (v0.0.17+)
🚀 **Current Version: v0.0.17+** - 210 tests passing, zero errors!

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

# Watch mode (new in v0.0.12+)
sy /source /destination --watch                        # Continuous sync on file changes

# JSON output (new in v0.0.11+)
sy /source /destination --json                         # Machine-readable NDJSON output
sy /source /destination --json | jq                    # Pipe to jq for processing

# Config profiles (new in v0.0.11+)
sy --profile backup-home                               # Use saved profile
sy --list-profiles                                     # Show available profiles
sy --show-profile backup-home                          # Show profile details

# Resume support (new in v0.0.13+)
sy /large /destination                                 # Interrupt with Ctrl+C
sy /large /destination                                 # Re-run to resume from checkpoint

# Verification modes (new in v0.0.14+)
sy /source /destination --verify                       # BLAKE3 cryptographic verification
sy /source /destination --mode fast                    # Size + mtime only (fastest)
sy /source /destination --mode standard                # + xxHash3 checksums (default)
sy /source /destination --mode verify                  # + BLAKE3 end-to-end (cryptographic)
sy /source /destination --mode paranoid                # BLAKE3 + verify every block (slowest)

# Symlink handling (new in v0.0.15+)
sy /source /destination --links preserve               # Preserve symlinks as symlinks (default)
sy /source /destination -L                             # Follow symlinks and copy targets
sy /source /destination --links skip                   # Skip all symlinks

# Hardlink preservation (new in v0.0.17+)
sy /source /destination -H                             # Preserve hard links
sy /source /destination --preserve-hardlinks           # Same as -H

# ACL preservation (new in v0.0.17+)
sy /source /destination -A                             # Preserve ACLs (Unix/Linux/macOS)
sy /source /destination --preserve-acls                # Same as -A
```

## Features

### ✅ What Works Now (v0.0.13)

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
- **Parallel Checksums**: 2-4x faster block checksumming (v0.0.10)
- **Configurable Workers**: Default 10, adjustable via `-j` flag
- **Thread-Safe Stats**: Accurate progress tracking with Arc<Mutex<>>
- **Semaphore Control**: Prevents resource exhaustion
- **Error Handling**: Collects all errors, reports first failure

**UX & Polish (v0.0.10)**:
- **Color-Coded Output**: Green (created), yellow (updated), cyan (transfer stats), magenta (delta sync)
- **Performance Metrics**: Duration and transfer rate displayed in summary
- **Enhanced Dry-Run**: Clear "Would create/update/delete" messaging
- **Better Errors**: Actionable suggestions (e.g., "check disk space", "verify permissions")
- **CLI Examples**: Built-in help with common usage patterns
- **Delta Sync Visibility**: Real-time compression ratio and bandwidth savings
- **Compression Stats**: Files compressed and bytes saved displayed in summary
- **File Size Filtering**: `--min-size` and `--max-size` flags with human-readable units
- **Exclude Patterns**: `--exclude` flag for flexible glob-based filtering
- **Bandwidth Limiting**: `--bwlimit` flag for controlled transfer rates

**Compression (Phase 3.5 - Complete)**:
- **Performance** (benchmarked):
  - LZ4: 23 GB/s throughput
  - Zstd: 8 GB/s throughput (level 3)
- **Smart Heuristics**:
  - Local: never compress (disk I/O bottleneck)
  - Network: always Zstd (CPU never bottleneck, even on 100 Gbps)
  - Skip: files <1MB, pre-compressed formats (jpg, mp4, zip, pdf, etc.)
- **Status**:
  - ✅ Module implemented and tested (18 unit tests)
  - ✅ Integration tests pass (5 tests, proven end-to-end)
  - ✅ Benchmarks prove 50x faster than originally assumed
  - ✅ Production integration complete (v0.0.10)
  - ✅ Compression stats tracked and displayed
  - ✅ 2-5x reduction on text/code files

**Advanced Features (Phase 4 - Complete)**:
- **JSON Output** (v0.0.11):
  - Machine-readable NDJSON format for scripting
  - Events: start, create, update, skip, delete, summary
  - Auto-suppresses logging in JSON mode
  - Example: `sy /src /dst --json | jq`
- **Config Profiles** (v0.0.11):
  - Save common sync configurations
  - Config file: `~/.config/sy/config.toml`
  - Commands: `--profile`, `--list-profiles`, `--show-profile`
  - CLI args override profile settings
- **Watch Mode** (v0.0.12):
  - Continuous file monitoring for real-time sync
  - 500ms debouncing to avoid excessive syncing
  - Graceful Ctrl+C shutdown
  - Cross-platform (Linux, macOS, Windows)
- **Resume Support** (v0.0.13):
  - Automatic recovery from interrupted syncs
  - State file: `.sy-state.json` in destination
  - Flag compatibility checking
  - Skips already-completed files on resume

**Verification & Reliability (Phase 5 - In Progress)**:
- **Verification Modes** (v0.0.14):
  - **Fast**: Size + mtime only (trust filesystem)
  - **Standard** (default): + xxHash3 checksums
  - **Verify**: + BLAKE3 cryptographic end-to-end verification
  - **Paranoid**: BLAKE3 + verify every block written
  - Flags: `--mode <mode>` or `--verify` (shortcut for verify mode)
  - Shows verification stats in summary output
  - JSON output includes verification counts and failures
- **BLAKE3 Integration**:
  - 32-byte cryptographic hashes for data integrity
  - Verifies source and destination match after transfer
  - Fast parallel hashing (multi-threaded by default)
  - Detects silent corruption that TCP checksums miss
- **Symlink Support** (v0.0.15):
  - **Preserve** (default): Copy symlinks as symlinks
  - **Follow** (`-L`): Copy the symlink target file
  - **Skip**: Ignore all symlinks
  - Detects broken symlinks and logs warnings
  - Cross-platform (Unix/Linux/macOS)
- **Sparse File Support** (v0.0.15):
  - Automatic detection of sparse files (files with "holes")
  - Preserves sparseness during transfer (Unix/Linux/macOS)
  - Efficient transfer - only allocated blocks are copied
  - Critical for VM disk images, database files, etc.
  - Zero configuration - works transparently
- **Extended Attributes Support** (v0.0.16):
  - `-X` flag to preserve extended attributes (xattrs)
  - Preserves metadata like macOS Finder info, security contexts
  - Always scanned, conditionally preserved (minimal overhead)
  - Full-fidelity backups when combined with other features
  - Cross-platform (Unix/Linux/macOS)
- **Hardlink Preservation** (v0.0.17):
  - `-H` flag to preserve hard links between files
  - Tracks inode numbers during scan
  - Creates hardlinks instead of copying duplicate data
  - Preserves disk space savings from source to destination
  - **Local sync**: ✅ Fully working and tested
  - **SSH sync**: ⚠️ Partial - works for sequential files, known race condition with parallel transfers
  - Critical for backup systems, package managers, etc.
  - Cross-platform (Unix/Linux/macOS)
- **ACL Preservation** (v0.0.17+):
  - `-A` flag to preserve POSIX Access Control Lists
  - Always scanned, conditionally preserved (minimal overhead)
  - Preserves fine-grained permissions beyond owner/group/other
  - Parses and applies ACLs using standard text format
  - Essential for enterprise systems with complex permission models
  - Cross-platform (Unix/Linux/macOS)
  - Fully implemented and tested

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

### Verifiable Integrity ✅ (Implemented in v0.0.14)
```bash
# Multiple verification modes
sy ~/src remote:/dst --mode standard   # Default: xxHash3 checksums
sy ~/src remote:/dst --mode verify     # Cryptographic: BLAKE3 end-to-end
sy ~/src remote:/dst --mode paranoid   # Maximum: BLAKE3 + verify every block
sy ~/src remote:/dst --mode fast       # Fastest: size + mtime only
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

| Feature | rsync | rclone | sy (v0.0.10) |
|---------|-------|--------|-----|
| **Performance (local)** | baseline | N/A | **2-11x faster** |
| Parallel file transfers | ❌ | ✅ | ✅ |
| Parallel checksums | ❌ | ❌ | ✅ |
| Delta sync | ✅ | ❌ | ✅ |
| Streaming delta | ❌ | ❌ | ✅ **Constant memory!** |
| True O(1) rolling hash | ❌ | ❌ | ✅ **2ns per operation!** |
| Block checksums | ✅ MD5 | ❌ | ✅ xxHash3 |
| Compression | ✅ | ✅ | ✅ **Zstd (8 GB/s)** |
| Bandwidth limiting | ✅ | ✅ | ✅ |
| File filtering | ✅ | ✅ | ✅ |
| Modern UX | ❌ | ⚠️ | ✅ |
| Single file sync | ⚠️ Complex | ✅ | ✅ |
| Zero compiler warnings | N/A | N/A | ✅ |

**Future planned**: End-to-end checksums, network auto-detection, parallel chunk transfers, resume support

## Example Usage

```bash
# Basic sync (local or remote)
sy /source /destination
sy /source user@host:/dest

# Preview changes (dry-run)
sy /source /destination --dry-run

# Mirror mode (delete files not in source)
sy /source /destination --delete

# Parallel transfers (10 workers by default)
sy /source /destination -j 20

# File filtering
sy /source /destination --min-size 1MB --max-size 100MB
sy /source /destination --exclude "*.log" --exclude "node_modules"

# Bandwidth limiting
sy /source user@host:/dest --bwlimit 1MB
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

### ✅ Phase 3: Parallelism + Optimization (v0.0.4-v0.0.10) - **COMPLETE**
- ✅ Parallel file transfers (5-10x speedup for multiple files)
- ✅ Parallel checksum computation (2-4x faster)
- ✅ Configurable worker count (default 10, via `-j` flag)
- ✅ Thread-safe statistics tracking
- ✅ TRUE O(1) rolling hash (fixed critical bug, verified 2ns constant time)
- ✅ Streaming delta generation (constant ~256KB memory)
- ✅ Size-based local delta heuristic (>1GB files)
- ✅ **Full compression integration** (Zstd level 3, 8 GB/s throughput)
- ✅ Compression stats tracking and display
- ✅ Single file sync support
- ✅ Zero clippy warnings (idiomatic Rust)

**Critical Bug Fixed (v0.0.5)**: Original "O(1)" rolling hash was actually O(n) due to `Vec::remove(0)`. Fixed by removing unnecessary window field. Verified true constant time: 2ns per operation across all block sizes.

**Memory Win (v0.0.6)**: Streaming delta generation uses constant ~256KB memory regardless of file size. 10GB file: 10GB RAM → 256KB (39,000x reduction).

See [docs/OPTIMIZATIONS.md](docs/OPTIMIZATIONS.md) for detailed optimization history.

### Phase 4: Advanced Features (v0.1.0+) - NEXT
- Network speed detection
- Parallel chunk transfers for very large files
- Resume support for interrupted transfers
- End-to-end cryptographic checksums (BLAKE3)

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

**Test Coverage (100+ tests):**
- **Unit Tests (83)**: Core functionality, CLI, compression, delta sync, SSH config
- **Integration Tests (36)**: End-to-end scenarios, compression, edge cases, performance
  - Compression integration (5 tests)
  - Edge cases (11 tests)
  - Full sync scenarios (13 tests)
  - Performance regression (7 tests)

**Code Quality:**
- ✅ Zero compiler warnings
- ✅ Zero clippy warnings
- ✅ 100% of public API documented
- ✅ 5,500+ lines of Rust code
- ✅ All performance tests passing

See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for performance testing and regression tracking.

## Performance

**sy is consistently 2-11x faster than rsync for local sync:**

| Scenario | sy | rsync | Speedup |
|----------|-----|-------|---------|
| **100 small files** | 25 ms | 40 ms | **1.6x faster** |
| **50MB file** | 21 ms | 185 ms | **8.8x faster** |
| **Idempotent sync** | 8 ms | 17 ms | **2.1x faster** |
| **500 files** | <10 ms | 110 ms | **11x faster** |

**Why so fast?**
- Modern Rust stdlib with platform optimizations (`copy_file_range`, `clonefile`)
- Parallel file transfers (10 workers by default)
- Parallel checksum computation
- Efficient scanning with pre-allocated vectors
- Smart size+mtime comparison (vs rsync's checksums)

See [docs/BENCHMARK_RESULTS.md](docs/BENCHMARK_RESULTS.md) for comprehensive benchmark analysis.

## Contributing

sy v0.0.10 is production-ready! Phases 1-3 are complete.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

**Interested in contributing?** Areas we'd love help with:
- **Testing**: Cross-platform testing (Windows, Linux, macOS)
- **Performance**: Profiling and optimization for very large datasets
- **Features**: Phase 4 features (network detection, resume support, parallel chunks)
- **Documentation**: Usage examples, tutorials, blog posts
- **Real-world testing**: Use sy in your workflows and report issues!

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
