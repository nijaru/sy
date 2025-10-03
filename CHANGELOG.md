# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Delta sync metrics and progress visibility
  - Progress messages now show compression ratio (e.g., "delta: 2.4% literal")
  - TransferResult includes delta operations count and literal bytes transferred
  - Users can see bandwidth savings in real-time
- Delta sync summary statistics
  - Final summary shows total files using delta sync
  - Displays total bandwidth saved (e.g., "Delta sync: 3 files, 45.2 MB saved")
- Integration tests for file updates and delta sync
  - Verify update statistics accuracy
  - End-to-end delta sync validation (ignored by default - slow)
- Enhanced error messages with actionable suggestions
  - Permission denied: suggests checking ownership
  - Copy failed: suggests checking disk space
  - Directory read failed: suggests verifying path exists
- CLI help improvements
  - Added EXAMPLES section with common usage patterns
  - Shows basic, dry-run, delete, parallel, single file, and remote sync examples
- Timing and performance metrics
  - Sync duration displayed in summary (auto-formats: ms, seconds, minutes, hours)
  - Transfer rate calculation and display (bytes/sec)
  - Users can see sync speed and duration

### Changed
- Error messages now include helpful context and resolution steps
- Summary output formatting improved with better alignment and visual sections

### Planned for v0.1.0
- Network speed detection
- Adaptive compression integration
- Parallel chunk transfers (within single files)
- Resume support for interrupted transfers

### Planned for v0.5.0
- Multi-layer checksums (BLAKE3 end-to-end)
- Verification modes (fast, standard, paranoid)
- Atomic operations
- Crash recovery

## [0.0.8] - 2025-10-02

### Added
- Single file sync support (not just directories)
- Configurable parallel workers via `-j` flag (default 10)
- Size-based local delta heuristic (>1GB files automatically use delta sync)

### Changed
- Implemented `FromStr` trait for `Compression` enum (more idiomatic)
- Replaced `or_insert_with(Vec::new)` with `or_default()` (more idiomatic)
- Removed redundant closures in transport layer
- Delta sync now activates automatically for large local files (>1GB threshold)

### Fixed
- All clippy warnings resolved (7 warnings → 0)
- Code is now fully idiomatic Rust

### Performance
- Local delta sync enabled for large files where benefit outweighs overhead

### Testing
- Updated integration test to validate single file sync
- All 193 tests passing
- Zero compiler and clippy warnings

## [0.0.7] - 2025-10-01

### Added
- Comprehensive compression module (LZ4 + Zstd, ready for integration)
- Smart compression heuristics (skips small files <1MB and pre-compressed formats)
- Extension detection for 30+ pre-compressed formats (jpg, mp4, zip, pdf, etc.)

### Testing
- 11 compression tests added (roundtrip validation, ratio verification)
- Total test count: 182 tests

### Technical
- LZ4 compression: ~400-500 MB/s throughput
- Zstd compression: Better ratio (level 3, balanced)
- Format detection prevents double-compression

## [0.0.6] - 2025-10-01

### Added
- Streaming delta generation with constant ~256KB memory usage
- Delta sync now works with files of any size without memory constraints
- Integration with SSH transport for remote delta sync

### Performance
- **Memory improvement**: 10GB file uses 256KB instead of 10GB RAM (39,000x reduction)
- Constant memory usage regardless of file size

### Testing
- Streaming delta generation validated
- Total test count: 171 tests

## [0.0.5] - 2025-09-30

### Fixed
- **CRITICAL**: Fixed O(n) rolling hash bug
  - Root cause: Using `Vec::remove(0)` which is O(n), not O(1)
  - Solution: Removed unnecessary `window` field from `RollingHash` struct
  - Impact: 6124x performance improvement in rolling hash operations

### Performance
- Verified true O(1) performance: 2ns per operation across all block sizes
- Rolling hash now truly constant time (not dependent on block size)
- Benchmarks confirm consistent 2ns for 4KB, 64KB, and 1MB blocks

