# Status

_Last Updated: 2025-10-27_

## Current State
- Version: v0.0.47-dev (fix ready, not yet released)
- Phase: SSH bidirectional sync - **FIXED** ✅
- Test Coverage: 410 tests passing (402 + 8 bisync state tests, 12 ignored)
- Build: Passing (0 warnings, all tests green)
- Performance: 1.3x - 8.8x faster than rsync; sparse files: up to 10x faster (see docs/PERFORMANCE.md)

### ✅ CRITICAL BUG FIXED (v0.0.47-dev)
**SSH Bidirectional Sync Now Works**
- **v0.0.46 Issue**: `SshTransport` missing `write_file()` implementation - bisync reported success but files never written to remote
- **v0.0.47 Fix**: Implemented `write_file()` for SSH transport (src/transport/ssh.rs:1244-1332)
- **Testing**: All 8 real-world SSH bisync tests pass (Mac ↔ Fedora over Tailscale)
- **Verification**: Deletion propagation, conflict resolution, large files (10MB @ 8.27 MB/s), dry-run all work
- **Status**: Ready for v0.0.47 release

## Implemented Features
- ✅ Local and remote (SSH) sync
- ✅ Delta sync with COW optimization (v0.0.23)
- ✅ Filesystem-aware strategy selection
- ✅ Hard link preservation
- ✅ Parallel file transfers
- ✅ Compression (zstd) with content-based auto-detection (v0.0.37)
- ✅ Progress display with colors
- ✅ Gitignore awareness
- ✅ JSON output
- ✅ Config profiles
- ✅ Watch mode
- ✅ Resume support
- ✅ Performance monitoring (--perf flag, v0.0.33)
- ✅ Comprehensive error reporting (v0.0.34)
- ✅ Pre-transfer checksums (--checksum flag, v0.0.35) - local→local, saves bandwidth!
- ✅ Checksum database (--checksum-db flag, v0.0.35) - 10-100x faster re-syncs!
- ✅ Verify-only mode (--verify-only flag, v0.0.36) - audit integrity, JSON output, exit codes!
- ✅ Compression auto-detection (--compression-detection flag, v0.0.37) - content sampling, 10% threshold!
- ✅ Enhanced progress display (v0.0.38) - byte-based progress, transfer speed, current file!
- ✅ Bandwidth utilization (--perf + --bwlimit, v0.0.39) - shows % utilization in summary and JSON!
- ✅ Symlink loop detection (v0.0.40) - safe symlink traversal with automatic cycle detection!
- ✅ BSD file flags preservation (--preserve-flags/-F flag, v0.0.41) - macOS hidden, immutable, nodump flags!
- ✅ Bidirectional sync (--bidirectional/-b flag, v0.0.43) - two-way sync with conflict resolution!
- ✅ Text-based state tracking (v0.0.44) - persistent state in ~/.cache/sy/bisync/ for accurate conflict detection!
- ✅ 6 conflict resolution strategies (v0.0.43) - newer/larger/smaller/source/dest/rename!
- ✅ SSH bidirectional sync (v0.0.46) - bisync now works with remote servers (local↔remote, remote↔remote)!

