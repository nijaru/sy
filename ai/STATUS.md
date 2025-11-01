# Status

_Last Updated: 2025-10-31_

## Current State
- Version: v0.0.56 (READY FOR RELEASE - 2025-10-31) ✅
- Current Work: **Critical trait dispatch bug fixed** - Arc<T> Transport impl completed
- Test Coverage: **603 tests total (100% passing)** ✅
  - **Local tests**: 555 passing
    - Library tests (sy): 465
    - Main tests: 3
    - Integration tests: 83 (change ratio, compression, delta sync, edge cases, hard links, per-file progress, performance, property tests)
    - Doc tests: 4
  - **SSH tests**: 48 passing (validated against fedora via SSH)
    - ssh_comprehensive_test.rs: 21/21 ✅ (operations, transfers, errors, edge cases, large scale)
    - ssh_bisync_test.rs: 9/9 ✅ (bidirectional sync scenarios)
    - ssh_sparse_hardlink_test.rs: 7/7 ✅ (sparse files, hard links)
    - ssh_resume_retry_test.rs: 6/6 ✅ (retry with backoff)
    - ssh_remote_to_local_progress_test.rs: 5/5 ✅ (per-file progress)
  - **SSH Performance**: 20-33 MB/s transfers, 100 files in ~11s
- Build: Passing (all tests green)
- Performance: 1.3x - 8.8x faster than rsync; sparse files: up to 10x faster (see docs/PERFORMANCE.md)
- Memory: 100x reduction for large file sets (1.5GB → 15MB for 100K files)

### 🔧 v0.0.53 Critical Fixes & v0.0.54 Proper Solutions (2025-10-31)

**v0.0.53 Release (SHIPPED)** - Hotfix for SSH sync failures:
- Fixed 6 critical bugs preventing SSH sync to remote destinations
- **Temporary solutions**: Skip operations that can't be done locally
  - ⚠️ File verification: Skipped for remote (NO corruption detection)
  - ⚠️ Disk space checks: Skipped for remote (NO out-of-space protection)
  - ⚠️ Xattrs/ACLs/BSD flags: Skipped for remote (NO preservation)
- **Why shipped**: Unblocked users who couldn't sync at all
- **Trade-off**: Basic SSH sync works, but missing critical features

**v0.0.54 Release (SHIPPED)** - Critical data integrity fix:
1. ✅ **File verification** (CRITICAL - data integrity) - **COMPLETED**
   - Implemented proper transport.read_file() for remote verification
   - Added read_file() delegation to DualTransport and TransportRouter
   - Remote files now verified via SFTP with same checksums as local
   - **Tested**: 10/10 files verified on macOS → Linux SSH sync
   - **Impact**: Restores corruption detection for SSH syncs

**v0.0.55 Release (SHIPPED)** - Complete remote operations:
1. ✅ **Disk space checks** - **COMPLETED**
   - Executes `df -P -B1` via SSH to check remote space
   - Prevents out-of-space failures mid-sync
   - Uses POSIX format for Linux/macOS compatibility
2. ✅ **Xattrs/ACLs/BSD flags** - **COMPLETED**
   - Xattrs: Platform-specific (setfattr on Linux, xattr on macOS)
   - ACLs: `setfacl -M` via SSH with stdin piping
   - BSD flags: `chflags` for macOS file attributes
   - All operations gracefully warn on failure without blocking sync

**v0.0.56 Release (READY)** - Critical trait dispatch fix:
- **Root Cause**: Arc<T> Transport impl was missing several methods (check_disk_space, set_xattrs, set_acls, set_bsd_flags)
- **Impact**: When these methods were called on Arc-wrapped transports, they fell back to default trait impl
- **Symptom**: "No such file or directory (os error 2)" - tried to check remote path on local filesystem
- **Fix**: Added all missing methods to Arc<T> impl to properly delegate (commit: b956aa3)
- **Shell-agnostic commands**: Also fixed fish shell compatibility for disk space checks (commit: ee128bd)
- **Tested**: Successfully synced 436-file project from macOS → Linux via SSH/Tailscale

**Installation**: Now available via prebuilt binary for Apple Silicon!
- Homebrew: `brew install nijaru/tap/sy` (1 second install, no Rust needed)
- Crates.io: `cargo install sy`
- GitHub: https://github.com/nijaru/sy/releases/latest

**Architecture Pattern**:
- Problem: v0.0.53 checked `if !path.exists()` → skip operation
- Solution: v0.0.54 uses transport layer for remote operations
- Next: v0.0.55 will complete remaining remote operations

### ✅ Medium-Priority Safety Features COMPLETE (2025-10-29)
**Goal**: Close critical test gaps and improve data safety

**Completed Features**:

1. **State Corruption Recovery** (commit: 2c39cd0)
   - Added StateCorruption error type with recovery instructions
   - Implemented validate_state_file() with 9 validation checks
   - Added --force-resync CLI flag for recovery
   - Clear error messages guide users to recovery
   - 9 comprehensive tests passing
   - Impact: Graceful recovery from corrupted state instead of mysterious failures

2. **Concurrent Sync Safety** (commit: a3811f2)
   - File-based locking prevents simultaneous syncs to same directory pair
   - SyncLock RAII guard with automatic cleanup
   - Lock files in ~/.cache/sy/locks/ with PID tracking
   - Cross-platform support via fs2 crate
   - 5 comprehensive tests passing
   - Impact: Data safety - prevents race conditions and corruption

3. **Hard Link Handling Tests** (commit: c256733)
   - 7 comprehensive tests for hard link detection and bisync behavior
   - Documents current behavior: hard links NOT preserved (become independent files)
   - Bisync correctly handles hard links without false conflicts
   - Modifications detected and synced properly
   - Impact: Verified correct behavior with hard links (dev environments, node_modules)

**Test Script**: test-safety-features.sh runs all safety tests (commit: ed754b7)

### ✅ Per-File Progress Feature COMPLETE (2025-10-29)
**Goal**: Improve UX for large file transfers

**Implementation** (commits: 28491f7, fff6be9, bc7d8fe, 4ec46d2):

1. **Per-File Progress Bars** (28491f7)
   - Real-time progress bars for files >= 1MB
   - Uses indicatif with automatic TTY detection
   - Shows: filename, progress bar, speed (MB/s), percentage, ETA
   - Format: `filename.txt [=====>] 45MB/100MB (45%) 12MB/s ETA: 4s`
   - Leverages existing copy_file_streaming infrastructure
   - Works with SSH remote transfers

2. **Smart Behavior** (fff6be9)
   - Respects --quiet flag (disabled when quiet=true)
   - Auto-hides when output is piped/redirected
   - Opt-in via --per-file-progress flag
   - Clear documentation in CLI help

3. **Test Fixes** (bc7d8fe)
   - Added serial_test crate for proper test isolation
   - Fixed 8 tests with XDG_CACHE_HOME pollution
   - All 463 tests passing ✅

4. **Local Streaming** (4ec46d2)
   - Implemented copy_file_streaming() for LocalTransport
   - Streams files in 1MB chunks (no memory issues for large files)
   - Real-time progress callbacks after each chunk
   - Added 2 comprehensive tests for streaming
   - All 465 tests passing ✅

5. **Comprehensive Test Suite** (16111e3) - **Production-Ready** ✅
   - **24 new tests** covering all real-world scenarios
   - **Integration tests** (8 tests - `tests/per_file_progress_test.rs`):
     - Progress shown for files >= 1MB
     - Progress hidden for files < 1MB
     - Multiple large files with sequential progress
     - Mixed file sizes (small + large)
     - Very large files (100MB+)
     - Nested directories
     - File integrity verification
   - **Edge case tests** (11 tests - `tests/per_file_progress_edge_cases.rs`):
     - Empty files (0 bytes), exact chunk boundaries, odd file sizes
     - Progress never exceeds total, monotonic increase
     - Binary data integrity, mtime preservation
     - Concurrent file transfers (parallel workers)
     - Read-only source files
     - Parent directory creation
   - **SSH tests** (5 tests - `tests/ssh_remote_to_local_progress_test.rs`):
     - Remote → Local downloads (2MB, 10MB, 5MB files)
     - Progress monotonic increase verification
     - Connection pooling with concurrent transfers
     - IGNORED by default (run with: `cargo test --test ssh_remote_to_local_progress_test -- --ignored`)
     - Note: Local → Remote tested via CLI (requires DualTransport setup)
   - **Test isolation fixes**: Added `#[serial]` to 3 more state tests
   - **Total**: 484 tests passing + 5 SSH tests (manual)

**Usage**:
```bash
sy /source /dest --per-file-progress  # Show progress in TTY
sy /source /dest --per-file-progress --quiet  # Hidden (quiet wins)
sy /source /dest --per-file-progress | tee log.txt  # Auto-hidden
```

### ✅ v0.0.52 COMPLETE (Performance at Scale)
**Goal**: Optimize memory usage and performance for large-scale file synchronization (100K+ files)

**Released**: 2025-10-28
- ✅ Published to crates.io: https://crates.io/crates/sy/0.0.52
- ✅ GitHub release: https://github.com/nijaru/sy/releases/tag/v0.0.52
- ✅ Release notes: release_notes_v0.0.52.md