### Documentation
- Added detailed optimization history in `docs/OPTIMIZATIONS.md`
- Documented the O(n) bug and its fix with benchmarks

## [0.0.4] - 2025-09-30

### Added
- Parallel file transfers (5-10x speedup for multiple files)
- Thread-safe statistics tracking with `Arc<Mutex<>>`
- Semaphore-based concurrency control
- Error collection and reporting for parallel operations
- `--parallel` / `-j` flag to control worker count (default: 10)

### Changed
- SyncEngine now executes file operations in parallel
- Progress bar updates from multiple threads safely
- Statistics accumulated across parallel workers

### Performance
- 5-10x speedup for syncing multiple files
- Semaphore prevents resource exhaustion
- Configurable parallelism for different workloads

### Testing
- Validated parallel execution correctness
- Thread-safe statistics verified
- Total test count: 160 tests

## [0.0.3] - 2025-09-29

### Added
- **Delta Sync Implementation** - Full rsync algorithm for efficient file updates
  - Adler-32 rolling hash for fast block matching
  - xxHash3 strong checksums for block verification
  - Adaptive block size calculation (√filesize, capped 512B-128KB)
  - Partial block matching for file-end edge cases
  - Compression ratio reporting (% literal data transferred)
- SSH transport implementation (SFTP-based)
- SSH config integration (~/.ssh/config support)
- SSH authentication (agent, identity files, default keys)
- Remote path parsing (user@host:/path format)
- DualTransport for mixed local/remote operations
- TransportRouter for automatic local/SSH transport selection
- Atomic file updates via temp file + rename pattern
- `sync_file_with_delta()` method in Transport trait