## What Worked
- **Local delta sync optimization** (v0.0.23): Using simple block comparison instead of rolling hash for local→local sync achieved 5-9x speedup
- **COW-aware strategies**: Automatic filesystem detection and strategy selection prevents data corruption
- **Performance monitoring**: Arc<Mutex<PerformanceMonitor>> with atomic counters provides thread-safe metrics without overhead
- **Error collection**: Collecting errors in Vec<SyncError> during parallel execution gives users comprehensive view of all failures
- **Documentation reorganization**: Following agent-contexts v0.1.1 patterns with docs/architecture/ and ai/ separation provides clear structure and knowledge graduation path
- **Comprehensive documentation**: Documenting new features (--perf, error reporting) immediately after implementation helps users discover and use them
- **Pre-transfer checksums** (v0.0.35): Computing xxHash3 checksums during planning phase before transfer saves bandwidth on re-syncs and detects bit rot
- **Checksum database** (v0.0.35): SQLite-based persistent cache with mtime+size validation achieves 10-100x speedup on re-syncs by eliminating redundant I/O
- **Verify-only mode** (v0.0.36): Read-only integrity audit with structured JSON output and exit codes enables automation and monitoring workflows
- **Compression auto-detection** (v0.0.37): Content sampling with LZ4 (BorgBackup approach) provides accurate compressibility detection with minimal overhead (~3μs per file)
- **Enhanced progress display** (v0.0.38): Byte-based progress with transfer speed and current file provides better UX and more accurate ETA than file-count-based approach
- **Bandwidth utilization** (v0.0.39): Performance metrics including bandwidth % now available in JSON output for automation; was already working in --perf mode
- **Symlink loop detection** (v0.0.40): Leveraging walkdir's built-in ancestor tracking for loop detection avoids custom DFS implementation; simpler and more reliable than manual cycle detection
- **Performance optimization** (v0.0.40): Eliminated String allocation in is_compressed_extension (10,000 allocations saved for 10K files); comprehensive benchmark analysis shows NO regressions
- **Sparse file module** (v0.0.40): Foundation laid with detect_data_regions using SEEK_HOLE/SEEK_DATA; infrastructure ready for future SSH sparse transfer (~8h remaining work)
- **BSD file flags preservation** (v0.0.41): macOS-specific flag preservation using chflags() syscall; explicitly clears flags when not preserving to prevent auto-preservation; comprehensive tests for both preservation and clearing behaviors
- **Cross-platform BSD flags compilation** (v0.0.41): Removed all `#[cfg(target_os = "macos")]` from preserve_flags usage sites (24+ locations); field definitions unconditional with runtime checks in helpers; enables compilation on Linux/Windows while maintaining macOS-only runtime behavior
- **macOS Finder tags** (v0.0.16+): Already supported via xattr preservation (`-X` flag); tags stored as `com.apple.metadata:_kMDItemUserTags` xattr; works seamlessly with existing infrastructure
- **macOS resource forks** (v0.0.16+): Already supported via xattr preservation; stored as `com.apple.ResourceFork` xattr on modern macOS; AppleDouble format for legacy compatibility
- **Windows strategy** (v0.0.41+): Focus on core rsync advantages (native binary, delta-transfer, SSH) rather than Windows-specific features (ACLs/attributes); fills gap where Robocopy lacks delta-transfer and SSH support
- **SSH multiplexing research** (v0.0.41+): ControlMaster NOT recommended for sy's parallel file transfers (bottlenecks on one TCP connection); better approach is SSH connection pooling (N connections = N workers) for true parallel throughput; see ai/research/ssh_multiplexing_2025.md
- **COW strategy edge case tests** (v0.0.41+): Added 11 comprehensive edge case tests for filesystem detection (non-existent paths, parent/child relationships, symlinks, 3-way hard links); all edge cases handle errors gracefully by returning false (conservative approach)
- **Testing improvements** (2025-10-23): Added 24 comprehensive tests across 3 modules (9 perf accuracy, 4 error threshold, 11 sparse edge cases); test coverage increased from 355 to 377 tests; all quality assurance tests now in place
- **SSH connection pooling** (2025-10-23): Implemented connection pool with N sessions = N workers for true parallel SSH transfers; avoids ControlMaster bottleneck (which serializes on one TCP connection); round-robin distribution via atomic counter; pool size automatically matches --parallel flag; 5 new unit tests added
- **SSH sparse file transfer** (2025-10-23): Implemented automatic sparse file detection and transfer over SSH; detects data regions using SEEK_HOLE/SEEK_DATA, sends only actual data (not holes), reconstructs sparse file on remote; achieves 10x bandwidth savings for VM images, 5x for databases; auto-detection on Unix (allocated_size < file_size); graceful fallback if sparse detection fails; 3 new integration tests
- **Bidirectional sync research** (2025-10-24): Analyzed rclone bisync, Unison, Syncthing; snapshot-based state tracking (rclone approach) chosen over vector clocks for simplicity; covers 80% use cases with ~500 lines vs. 3000+ for full Unison approach
- **Bisync state DB** (2025-10-24): SQLite-based persistent state in ~/.cache/sy/bisync/; stores (path, side, mtime, size, checksum) from prior sync; enables accurate conflict detection without complex algorithms
- **Change classification** (2025-10-24): 9 change types with content equality checks reduce false conflicts; handles edge cases (partial state, missing files) gracefully
- **Conflict resolution strategies** (2025-10-24): 6 automated strategies (newer/larger/smaller/source/dest/rename) with automatic tie-breaker fallback; simpler than Unison's manual reconciliation, more flexible than Syncthing's rename-only
- **Deletion safety** (2025-10-24): Configurable max-delete percentage (default 50%) prevents cascading data loss from bugs or misconfiguration
- **Text-based state format** (2025-10-26): Refactored bisync from SQLite to text files; simpler (~100 lines less code), debuggable (cat ~/.cache/sy/bisync/*.lst), inspired by rclone bisync format; atomic writes with temp+rename
- **Dead code annotations** (2025-10-26): Using #[allow(dead_code)] with explanatory comments for intentional unused code (public APIs, future features, test infrastructure) maintains production quality while preserving library interface and extensibility points
- **SSH bidirectional sync** (2025-10-26): Refactored BisyncEngine to use Transport abstraction; made sync() async; replaced direct std::fs calls with transport.read_file()/write_file(); enables local↔remote and remote↔remote bisync; ~200 lines changed in engine.rs + ~70 lines in main.rs
- **Real-world bisync testing** (2025-10-27): Created comprehensive test suite (bisync_real_world_test.sh) with 7 scenarios; caught critical state storage bug before release; test-driven debugging approach (create minimal failing test → trace through code → identify root cause → fix → verify all tests pass) was highly effective
- **SSH write_file() implementation** (2025-10-27): Discovered v0.0.46 shipped with broken SSH bisync (write_file not implemented); files reported as synced but never written to remote; implemented write_file() using SFTP session with recursive mkdir, file creation, mtime preservation; 89 lines; tested Mac↔Fedora over Tailscale; all 8 comprehensive tests pass including deletion propagation, conflicts, large files (10MB @ 8.27 MB/s), dry-run

## What Didn't Work
- **Bisync state storage (2025-10-27, v0.0.46)**: Initial implementation only stored destination side state after copy operations, not both sides; caused deletions to be misclassified as "new files" and copied back instead of propagating; fixed by storing both source AND dest states after any copy
- **SSH bisync write_file missing** (2025-10-27, v0.0.46): SshTransport didn't implement write_file(), falling back to LocalTransport default which writes locally; bisync silently failed - reported success but files never reached remote; caught by real-world testing Mac↔Fedora; fixed in v0.0.47
- QUIC transport: 45% slower than TCP on fast networks (>600 Mbps) - documented in DESIGN.md
- Compression on local/high-speed: CPU bottleneck negates benefits above 4Gbps
- Initial sparse file tests: Had to make filesystem-agnostic due to varying FS support
- macOS APFS sparse detection: SEEK_DATA/SEEK_HOLE not reliably supported; tests must be ignored on APFS
- SSH ControlMaster for parallel transfers: Bottlenecks all transfers on one TCP connection; defeats purpose of parallel workers

## Active Work
None

## Recently Completed
- ✅ Performance Profiling (2025-10-26, commit d6ab721)
  - Comprehensive code analysis of bisync engine ✅
  - Created bisync_bench.rs for future profiling ✅
  - No bottlenecks found - all O(n) algorithms ✅
  - I/O dominates as expected (not CPU-bound) ✅
  - Documented findings in ai/PROFILING_FINDINGS.md ✅
  - Recommendation: Ship v0.0.46 ✅
- ✅ SSH Bidirectional Sync (2025-10-26, commit b7dd6fb)
  - Refactored BisyncEngine to use Transport abstraction ✅
  - Made sync() async and updated all call sites ✅
  - Replaced std::fs with transport methods (read_file, write_file, remove) ✅
  - Updated CLI to create transports based on path types (local, SSH) ✅
  - Supports local↔local, local↔remote, and remote↔remote bisync ✅
  - All 410 tests passing, 0 warnings ✅
  - Documentation updated (README.md, STATUS.md) ✅
  - Committed and ready for testing ✅
- ✅ Compiler Warning Cleanup (2025-10-26, commit 63a267b)
  - Fixed all 6 compiler warnings with #[allow(dead_code)] annotations ✅
  - ConflictInfo fields (future detailed reporting) ✅
  - BisyncStateDb methods (future state management) ✅
  - Sparse file infrastructure (foundation for optimization) ✅
  - SSH pool API methods (backward compatibility) ✅
  - Build status: 0 warnings, 410 tests passing ✅
  - Committed and pushed to main ✅
- ✅ Bisync Enhancements (2025-10-27, commit 074ec7a)
  - **Conflict History Logging**: Automatic audit trail in ~/.cache/sy/bisync/*.conflicts.log ✅
  - **Format**: timestamp | path | conflict_type | strategy | winner ✅
  - **Winner Resolution**: Intelligent logic for newer/larger/smaller/source/dest/rename strategies ✅
  - **Append-only**: Preserves complete conflict history across syncs ✅
  - **Exclude Patterns**: Confirmed .gitignore support works with bisync ✅
  - **Documentation**: Updated README and BIDIRECTIONAL_SYNC_DESIGN.md ✅
  - **Testing**: All 410 unit tests + 11 real-world bisync tests pass ✅
- ✅ Critical Bisync State Storage Bug Fix (2025-10-27, commit 84f065b)
  - **Issue**: update_state() only stored one side after copy operations ✅
  - **Impact**: Deletions misclassified as "new files", copied back instead of propagating ✅
  - **Fix**: Store both source AND dest states after any copy operation ✅
  - **Testing**: Created bisync_real_world_test.sh with 7 comprehensive scenarios ✅
  - **Results**: All 7 real-world tests pass, all 410 unit tests pass ✅
  - Deletion safety limits now work correctly (blocks 60% deletion at 50% threshold) ✅
  - State persistence across syncs now works correctly ✅
  - Idempotent syncs properly detect no changes ✅
  - Conflict resolution (rename) works correctly ✅
- ✅ v0.0.45 Release - Bisync State Format v2 (2025-10-26)
  - Fixed proper escaping for quotes, newlines, backslashes, tabs ✅
  - Fixed parse error handling (no more silent corruption) ✅
  - Fixed last_sync field (now separate from mtime) ✅
  - Added backward compatibility with v1 format ✅
  - Added 8 comprehensive edge-case tests ✅
  - All 410 tests passing ✅
  - Git tag v0.0.45 created and pushed ✅
  - Published to crates.io ✅
- ✅ v0.0.44 Release - Bisync State Refactoring (2025-10-26)
  - Switched from SQLite to text-based format (.lst files) ✅
  - Format inspired by rclone bisync: human-readable, debuggable ✅
  - Simpler implementation: ~300 lines vs ~400 (removed ~100 lines SQL) ✅
  - Atomic writes with temp file + rename ✅
  - Format spec documented in docs/architecture/BISYNC_STATE_FORMAT.md ✅
  - All 414 tests passing ✅
  - Breaking change: v0.0.43 .db files ignored (clean break) ✅
  - Git tag v0.0.44 created and pushed ✅
  - Published to crates.io ✅
- ✅ v0.0.43 Release - Bidirectional Sync (2025-10-24)
  - Text-based state tracking in ~/.cache/sy/bisync/ (refactored in v0.0.44) ✅
  - 9 change types with content equality checks ✅
  - 6 conflict resolution strategies (newer/larger/smaller/source/dest/rename) ✅
  - Deletion safety with configurable max-delete percentage ✅
  - CLI flags: --bidirectional, --conflict-resolve, --max-delete, --clear-bisync-state ✅
  - Core implementation: ~2,000 lines + 32 tests ✅
  - End-to-end testing: verified working with real file scenarios ✅
  - Documentation: CHANGELOG, README, design docs complete ✅
  - Git tag v0.0.43 created and pushed ✅
  - Published to crates.io ✅
- ✅ v0.0.42 Release - SSH Performance & Sparse File Optimization (2025-10-23)
  - SSH connection pooling (N workers = N connections) ✅
  - SSH sparse file transfer (10x bandwidth savings for VMs) ✅
  - Testing improvements (27 new tests added) ✅
  - Cross-platform compilation fixes (preserve_flags) ✅
  - CI timing test fixes (5 tests ignored for CI) ✅
  - GitHub release published ✅
  - Published to crates.io ✅
- ✅ SSH Sparse File Transfer (2025-10-23) - COMPLETE
  - sy-remote ReceiveSparseFile command ✅
  - SSH transport copy_sparse_file() method ✅
  - Auto-detection in copy_file() (Unix: blocks*512 vs file_size) ✅
  - Graceful fallback to regular transfer ✅
  - 3 comprehensive tests (sy-remote) ✅
  - Test coverage: 382 → 385 tests ✅
  - Protocol: detect regions → send JSON + stream data → reconstruct ✅
  - Bandwidth savings: 10x for VM images, 5x for databases ✅
- ✅ SSH Connection Pooling (2025-10-23)
  - Implemented ConnectionPool with round-robin session distribution ✅
  - Pool size automatically matches --parallel worker count ✅
  - Each worker gets dedicated SSH connection (true parallelism) ✅
  - Avoids ControlMaster TCP bottleneck ✅
  - Added 5 unit tests (atomicity, wrapping, round-robin) ✅
  - Test coverage: 377 → 382 tests ✅
- ✅ Testing Improvements (2025-10-23)
  - Performance monitoring accuracy tests (9 new tests: duration, speed, concurrency) ✅
  - Error collection threshold tests (4 new tests: unlimited, abort, below threshold) ✅
  - Sparse file edge case tests (11 new tests: holes, regions, boundaries) ✅
  - Test coverage increased: 355 → 377 tests (22 new tests added) ✅
- ✅ v0.0.41 Release - macOS BSD File Flags + Cross-Platform Compilation (2025-10-23)
  - BSD flags preservation with --preserve-flags/-F flag ✅
  - Cross-platform compilation fixes (24+ locations) ✅
  - Finder tags documentation (already working via xattrs) ✅
  - Resource forks support (already working via xattrs) ✅
  - SSH multiplexing research (2025 best practices) ✅
  - Windows strategy decision (focus on core strengths) ✅
  - GitHub release published ✅

## Next Steps
**Research Opportunities:**
- Latest filesystem feature detection methods (2025)
- State-of-the-art compression algorithms for file sync

**Feature Candidates:**
- Sparse file optimization improvements (foundation ready in src/sparse.rs)
- Windows-specific features (file attributes, ACLs)
- Multi-destination sync
- Bidirectional sync
- Cloud storage backends expansion (S3, etc.)
- Plugin system architecture

## Blockers
None currently

## Performance Metrics
- Local→Local: 1.3x - 8.8x faster than rsync
- Delta sync (100MB file): ~4x faster (320 MB/s vs 84 MB/s)
- COW strategy (APFS): 5-9x faster than rsync
- Parallel transfers: Scales well with concurrent operations

See docs/PERFORMANCE.md and docs/EVALUATION_v0.0.28.md for detailed benchmarks.