**Optimizations** (commits: ba302b1, 0000261, 7f863e0):
1. **Arc<PathBuf> in FileEntry** (ba302b1)
   - O(1) cloning instead of O(n) memory allocation
   - Eliminates ~152MB allocations for 1M files
   - Changed path, relative_path, symlink_target to Arc<PathBuf>

2. **Arc<FileEntry> in SyncTask** (0000261)
   - Tasks passed by 8-byte pointer instead of 152+ byte struct copy
   - Eliminates ~152MB task allocations for 1M files

3. **HashMap capacity hints** (7f863e0)
   - Pre-allocate HashMaps/HashSets in hot paths
   - 30-50% faster map construction
   - Applied to bisync classifier and strategy planner

**Measured Impact**:
- 100K files: 1.5GB → 15MB memory usage (100x reduction)
- Planning phase: 50-100% faster
- All 444 tests passing
- Full backward compatibility maintained

**Cross-Project Analysis**:
- Analyzed kombrucha for similar optimization opportunities
- Created OPTIMIZATIONS.md in kombrucha repo with 3 concrete recommendations
- Estimated 5-10% performance gain with ~30 min effort

**Production Hardening** (commit: 3588dac):
- Added comprehensive test suites for large files and massive directories
- tests/large_file_test.rs: 7 tests (100MB, 500MB, 1GB files)
- tests/massive_directory_test.rs: 8 tests (1K, 10K, 100K files)
- All 10 production tests passing:
  - 500MB file sync: 4.53s
  - 10K files sync: 1.9s
  - Idempotent 100MB: 11ms (extremely fast metadata check)
  - Idempotent 10K files: 267ms
  - Nested directories (100 dirs × 100 files): 1.98s
  - Deletion planning with safety checks: 1.88s
  - Progress tracking: accurate for all scales
- Verified: O(1) idempotent sync, accurate progress tracking, proper deletion safety

### ✅ v0.0.51 COMPLETE (Resume Integration)
**Goal**: Integrate TransferState into file transfers for automatic resume of interrupted large files

**✅ Phase 1: SFTP Streaming Resume** (commit: e155c00)
- Remote→Local transfers (copy_file_streaming)
- SFTP seek to resume offset
- Local file append mode
- Checkpoint saves every 10MB
- All 444 library tests passing

**✅ Phase 2: Local→Remote Resume** (commit: 1ca5b5e)
- Local→Remote transfers (copy_file SFTP path)
- Local file seek + remote SFTP seek
- Checkpoint saves every 10MB
- Bidirectional resume complete

**Implementation Details:**
- TransferState loaded before each transfer starts
- Remote→Local: `remote_file.seek()` + local append mode
- Local→Remote: local `source_file.seek()` + `sftp.open_mode()` with WRITE + remote seek
- Checkpoint saves every 10MB (10 * 1MB chunks)
- Resume state cleared on successful completion
- User feedback shows resume progress percentage
- Staleness detection via mtime comparison
- Atomic state writes (temp + rename)

**Impact**: sy now automatically resumes interrupted large file transfers in both directions!

**See**: ai/research/resume_integration_v0.0.51.md for full design

### ✅ v0.0.50 COMPLETE (Network Recovery Activation)
**Goal**: Activate retry infrastructure built in v0.0.49, making it functional in production

**✅ Phase 1: All SSH/SFTP Operations Use Retry** (commits: cc9f9aa, ff3372b, b16399a)
- Converted 14 operations to use retry_with_backoff:
  - Command ops: scan, exists, create_dir_all, remove, create_hardlink, create_symlink
  - SFTP ops: read_file, write_file, get_mtime, file_info, copy_file_streaming
  - Transfer ops: copy_file, copy_sparse_file, sync_file_with_delta
- Replaced spawn_blocking + execute_command pattern with retry-wrapped async calls
- Automatic retry with exponential backoff (1s → 2s → 4s → 8s, capped at 30s)
- Intelligent error classification (retryable vs fatal)
- All 957 tests passing
- Zero overhead when operations succeed

**Deferred to Future Releases:**
- Phase 2 (Health checks): Reactive retry via Phase 1 is sufficient
- Phase 3 (Resume integration): Deserves focused release with dedicated testing (v0.0.51+)

**Impact**: sy now automatically recovers from network interruptions without user intervention!

### ✅ v0.0.49 COMPLETE (Network Interruption Recovery Infrastructure)
**Goal**: Build infrastructure for retry and resume capabilities

**✅ Phase 1: Error Classification** (commit: 3e533a2)
- Added 4 network error types: NetworkTimeout, NetworkDisconnected, NetworkRetryable, NetworkFatal
- Implemented is_retryable() and requires_reconnection() helper methods
- Added from_ssh_io_error() to classify IO errors based on ErrorKind
- 12 comprehensive tests for error classification logic

