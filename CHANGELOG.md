# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- Delta sync disabled for local-to-local operations (191x performance improvement)
- LocalTransport now uses direct copy instead of delta sync

### Performance
- **Local sync improvement**: 26.9s → 0.14s (191x faster)
- Root cause: Rolling hash O(n*block_size) overhead exceeds local copy benefit
- Decision: Keep delta sync for remote operations where network cost dominates

## [0.0.3] - 2025-10-02

### Added
- **Delta Sync Implementation** - Full rsync algorithm for efficient file updates
  - Adler-32 rolling hash for fast block matching
  - xxHash3 strong checksums for block verification
  - Adaptive block size calculation (√filesize, capped 512B-128KB)
  - Partial block matching for file-end edge cases
  - Compression ratio reporting (% literal data transferred)
- Delta sync support for LocalTransport (local-to-local sync)
- Delta sync support for SshTransport (local-to-remote sync via SFTP)
- Delta sync support for DualTransport (automatic routing)
- Atomic file updates via temp file + rename pattern
- `sync_file_with_delta()` method in Transport trait

### Changed
- File updates now use delta sync instead of full copy when beneficial
- Transferrer now calls `sync_file_with_delta()` for file updates
- Added `tempfile` as regular dependency (was dev-only)
- Module structure: added `mod delta;` to binary crate for proper resolution

### Fixed
- Cargo module resolution issue preventing delta module access from binary
- Edge case: Block count calculation for partial blocks (test expectation fix)
- Edge case: Partial block matching at file end now works correctly

### Performance
- **50MB file with 1KB change**: Delta sync transfers only changed blocks (0.0% literal data)
- **Bandwidth savings**: Dramatically reduced for incremental updates
- Downloads remote file for checksum computation (future: compute checksums remotely)

### Testing
- Added 21 delta sync tests (total: 64 tests, all passing)
- Tests cover: block size calculation, rolling hash, checksum computation, delta generation, delta application
- End-to-end validation of delta sync for both local and remote scenarios

### Documentation
- Updated README with delta sync features and benefits
- Updated comparison table showing delta sync implemented
- Updated roadmap to show Phase 2 progress (SSH + Delta)
- Added delta sync usage examples

### Technical
- **Delta Module Structure**:
  - `delta/mod.rs` - Block size calculation
  - `delta/rolling.rs` - Adler-32 rolling hash (156 test iterations)
  - `delta/checksum.rs` - xxHash3 strong checksums + block metadata
  - `delta/generator.rs` - Delta operation generation (Copy/Data ops)
  - `delta/applier.rs` - Delta application with temp file atomicity
- **Algorithm**: Classic rsync (Andrew Tridgell 1996) with modern hashes
- **Block Operations**: Copy {offset, size} for matches, Data(Vec<u8>) for literals
- **Hash Map Lookup**: O(1) weak hash lookup, strong hash verification on collision

## [0.0.2] - 2025-10-02

### Added
- Streaming file transfers with fixed memory usage (128KB chunks)
- xxHash3 checksum calculation for all file transfers
- Checksum logging in debug mode for verification

### Changed
- LocalTransport now uses buffered streaming instead of `fs::copy()`
- SshTransport now streams files in chunks instead of loading entire file
- Memory usage is now constant regardless of file size

### Fixed
- OOM issues with large files (>1GB) resolved
- All file transfers now verifiable via checksums

## [0.0.1] - 2025-10-02

### Added
- Transport abstraction layer for local and remote operations
- LocalTransport implementation wrapping Phase 1 functionality
- Async Transport trait for future SSH/SFTP support
- SSH config parser (~/.ssh/config support)
- SSH config struct with all major directives
- SSH connection establishment module (connect.rs)
- SSH authentication (agent, identity files, default keys)
- TCP connection with timeout handling
- SshTransport implementation with remote command execution
- sy-remote helper binary for efficient remote directory scanning
- JSON-based remote protocol for file metadata transfer
- Remote scanning via SSH exec
- SFTP-based file transfer (copy_file method)
- Remote directory creation (create_dir_all)
- Remote file/directory deletion (remove)
- Modification time preservation for remote files
- Remote path parsing (user@host:/path format)
- DualTransport for mixed local/remote operations
- TransportRouter for automatic local/SSH transport selection
- CLI integration for remote sync (sy /local user@host:/remote)
- Windows drive letter support in path parsing
- SshConfig Default implementation
- 11 comprehensive SSH config unit tests
- 6 comprehensive LocalTransport unit tests
- 9 path parsing unit tests
- Performance regression tests (7 tests) with conservative baselines
- Comparative benchmarks against rsync and cp
- Performance optimizations (10% improvement in idempotent sync)
- Future optimization roadmap documentation
- Phase 2 implementation plan (docs/PHASE2_PLAN.md)

### Changed
- SyncEngine now generic over Transport trait
- sync() method is now async
- main() uses tokio runtime (#[tokio::main])
- Transferrer refactored to be generic over Transport and fully async
- StrategyPlanner.plan_file_async now checks metadata for update detection
- TransportRouter uses DualTransport for mixed local/remote operations
- Pre-allocated vectors to reduce allocations
- Skip metadata reads for directory existence checks
- Batched progress bar updates to reduce overhead
- Use enumerate() instead of explicit counter loops (clippy)

### Fixed
- SSH session blocking mode issue (handshake failures)
- Update action now properly detected for existing files
- Local→remote and remote→local sync now work correctly

### Technical
- Added async-trait dependency
- Added tokio rt-multi-thread and time features
- Added whoami, dirs, regex dependencies for SSH config
- Added ssh2 dependency for SSH connectivity
- Added serde_json for JSON remote protocol
- Created sy-remote binary target for remote execution
- Arc<Mutex<Session>> for thread-safe SSH session sharing
- All 77 tests passing (43 unit + 34 integration/property/perf/edge)
- No performance regression

### Performance
- **100 files**: 40-79% faster than rsync/cp
- **Large files (50MB)**: 64x faster than rsync, 7x faster than cp
- **Idempotent sync**: 4.7x faster than rsync (was 4.3x)
- **1000 files**: 40-47% faster than alternatives

### Planned
- Phase 2: Network sync (SSH transport, SFTP fallback)
- Phase 3: Parallel transfers (rayon, async I/O, memory-mapped I/O)
- Phase 4: Delta sync (rsync algorithm)
- Phase 5: Multi-layer checksums and verification

## [0.1.0] - 2025-10-01

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

### Technical Details
- **Architecture**: Scanner → Strategy → Transfer → Engine
- **Dependencies**: walkdir, ignore, clap, indicatif, tracing, thiserror, anyhow
- **Performance**: Handles 1,000+ files efficiently
- **Code Quality**: All clippy warnings fixed, formatted with rustfmt

### Known Limitations
- Phase 1 only supports local sync (no network/SSH)
- No delta sync (copies full files)
- No compression
- No parallel transfers
- Permissions not fully preserved (future enhancement)
- No symlink support (planned for Phase 6)

## [0.0.1] - 2025-09-30

### Added
- Initial project setup
- Comprehensive design documentation
- Project structure
- Basic module scaffolding

[Unreleased]: https://github.com/nijaru/sy/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nijaru/sy/releases/tag/v0.1.0
[0.0.1]: https://github.com/nijaru/sy/releases/tag/v0.0.1
