# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
- 11 comprehensive SSH config unit tests
- 6 comprehensive LocalTransport unit tests
- Performance regression tests (7 tests) with conservative baselines
- Comparative benchmarks against rsync and cp
- Performance optimizations (10% improvement in idempotent sync)
- Future optimization roadmap documentation
- Phase 2 implementation plan (docs/PHASE2_PLAN.md)

### Changed
- SyncEngine now generic over Transport trait
- sync() method is now async
- main() uses tokio runtime (#[tokio::main])
- Pre-allocated vectors to reduce allocations
- Skip metadata reads for directory existence checks
- Batched progress bar updates to reduce overhead
- Use enumerate() instead of explicit counter loops (clippy)

### Technical
- Added async-trait dependency
- Added tokio rt-multi-thread and time features
- Added whoami, dirs, regex dependencies for SSH config
- Added ssh2 dependency for SSH connectivity
- Added serde_json for JSON remote protocol
- Created sy-remote binary target for remote execution
- Arc<Mutex<Session>> for thread-safe SSH session sharing
- All 67 tests passing (33 unit + 34 integration/property/perf/edge)
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