**✅ Phase 2: Retry Logic with Exponential Backoff** (commit: 3e533a2)
- Created retry module with RetryConfig and retry_with_backoff()
- Configurable max attempts (default: 3), initial delay (default: 1s), backoff multiplier (2.0)
- Added --retry and --retry-delay CLI flags
- 9 tests for retry logic (success, failure, exhaustion, non-retryable errors)

**✅ Phase 3: Resume Capability** (commit: d266d9d)
- Created resume module with TransferState for chunked file transfer tracking
- Store resume state in ~/.cache/sy/resume/<hash>.json (hash = blake3(source + dest + mtime))
- Support 1MB default chunks with configurable size
- Automatic staleness detection (reject resume if file modified)
- CLI flags: --resume-only, --clear-resume-state
- 10 comprehensive tests for state management and chunking

**✅ Phase 4: Integration** (commit: 15789e5)
- Integrated retry config into TransportRouter and SshTransport
- All SSH operations convert IO errors to network errors for proper classification
- CLI args (--retry, --retry-delay) wired through to SSH transport layer
- Ready for automatic retry on network failures
- Foundation complete for connection pool resilience (future enhancement)

### ✅ v0.0.48 RELEASE (2025-10-27)
**Two Critical Fixes from Comprehensive Testing**

1. **Remote→Remote Bidirectional Sync** (feat: 6d9474d)
   - Previously: Explicitly rejected with "Remote-to-remote sync not yet supported"
   - Now: Dual SSH connection pools enable syncing between two SSH hosts
   - Testing: Mac→Fedora→Fedora with bidirectional changes (3 files, 59 bytes in 472ms)
   - Example: `sy -b user@host1:/path user@host2:/path`

2. **.gitignore Support Outside Git Repos** (fix: 6453681)
   - Previously: Patterns ignored in bisync (*.tmp, *.log, node_modules/ all synced)
   - Root cause: `ignore` crate's `git_ignore(true)` only works in git repos
   - Fix: Explicit `WalkBuilder::add_ignore()` for .gitignore files everywhere
   - Testing: 2 files synced vs 5 before (patterns now respected)

**Comprehensive Testing**: 23 scenarios tested over 2 hours (Mac ↔ Fedora), now 100% pass rate (up from 91.3%)

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
- ✅ SSH bidirectional sync (v0.0.46-v0.0.48) - bisync works with remote servers (local↔remote: v0.0.46, remote↔remote: v0.0.48)!

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
- **Network error classification** (2025-10-27, v0.0.49): Systematic classification of IO errors into retryable vs fatal types using ErrorKind pattern matching; helper methods (is_retryable, requires_reconnection, from_ssh_io_error) provide clean API; comprehensive test coverage (12 tests) ensures correct classification for all error types; foundation for robust network interruption handling
- **Retry with exponential backoff** (2025-10-27, v0.0.49): Generic retry_with_backoff() function with configurable delays and backoff multiplier enables resilient network operations; CLI integration (--retry, --retry-delay flags) gives users control; 9 tests verify success, failure, exhaustion, and non-retryable scenarios; clean separation of retry logic from business logic promotes reuse

## What Didn't Work
- **Bisync state storage (2025-10-27, v0.0.46)**: Initial implementation only stored destination side state after copy operations, not both sides; caused deletions to be misclassified as "new files" and copied back instead of propagating; fixed by storing both source AND dest states after any copy
- **SSH bisync write_file missing** (2025-10-27, v0.0.46): SshTransport didn't implement write_file(), falling back to LocalTransport default which writes locally; bisync silently failed - reported success but files never reached remote; caught by real-world testing Mac↔Fedora; fixed in v0.0.47
- QUIC transport: 45% slower than TCP on fast networks (>600 Mbps) - documented in DESIGN.md
- Compression on local/high-speed: CPU bottleneck negates benefits above 4Gbps
- Initial sparse file tests: Had to make filesystem-agnostic due to varying FS support
- macOS APFS sparse detection: SEEK_DATA/SEEK_HOLE not reliably supported; tests must be ignored on APFS
- SSH ControlMaster for parallel transfers: Bottlenecks all transfers on one TCP connection; defeats purpose of parallel workers

## Active Work
- **Network Interruption Recovery (v0.0.49)**
  - ✅ Phase 1: Error classification (NetworkTimeout, NetworkDisconnected, NetworkRetryable, NetworkFatal)
  - ✅ Phase 2: Retry logic with exponential backoff (--retry, --retry-delay flags)
  - ⏳ Phase 3: Resume capability for interrupted transfers
  - ⏳ Phase 4: Connection pool resilience (auto-reconnect)
  - Design: ai/research/network_interruption_recovery_2025.md
  - Next: Implement chunked transfers with resume state tracking

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