### Changed
- File updates now use delta sync instead of full copy when beneficial
- Transferrer now calls `sync_file_with_delta()` for file updates
- SyncEngine now generic over Transport trait
- sync() method is now async
- main() uses tokio runtime (#[tokio::main])
- Module structure: added `mod delta;` to binary crate

### Fixed
- SSH session blocking mode issue (handshake failures)
- Cargo module resolution issue preventing delta module access
- Edge case: Block count calculation for partial blocks
- Edge case: Partial block matching at file end
- Update action now properly detected for existing files

### Performance
- **50MB file with 1KB change**: Delta sync transfers only changed blocks (0.0% literal data)
- **Bandwidth savings**: Dramatically reduced for incremental updates
- Delta sync enabled for all remote operations by default

### Testing
- Added 21 delta sync tests
- Tests cover: block size, rolling hash, checksums, delta generation, delta application
- End-to-end validation for local and remote scenarios
- Total test count: 64 tests

### Technical
- **Delta Module Structure**:
  - `delta/mod.rs` - Block size calculation
  - `delta/rolling.rs` - Adler-32 rolling hash
  - `delta/checksum.rs` - xxHash3 strong checksums + block metadata
  - `delta/generator.rs` - Delta operation generation (Copy/Data ops)
  - `delta/applier.rs` - Delta application with temp file atomicity
- **Algorithm**: Classic rsync (Andrew Tridgell 1996) with modern hashes
- **Hash Map Lookup**: O(1) weak hash lookup, strong hash verification on collision

### Dependencies
- Added async-trait, tokio, ssh2, serde_json, tempfile
- Added whoami, dirs, regex for SSH config parsing

## [0.0.2] - 2025-09-28

### Added
- Streaming file transfers with fixed memory usage (128KB chunks)
- xxHash3 checksum calculation for all file transfers
- Checksum logging in debug mode for verification
- Transport abstraction layer for local and remote operations
- LocalTransport implementation wrapping Phase 1 functionality
- Async Transport trait for future SSH/SFTP support

### Changed
- LocalTransport now uses buffered streaming instead of `fs::copy()`
- Memory usage is now constant regardless of file size

### Fixed
- OOM issues with large files (>1GB) resolved
- All file transfers now verifiable via checksums

### Performance
- Constant memory usage for files of any size
- Efficient streaming with 128KB buffer

## [0.0.1] - 2025-09-27

### Added
- **Core Functionality**
  - Basic local directory synchronization
  - File comparison using size + mtime (1s tolerance)
  - Full file copy with modification time preservation
  - Progress bar display (indicatif)
  - Dry-run mode (`--dry-run` / `-n`)
  - Delete mode (`--delete`)
  - Quiet mode (`--quiet` / `-q`)
  - Verbose logging (`-v`, `-vv`, `-vvv`)

- **File Handling**
  - `.gitignore` pattern support (respects .gitignore files in git repos)
  - Automatic `.git` directory exclusion
  - Hidden files support (synced by default)
  - Empty directory preservation
  - Nested directory structures
  - Unicode and special character filenames
  - Binary file support
  - Large file handling (tested up to 10MB)
  - Zero-byte file support

- **Platform Optimizations**
  - macOS: `clonefile()` for fast local copies
  - Linux: `copy_file_range()` for efficient transfers
  - Fallback: standard buffered copy

- **Testing** (49 tests total)
  - **Unit Tests (15)**: CLI validation, scanner, strategy, transfer modules
  - **Integration Tests (11)**: End-to-end workflows, error handling
  - **Property-Based Tests (5)**: Idempotency, completeness, correctness
  - **Edge Case Tests (11)**: Empty dirs, unicode, deep nesting, large files
  - **Performance Regression Tests (7)**: Ensure performance stays within bounds

- **Development**
  - Comprehensive error handling with thiserror
  - Structured logging with tracing
  - CLI argument parsing with clap
  - Benchmarks for basic operations (criterion)
  - GitHub Actions CI/CD (test, clippy, fmt, security audit, coverage)
  - Cross-platform support (Linux, macOS, Windows)

- **Documentation**
  - Complete design document (2,400+ lines)
  - User-facing README with examples
  - Contributing guidelines
  - AI development context (.claude/CLAUDE.md)
  - Inline code documentation

### Performance
- **100 files**: 40-79% faster than rsync/cp
- **Large files (50MB)**: 64x faster than rsync, 7x faster than cp
- **Idempotent sync**: 4.7x faster than rsync
- **1000 files**: 40-47% faster than alternatives

### Technical Details
- **Architecture**: Scanner → Strategy → Transfer → Engine
- **Dependencies**: walkdir, ignore, clap, indicatif, tracing, thiserror, anyhow
- **Code Quality**: All clippy warnings fixed, formatted with rustfmt

### Known Limitations
- Phase 1 only supports local sync (no network/SSH)
- No delta sync (copies full files)
- No compression
- No parallel transfers
- Permissions not fully preserved (future enhancement)
- No symlink support (planned for Phase 6)

---

**Key Milestones:**
- ✅ Phase 1: MVP (v0.0.1) - Basic local sync
- ✅ Phase 2: Network + Delta (v0.0.2-v0.0.3) - SSH transport + rsync algorithm
- ✅ Phase 3: Parallelism + Optimization (v0.0.4-v0.0.8) - Parallel transfers + optimizations
- 🚧 Phase 4: Advanced Features (v0.1.0+) - Network detection, compression, resume

[Unreleased]: https://github.com/nijaru/sy/compare/v0.0.8...HEAD
[0.0.8]: https://github.com/nijaru/sy/releases/tag/v0.0.8
[0.0.7]: https://github.com/nijaru/sy/releases/tag/v0.0.7
[0.0.6]: https://github.com/nijaru/sy/releases/tag/v0.0.6
[0.0.5]: https://github.com/nijaru/sy/releases/tag/v0.0.5
[0.0.4]: https://github.com/nijaru/sy/releases/tag/v0.0.4
[0.0.3]: https://github.com/nijaru/sy/releases/tag/v0.0.3
[0.0.2]: https://github.com/nijaru/sy/releases/tag/v0.0.2
[0.0.1]: https://github.com/nijaru/sy/releases/tag/v0.0.1
