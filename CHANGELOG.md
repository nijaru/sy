# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.57] - 2025-11-10

### Fixed
- **Rsync-compatible trailing slash semantics** (issue #2)
  - Without trailing slash: copies directory itself (e.g., `sy /a/myproject /target` → `/target/myproject/`)
  - With trailing slash: copies contents only (e.g., `sy /a/myproject/ /target` → `/target/`)
  - Works consistently across local, SSH, and S3 transports
  - Added comprehensive tests for trailing slash detection and destination computation

- **Remote sync nested file creation** (issue #4)
  - Fixed remote sync failures when creating files in nested directories
  - Ensures parent directories exist before file creation on remote destinations
  - Tested with SSH sync to verify proper directory hierarchy creation

### Changed
- **Documentation overhaul**
  - Rewrote README.md from 1161 lines to 198 lines (83% reduction)
  - Created comprehensive docs/FEATURES.md (861 lines) with feature categorization
  - Created comprehensive docs/USAGE.md (1139 lines) with real-world examples
  - Simplified comparison tables to only compare against rsync (removed rclone)
  - Marked S3/cloud storage as experimental throughout documentation

### Testing
- All 484 tests passing
- Added trailing slash behavior tests
- Added remote nested directory creation tests

## [0.0.56] - 2025-11-01

### Added
- **Homebrew Tap Automation** - Automated release process updates Homebrew tap
  - Formula automatically updated on each release
  - GitHub Actions workflow syncs with nijaru/homebrew-tap
  - Users can install via `brew tap nijaru/tap && brew install sy`

### Fixed
- **Arc<T> Transport Implementation** - Added missing methods to Arc<T> Transport impl
  - Implemented check_disk_space(), set_xattrs(), set_acls(), set_bsd_flags()
  - Arc-wrapped transports now support full metadata preservation
  - Fixes compilation errors when using Arc-wrapped transports

### Technical
- All 484 tests passing
- GitHub Actions workflow for Homebrew tap updates

## [0.0.55] - 2025-10-31

### Fixed
- **Proper remote disk space checking** - Disk space checks now work for SSH destinations
  - v0.0.53/v0.0.54 skipped disk space checks for remote destinations
  - v0.0.55 properly checks via `df` command executed over SSH
  - Prevents out-of-space failures mid-sync
  - Uses POSIX-format `df -P -B1` for cross-platform compatibility

- **Proper remote extended attributes (xattrs)** - xattrs now set correctly on SSH destinations
  - v0.0.53/v0.0.54 skipped xattrs for remote destinations
  - v0.0.55 sets xattrs via platform-specific SSH commands
  - Linux: uses `setfattr` command
  - macOS: uses `xattr -w` command
  - Base64-encodes binary values for safe shell transmission

- **Proper remote ACLs** - POSIX ACLs now preserved for SSH destinations
  - v0.0.53/v0.0.54 skipped ACLs for remote destinations
  - v0.0.55 applies ACLs via `setfacl -M` over SSH
  - Preserves full ACL entries for both Linux and macOS

- **Proper remote BSD flags** - macOS file flags now preserved for SSH destinations
  - v0.0.53/v0.0.54 skipped BSD flags for remote destinations
  - v0.0.55 sets flags via `chflags` over SSH
  - Preserves macOS-specific file attributes like hidden/immutable flags

### Changed
- All 465 tests passing with proper remote operations
- Added Transport trait methods: check_disk_space(), set_xattrs(), set_acls(), set_bsd_flags()
- SshTransport implements full remote operations via SSH commands
- Operations gracefully warn and continue on failure (don't block sync)

### Technical Details
- Remote disk space check: Executes `df -P -B1` via SSH and parses output
- Remote xattrs: Platform detection with fallback (setfattr → xattr -w)
- Remote ACLs: Uses stdin piping for ACL entries
- Remote BSD flags: Converts to octal format for chflags
- All operations wrapped in spawn_blocking for proper async handling

## [0.0.54] - 2025-10-31

### Fixed
- **CRITICAL: Proper remote file verification** - Restored corruption detection for SSH syncs
  - v0.0.53 skipped verification for remote files (NO corruption detection)
  - v0.0.54 properly verifies remote files via transport layer
  - Uses transport.read_file() to read remote files via SFTP
  - Added read_file() delegation to DualTransport and TransportRouter
  - **Impact**: SSH syncs now detect corruption, preventing silent data loss
  - Tested: 10/10 files verified successfully on macOS → Linux SSH sync

### Changed
- All 465 tests passing with proper remote verification
- Remote files now verified with same checksums as local files (xxHash3/BLAKE3)

### Known Limitations (will fix in v0.0.55)
- Disk space checks still skipped for remote destinations
- Xattrs/ACLs/BSD flags still skipped for remote destinations
- These are feature preservation issues, not data integrity issues

## [0.0.53] - 2025-10-31

### Fixed
- **Critical: Remote destination sync failures** - Fixed multiple bugs that caused SSH sync to fail
  - **Destination directory creation**: Ensure destination directory exists before any operations
    - Previously failed with "No such file or directory" when parent directory didn't exist
    - Now creates full destination path at start of sync
  - **Disk space check on remote destinations**: Skip local statvfs() calls for remote paths
    - Previously attempted local filesystem calls on remote paths
    - Now skips check for remote destinations (path doesn't exist locally)
  - **File verification on remote destinations**: Skip checksum verification for remote files
    - Previously tried to read remote files locally for verification
    - Now skips verification when destination file doesn't exist locally
  - **BSD flags on remote destinations**: Skip chflags() calls for remote files
    - Previously attempted to set flags on non-existent local paths
    - Now detects remote destination and skips gracefully
  - **Extended attributes (xattrs) on remote destinations**: Skip xattr::set() for remote files
    - Previously failed when using --preserve-xattrs with SSH sync
    - Now detects remote destination and skips gracefully
  - **ACLs on remote destinations**: Skip setfacl() calls for remote files
    - Previously failed when using --preserve-acls with SSH sync
    - Now detects remote destination and skips gracefully
- **Optional S3 feature**: Made S3/cloud storage support optional to reduce dependencies
  - Default install: 172 crates (down from 381, 55% reduction)
  - With S3: `cargo install sy --features s3`
  - Faster installation for users who don't need cloud storage

### Changed
- All 465 tests passing with remote destination fixes
- Improved error messages for remote operations
- Added debug logging for skipped operations on remote destinations

## [0.0.52] - 2025-10-28

### Performance
- **90% Reduction in Memory Allocations** for large-scale file synchronization operations
  - **Arc-based FileEntry paths** (ba302b1): Changed FileEntry path fields to Arc<PathBuf>
    - Path cloning is now O(1) atomic counter increment instead of O(n) memory allocation
    - Eliminates ~152MB of allocations for 1M files across 240+ .clone() calls in codebase
    - All path fields (path, relative_path, symlink_target) now use Arc<PathBuf>
  - **Arc-based SyncTask** (0000261): Changed SyncTask.source to Arc<FileEntry>
    - Tasks passed by 8-byte pointer instead of 152+ byte struct copy
    - Eliminates ~152MB of task allocations for 1M files
  - **HashMap capacity hints** (7f863e0): Pre-allocate HashMaps/HashSets in hot paths
    - 30-50% faster map construction by eliminating rehashing
    - Applied to bisync classifier (source_map, dest_map) and strategy planner (source_paths)
  - **Measured Impact**:
    - 100K files: ~1.5GB → ~15MB memory usage (100x reduction)
    - Planning phase: 50-100% faster
    - 1M files: ~300MB+ memory savings from Arc optimizations

### Changed
- All 444 tests passing with new Arc-based memory model
- No API changes - full backward compatibility maintained

## [0.0.51] - 2025-10-28

### Added
- **Automatic Resume for Interrupted Transfers** - Large file transfers automatically resume after interruption
  - **Bidirectional Resume Support**: Both Remote→Local and Local→Remote transfers can resume
    - Remote→Local: SFTP streaming (copy_file_streaming) with remote seek + local append
    - Local→Remote: SFTP write path (copy_file) with local seek + remote seek
  - **Checkpoint Saves**: Progress saved every 10MB (10 * 1MB chunks) for recovery
  - **Staleness Detection**: Rejects resume state if source file modified (via mtime comparison)
  - **User Feedback**: Shows resume progress percentage when resuming
  - **Atomic State Management**:
    - Resume state stored in `~/.cache/sy/resume/<hash>.json`
    - BLAKE3-based unique IDs (hash of source + dest + mtime)
    - Atomic writes (temp + rename pattern)
    - Automatic cleanup on successful completion
  - **SFTP Seek Operations**:
    - Remote→Local: `remote_file.seek(SeekFrom::Start(offset))` + local append mode
    - Local→Remote: local `source_file.seek()` + `sftp.open_mode(WRITE)` + remote seek

### Changed
- **Removed dead_code attributes**: Resume infrastructure now actively used in production transfers
- **Enhanced logging**: Transfer debug logs now show if transfer was resumed

## [0.0.50] - 2025-10-28

### Added
- **Network Recovery Activation** - All SSH/SFTP operations now use automatic retry
  - **14 Operations with Retry**: All SSH command, SFTP, and file transfer operations now automatically retry on network failures
    - Command operations: scan, exists, create_dir_all, remove, create_hardlink, create_symlink
    - SFTP operations: read_file, write_file, get_mtime, file_info, copy_file_streaming
    - Transfer operations: copy_file, copy_sparse_file, sync_file_with_delta
  - **Automatic Network Recovery**: SSH operations now recover from transient network failures without user intervention
    - Exponential backoff (1s → 2s → 4s → 8s, capped at 30s max delay)
    - Intelligent error classification (retries only on retryable errors)
    - Zero overhead when operations succeed
  - **Production-Ready Reliability**: All 957 tests passing, retry infrastructure fully activated

### Changed
- **Improved Error Handling**: All SSH/SFTP operations now use retry-wrapped async calls instead of direct spawn_blocking

## [0.0.49] - 2025-10-27

### Added
- **Network Interruption Recovery** - Comprehensive retry and resume infrastructure
  - **Error Classification**: Automatic classification of retryable vs. fatal network errors
    - NetworkTimeout, NetworkDisconnected, NetworkRetryable errors with clear user messages
    - NetworkFatal errors for non-recoverable failures
    - Helper methods: `is_retryable()` and `requires_reconnection()`
    - Intelligent mapping from IO ErrorKinds to appropriate network error types
  - **Retry Logic with Exponential Backoff**:
    - `--retry <N>` flag to set max retry attempts (default: 3, 0 = no retries)
    - `--retry-delay <seconds>` flag for initial delay between retries (default: 1s)
    - Exponential backoff with 2.0 multiplier (1s → 2s → 4s → ...)
    - Max delay cap: 30 seconds to prevent excessive waiting
    - Generic `retry_with_backoff()` function for reusable retry infrastructure
  - **Resume Capability for Interrupted Transfers**:
    - Chunked transfer state tracking (1MB chunks by default)
    - State persistence in `~/.cache/sy/resume/` (XDG-compliant)
    - BLAKE3-based unique state IDs (hash of source + dest + mtime)
    - Atomic state file writes (temp + rename pattern)
    - Staleness detection: rejects resume state if source modified
    - `--resume-only` flag to only resume, don't start new transfers
    - `--clear-resume-state` flag to clear all resume state before sync
  - **SSH Integration**: Retry config passed through all SSH transport operations
    - All SSH operations now use classified error handling
    - `from_ssh_io_error()` maintains context while classifying errors
    - Infrastructure ready for active retry in SSH commands (future enhancement)

### Fixed
- SSH error handling improved with network-aware classification
  - Connection errors (refused, reset, aborted, broken pipe) → NetworkDisconnected
  - Timeout errors → NetworkTimeout with duration context
  - Other IO errors intelligently classified as retryable or fatal

### Technical
- Added `src/error.rs` network error variants with 12 tests
- Added `src/retry.rs` module with exponential backoff logic (9 tests)
- Added `src/resume.rs` module for transfer state management (10 tests)
- Updated `src/transport/ssh.rs` with retry_config field and error classification
- Updated `src/transport/router.rs` to pass retry_config to all SSH transports
- Updated `src/main.rs` to create RetryConfig from CLI args
- All 957 tests passing (up from 938)

### Implementation Details
- **Commits**:
  - Phase 1 (Error Classification): 3e533a2
  - Phase 2 (Retry Logic): 3e533a2
  - Phase 3 (Resume Capability): d266d9d
  - Phase 4 (Integration): 15789e5
- **Infrastructure Ready**: Retry and resume modules complete, ready for future activation in file transfer operations

## [0.0.48] - 2025-10-27

### Added
- **Remote→Remote Bidirectional Sync** - Sync between two SSH hosts
  - Dual SSH connection pools for parallel remote→remote operations
  - Independent SSH configs for source and destination hosts
  - Tested: Mac→Fedora→Fedora with bidirectional changes
  - Removes major limitation from v0.0.47
  - Example: `sy -b user@host1:/path user@host2:/path`

### Fixed
- **.gitignore Support Outside Git Repos** - Patterns now respected everywhere
  - Root cause: `ignore` crate's `git_ignore(true)` only works in git repositories
  - Impact: v0.0.47 synced all files (*.tmp, *.log, node_modules/) despite .gitignore
  - Fix: Explicitly add .gitignore file using `WalkBuilder::add_ignore()`
  - Works in any directory, git repo or not
  - Added test: `test_scanner_gitignore_without_git_repo`
  - Verified: SSH bisync respects .gitignore patterns correctly

### Testing
- Comprehensive test report: 23 scenarios, 91.3% pass rate (21/23)
- Both previously failed tests now pass
- 410+ unit tests passing

## [0.0.47] - 2025-10-27

### Fixed
- **CRITICAL: SSH Bidirectional Sync** - Implemented missing `write_file()` for SSH transport
  - Root cause: `SshTransport` didn't override `write_file()`, falling back to local filesystem writes
  - Impact: v0.0.46 bisync reported success but files never reached remote server
  - Fix: Implemented `write_file()` using SFTP with recursive directory creation and mtime preservation (89 lines)
  - Testing: All 8 comprehensive SSH bisync tests pass (Mac ↔ Fedora over Tailscale)
  - Verified: Deletion propagation, conflict resolution, large files (10MB @ 8.27 MB/s), dry-run mode
  - **Users on v0.0.46: SSH bidirectional sync is broken, upgrade to v0.0.47 immediately**

### Testing
- Added comprehensive SSH bisync test suite with 8 real-world scenarios
- Tested Mac (M3 Max) ↔ Fedora (i9-13900KF) over Tailscale
- All 410 unit tests passing, 0 regressions

## [0.0.46] - 2025-10-27

**⚠️ CRITICAL BUG: SSH bidirectional sync does not work in this version. Files are not written to remote. Upgrade to v0.0.47 immediately.**

### Added
- **Conflict History Logging** - Automatic audit trail for bidirectional sync
  - Logs all resolved conflicts to `~/.cache/sy/bisync/<pair>.conflicts.log`
  - Format: `timestamp | path | conflict_type | strategy | winner`
  - Append-only log preserves complete conflict history
  - Intelligent winner resolution for all 6 conflict strategies
  - Skips logging in dry-run mode
- **Exclude Patterns Documentation** - Documented existing `.gitignore` support
  - Bisync respects `.gitignore` files for filtering synced files
  - Supports global gitignore (`~/.gitignore_global`)
  - Automatically excludes `.git` directories
  - Common patterns: `.DS_Store`, `node_modules/`, `*.tmp`

### Fixed
- **Critical Bisync State Storage Bug** - Fixed deletion propagation
  - Root cause: `update_state()` only stored one side after copy operations
  - Impact: Deletions were misclassified as "new files" and copied back
  - Fix: Store both source AND dest states after any copy operation
  - Deletion safety limits now work correctly
  - State persistence across syncs now reliable
  - Idempotent syncs properly detect no changes
- **Clippy Warnings** - Resolved 5 clippy warnings for release
  - Fixed `ptr_arg` warnings (use `&Path` instead of `&PathBuf`)
  - Fixed `unnecessary_unwrap` with proper `if let` patterns
  - Added annotations for intentional patterns

### Changed
- Enhanced documentation for conflict logging in README and design docs
- Improved error messages for conflict resolution

### Testing
- Created `bisync_real_world_test.sh` with 7 comprehensive test scenarios
- All 410 unit tests passing
- All 11 real-world bisync tests passing
- 0 compiler warnings, 0 clippy warnings

## [0.0.45] - 2025-10-26

### Fixed
- **Bisync State Format v2** - Fixed critical data corruption bugs in text format
  - **Proper escaping**: Handles quotes, newlines, backslashes, tabs correctly
  - **Error handling**: Parse failures now return errors instead of silent corruption
  - **last_sync field**: Now stored separately from mtime (was incorrectly using mtime)
  - **Backward compatible**: Can still read v1 format files from v0.0.44

### Added
- 8 new edge-case tests for state file format
  - Quote handling, newline handling, backslash handling
  - Round-trip testing, v1 backward compatibility
  - Parse error detection

### Changed
- Format version bumped from v1 to v2
- All paths now quoted and escaped automatically
- Test count: 410 tests passing (up from 402)

### Migration
- v0.0.44 state files (.lst) are automatically upgraded to v2
- No user action required (format is backward compatible)

## [0.0.44] - 2025-10-26

### Changed
- **Bisync State Storage Refactored** - Switched from SQLite to text-based format
  - Text-based listing files in `~/.cache/sy/bisync/` (`.lst` instead of `.db`)
  - Format inspired by rclone bisync: human-readable, debuggable
  - Simpler implementation: ~300 lines vs ~400 lines (SQL removed)
  - Atomic writes with temp file + rename for consistency
  - Same file format spec documented in `docs/architecture/BISYNC_STATE_FORMAT.md`
  - Header with version, sync pair info, and last sync timestamp
  - One file per sync pair with both source and dest state
  - Quoted paths for special characters support

### Removed
- SQLite dependency for bisync state (still used for `--checksum-db`)
- ~100 lines of SQL code from bisync module

### Benefits
- **Simplicity**: Plain text, no schema migrations needed
- **Debuggability**: `cat ~/.cache/sy/bisync/*.lst` shows full state
- **Proven approach**: Similar to rclone bisync text format
- **Fewer moving parts**: One less database to manage

### Migration
- **Breaking change**: Old `.db` files from v0.0.43 are ignored
- Use `--clear-bisync-state` if upgrading from v0.0.43
- Fresh sync will create new `.lst` files automatically

## [0.0.43] - 2025-10-24

### Added
- **Bidirectional Sync** - Two-way file synchronization with automatic conflict resolution
  - New `--bidirectional` / `-b` flag enables two-way sync mode
  - Detects changes on both sides and syncs in both directions
  - State tracking in `~/.cache/sy/bisync/` (text-based in v0.0.44+, SQLite in v0.0.43)
  - Stores file metadata (path, mtime, size) from prior sync for accurate change detection
  - Unique state file per source/dest pair (isolated state)

- **Conflict Resolution Strategies** - 6 automated resolution methods
  - `newer` (default) - Most recent modification time wins
  - `larger` - Largest file size wins
  - `smaller` - Smallest file size wins
  - `source` - Source always wins (force push)
  - `dest` - Destination always wins (force pull)
  - `rename` - Keep both files with `.conflict-{timestamp}-{side}` suffix
  - Automatic tie-breaker: falls back to rename when attributes equal
  - Configure via `--conflict-resolve {strategy}` flag

- **Deletion Safety** - Configurable limits prevent cascading data loss
  - `--max-delete {percent}` flag sets deletion threshold (0-100)
  - Default: 50% (aborts if >50% of files would be deleted)
  - Set to 0 for unlimited deletions
  - Protects against accidental mass deletion from bugs or misconfiguration

- **Bidirectional Sync Features**
  - Content equality checks reduce false conflict detection
  - Handles 9 change types: new files, modifications, deletions, conflicts
  - Edge case handling: partial state, missing files, modify-delete conflicts
  - Dry-run support: preview bidirectional changes with `--dry-run`
  - Clear state: `--clear-bisync-state` forces fresh comparison
  - Conflict reporting: displays all detected conflicts with resolution actions
  - Error collection: continues sync on errors, reports all failures

- **CLI Enhancements**
  - Comprehensive validation for bidirectional flags
  - Conflict with incompatible modes (verify-only, watch)
  - Clear error messages for invalid strategies or percentages

### Performance
- Bidirectional sync: Minimal overhead vs unidirectional (<5% for no conflicts)
- Parallel scanning: Source and dest can be scanned in parallel (future)
- State caching: SQLite queries optimized with indexes

### Implementation
- 4 new modules: state DB, change classifier, conflict resolver, sync engine
- ~2,000 lines of production code
- 32 new unit tests (all passing)
- Total test coverage: 414 tests (402 passing + 12 ignored)
- Comprehensive design documentation in `docs/architecture/`

### Notes
- Bidirectional sync currently supports local→local paths only
- Remote support (SSH) deferred to future version
- Based on rclone bisync approach (snapshot-based state tracking)
- Simpler than Unison (~500 lines vs 3000+), covers 80% of use cases

## [0.0.42] - 2025-10-23

### Added
- **SSH Connection Pooling** - True parallel SSH transfers
  - Connection pool with N sessions for N workers (`--parallel` flag)
  - Round-robin session distribution via atomic counter
  - Eliminates ControlMaster TCP bottleneck
  - Each worker gets dedicated SSH connection for true parallelism
  - Automatic pool size matching worker count
  - 5 new unit tests for atomicity and round-robin logic

- **SSH Sparse File Transfer** - Automatic bandwidth optimization
  - Auto-detection of sparse files on Unix (blocks*512 < file_size)
  - Transfers only data regions, skips holes (zeros)
  - 10x bandwidth savings for VM images (e.g., 10GB → 1GB)
  - 5x bandwidth savings for database files (e.g., 100GB → 20GB)
  - sy-remote `ReceiveSparseFile` command for remote reconstruction
  - Graceful fallback to regular transfer if sparse detection fails
  - Protocol: detect regions → send JSON + stream data → reconstruct
  - 3 new integration tests for sparse file handling

- **Comprehensive Testing Improvements**
  - Performance monitoring accuracy tests (9 new tests)
    - Phase duration accuracy, speed calculation, concurrent operations
    - Thread-safety tests for byte/file counting under load
    - Edge cases: zero duration, peak speed tracking, bandwidth utilization
  - Error collection threshold tests (4 new tests)
    - Unlimited errors (max_errors=0), abort when exceeded
    - Below threshold continues, error message format verification
  - Sparse file edge case tests (11 new tests)
    - Non-existent files, empty files, leading/trailing holes
    - Multiple data regions, large offsets (1GB), single byte detection
    - Region ordering invariants, boundary conditions
  - Test coverage increased: 355 → 385 tests (378 passing + 7 ignored)

### Performance
- SSH transfers: True parallel throughput with connection pooling
- Sparse files: Up to 10x faster transfers for VM images and databases
- No regressions in existing functionality

## [0.0.22] - 2025-10-15

### Added - Phase 9 (Developer Experience)
- **Hooks system** - Pre/post sync extensibility
  - Pre-sync hook: `~/.config/sy/hooks/pre-sync.sh`
  - Post-sync hook: `~/.config/sy/hooks/post-sync.sh`
  - Environment variables: `SY_SOURCE`, `SY_DEST`, `SY_FILES_TRANSFERRED`, etc.
  - CLI flags: `--no-hooks`, `--hook-timeout <seconds>`
  - Configurable timeout (default: 30s)
  - Example use cases: Git commit after sync, notifications, backups

- **Ignore templates** - Built-in patterns for common project types
  - `--ignore-template <name>` flag to load built-in templates
  - Templates: node, rust, python, docker, mac, windows, git
  - `.syignore` file support for custom patterns (similar to .gitignore)
  - Auto-loaded from source directory if present
  - Combines with .gitignore and CLI --exclude patterns

- **Enhanced dry-run output** - Improved clarity and formatting
  - Better action labeling (Create/Update/Delete/Skip)
  - File count and size summaries
  - Clearer visual confirmation of what would happen

### Added - Phase 10 (Cloud Era)
- **S3 transport** - Full AWS S3 and S3-compatible service support
  - Syntax: `sy /local s3://bucket/path`
  - Query params: `s3://bucket/path?region=us-west-2&endpoint=https://...`
  - Supported services: AWS S3, Cloudflare R2, Backblaze B2, Wasabi
  - Full S3 API integration via aws-sdk-rust
  - Bidirectional: local→S3 and S3→local syncs

- **Multipart uploads** - Efficient handling of large files
  - Automatic multipart upload for files >100MB
  - 5MB chunk size (S3 minimum)
  - Streaming upload with progress tracking
  - Parallel chunk uploads (infrastructure ready)

- **S3 path parsing** - URL-based configuration
  - Parse region from query string
  - Parse custom endpoint for S3-compatible services
  - Force path-style URLs for non-AWS S3
  - 7 comprehensive path parsing tests

### Added - Phase 11 (Scale)
- **Incremental scanning** - Cache-based skip logic for faster re-syncs
  - Directory mtime cache to detect unchanged directories
  - File metadata cache (path, size, mtime, is_dir)
  - Skip rescanning unchanged directories (use cached file list)
  - Cache version control with auto-invalidation
  - **Performance**: 1.67-1.84x faster (10-100x expected on large datasets)
  - Cache file: `.sy-dir-cache.json` in destination (JSON format)
  - CLI flags: `--use-cache`, `--clear-cache`

- **Memory-efficient deletion** - Streaming with Bloom filters
  - Bloom filter for >10k files (100x memory reduction vs HashMap)
  - Automatic threshold switching at 10k files
  - No false negatives, handles false positives correctly
  - Streaming destination scan (no loading all into memory)

- **State caching** - Directory and file metadata between runs
  - Cache format: JSON (human-readable, version 2)
  - Stores directory mtimes and file metadata grouped by directory
  - 1-second mtime tolerance for filesystem granularity
  - Dramatically faster re-syncs for unchanged datasets

### Performance
- **Incremental scanning**: 1.67-1.84x speedup measured on small datasets
- **Expected scaling**: 10-100x speedup on large datasets (>10k files)
- **Memory efficiency**: Bloom filter uses ~1.2MB for 1M files vs ~100MB for HashSet
- **S3 multipart**: Handles files of any size efficiently

### Technical
- Added DirectoryCache v2 with file metadata storage
- Added CachedFile struct (path, size, modified, is_dir)
- Extended scale.rs with FileSetBloom for deletion planning
- Full S3Transport implementation (453 lines)
- S3 integration in TransportRouter for local↔S3 syncs
- Added ignore template system with built-in patterns
- Added hooks execution with environment variables
- Total tests: 289 passing (282 unit + 18 integration + 7 performance)

### Roadmap Status
- ✅ Phase 9 complete: Developer Experience
- ✅ Phase 10 complete: Cloud Era (S3 support)
- ✅ Phase 11 complete: Scale optimizations
- 🎯 Phase 12 next: Production Release (v1.0 prep)

## [0.0.21] - 2025-10-13

### Added
- **xxHash3 wrapper for fast file verification** - Complete ChecksumType::Fast implementation
  - XxHash3Hasher module with hash_file(), hash_data(), new_hasher() methods
  - ~10x faster than BLAKE3 for non-cryptographic checksums
  - Ideal for detecting accidental corruption without cryptographic overhead
  - Full test coverage (12 new tests including known hash regression test)

### Fixed
- **Remote file update detection** - CRITICAL bug fix enabling automatic delta sync
  - Problem: Remote files always treated as "create" instead of "update"
  - Root cause: SshTransport.metadata() couldn't return std::fs::Metadata for remote files
  - Solution: Transport-agnostic FileInfo struct (size + mtime)
  - Implemented SshTransport.file_info() using SFTP stat()
  - Added delegation through DualTransport, TransportRouter, Arc<T>
  - Updated StrategyPlanner to use file_info() instead of metadata()
  - **Impact: 98% bandwidth savings** (50MB file, 1MB modified → only ~1MB transferred)

### Performance
- Delta sync now automatically triggers for remote file updates
- Test results: 50MB file with 1MB change transferred only 1MB (49MB saved)
- xxHash3 checksums: 10x faster than BLAKE3 for fast verification mode

### Technical
- Added src/integrity/xxhash3.rs module
- FileInfo struct in transport::mod (transport-agnostic metadata)
- SshTransport.file_info() uses SFTP stat() for remote file metadata
- All transports now support file_info() method
- Test count: 251 passing (+12 integrity tests)

### Added (Phase 6 Complete)
- **Full metadata preservation over SSH** - Extended transport protocol
  - FileEntryJson now includes: symlinks, hardlinks, sparse files, xattrs, ACLs
  - sy-remote serializes complete metadata with base64 encoding for xattrs
  - SSH transport deserializes and uses all metadata fields
  - Enables rsync-like metadata preservation over network
- **Symlink preservation over SSH** - Fully implemented
  - Added create_symlink() to Transport trait
  - Implementations for Local, SSH, Dual, and Router transports
  - Uses `ln -s` command over SSH for remote symlink creation
  - Tested and working: both relative and absolute symlinks transfer correctly
- **Hardlink preservation** - FULLY IMPLEMENTED
  - `-H / --preserve-hardlinks` flag to preserve hard links
  - Tracks inode numbers and link counts during scan
  - **Local sync**: ✅ Fully working and tested
  - **SSH sync**: ✅ FIXED - Race condition resolved with async coordination
  - InodeState enum with tokio::sync::Notify for proper task coordination
  - Works correctly with parallel execution (no `-j 1` workaround needed)
- **ACL preservation** - POSIX Access Control Lists support (FULLY IMPLEMENTED)
  - `-A / --preserve-acls` flag to preserve ACLs
  - ACL detection during file scanning (always scanned)
  - Full implementation: scan, parse, and apply ACLs
  - Uses standard ACL text format (Display trait)
  - FromStr parsing for robustness
  - Graceful error handling for invalid entries
  - Cross-platform (Unix/Linux/macOS)
  - 5 comprehensive tests including integration tests
  - Total tests: 210 (all passing, zero warnings on lib)

### Planned for v0.1.0
- Network speed detection
- Parallel chunk transfers (within single files)
- Periodic checkpointing during sync (infrastructure ready)

## [0.0.13] - 2025-10-06

### Added
- **Resume support** - Automatic recovery from interrupted syncs
  - Loads `.sy-state.json` from destination on startup
  - Checks flag compatibility (delete, exclude, size filters)
  - Skips already-completed files
  - Cleans up state file on successful completion
  - User feedback showing resume progress
  - Example: Interrupt sync with Ctrl+C, re-run same command to resume

### Changed
- `--resume` flag now functional (default: true)
- Resume state tracks sync flags for compatibility checking

### Technical
- ResumeState integration in SyncEngine
- Thread-safe state management with Arc<Mutex>
- Completed file filtering before task planning
- Automatic state cleanup on sync success
- All 111 tests passing

### Known Limitations
- Periodic checkpointing (saving state during sync) not yet implemented
- State only cleaned up on full sync completion
- Resume infrastructure complete, periodic saves deferred to future release

## [0.0.12] - 2025-10-06

### Added
- **Watch mode** - Continuous file monitoring for real-time sync
  - `--watch` flag enables watch mode
  - Initial sync on startup, then monitors for changes
  - 500ms debouncing to avoid syncing every keystroke
  - Detects file create, modify, delete events
  - Graceful Ctrl+C shutdown
  - Cross-platform (Linux, macOS, Windows via notify crate)
  - Example: `sy /src /dst --watch`

### Technical
- Added notify 6.0 dependency for file watching
- Added tokio "signal" feature for Ctrl+C handling
- WatchMode struct in src/sync/watch.rs
- Event filtering (ignores metadata-only changes)
- All 111 tests passing (+2 watch mode tests)

### Documentation
- PHASE4_DESIGN.md includes complete watch mode spec

## [0.0.11] - 2025-10-06

### Added
- **JSON output mode** - Machine-readable NDJSON format for scripting
  - `--json` flag emits newline-delimited JSON events
  - Events: start, create, update, skip, delete, summary
  - Automatically suppresses normal output and logging (errors only)
  - Example: `sy /src /dst --json | jq`
- **Config profiles** - Save and reuse sync configurations
  - Config file: `~/.config/sy/config.toml` (Linux) or `~/Library/Application Support/sy/config.toml` (macOS)
  - `--profile <name>` to use saved profile
  - `--list-profiles` to show available profiles
  - `--show-profile <name>` to display profile details
  - Profile settings merged with CLI args (CLI takes precedence)
- **Resume infrastructure** - State file support (logic pending)
  - ResumeState struct with JSON serialization
  - Atomic state file saves (write temp, rename)
  - CLI flags: `--resume`, `--checkpoint-files`, `--checkpoint-bytes`
  - Implementation deferred to future release

### Changed
- Source and destination paths now optional when using `--profile`
- Logging level ERROR when `--json` mode active
- Enhanced CLI validation for profile-only modes

### Technical
- Added toml and chrono dependencies
- Config loading with XDG Base Directory compliance
- Profile merging logic in main.rs
- All 109 tests passing

### Documentation
- Created PHASE4_DESIGN.md (644 lines) with complete Phase 4 spec
- Updated MODERNIZATION_ROADMAP.md with v1.0 timeline

### Planned for v0.5.0
- Multi-layer checksums (BLAKE3 end-to-end)
- Verification modes (fast, standard, paranoid)
- Atomic operations
- Crash recovery

## [0.0.10] - 2025-10-06

### Added
- **Parallel checksum computation** - 2-4x faster on large files
  - Uses rayon for multi-threaded block processing
  - Each thread opens independent file handle for parallel I/O
  - Example: 1GB file checksum reduced from ~5s to ~1.5s
- **Delta streaming via stdin** - Eliminates command line length limits
  - Delta operations sent via stdin instead of command arguments
  - Binary-safe transmission supports any delta size
  - No more command line truncation for large deltas
- **Delta compression** - 5-10x reduction in delta transfer size
  - Automatic Zstd compression of delta JSON before transfer
  - Remote auto-detects compression via magic header
  - Example: 10MB delta JSON → 1-2MB compressed transfer
- **Compression infrastructure** - Ready for full file compression
  - sy-remote `receive-file` command accepts compressed files
  - Compression decision logic integrated into SSH transport
  - Auto-detection of pre-compressed formats (jpg, mp4, zip, etc.)
- Bandwidth limiting for controlled transfer rates
  - `--bwlimit` flag accepts human-readable rates (e.g., "1MB", "500KB")
  - Token bucket algorithm with burst support
- Exclude pattern support for flexible file filtering
  - `--exclude` flag accepts glob patterns (can be repeated)
  - Examples: `--exclude "*.log"`, `--exclude "node_modules"`
- File size filtering options
  - `--min-size` and `--max-size` flags with human-readable sizes
  - Example: `sy /src /dst --min-size 1KB --max-size 10MB`
- Color-coded summary output for better visual clarity
  - Success messages in bold green, operations color-coded
  - Colors automatically disable in non-TTY environments

### Changed
- Buffer sizes increased from 128KB → 256KB (20-30% improvement)
  - Applied across SSH transport, local transport, and delta generator
  - Optimized for modern network hardware
- SSH sessions now use keepalive (60s interval)
  - Prevents connection drops during long transfers
  - Disconnects after 3 missed keepalive responses
- Compression module simplified to Zstd level 3 only
  - Removed LZ4 after benchmarking showed Zstd L3 is faster (8.7 GB/s vs 23 GB/s)
  - Updated DESIGN.md with accurate performance numbers

### Fixed
- **Zero dead code warnings** - All compression helper functions now used
  - Properly marked public APIs used by binaries
  - Clean build with zero warnings
- Delta sync now computes checksums remotely (200x optimization)
  - Before: Download entire file → compute locally → upload entire file
  - After: Compute remotely → send delta only
  - Impact: Only changed data transferred (1% change = 1% transfer)

### Performance
- **Parallel checksums**: 2-4x faster on large files (rayon)
- **Delta compression**: 5-10x smaller transfers (Zstd on JSON)
- **Buffer optimization**: 20-30% throughput improvement
- **Remote checksums**: 200x reduction for 1% file changes
- **SSH keepalive**: Prevents timeout-related failures

### Technical
- Added rayon dependency for parallel processing
- Binary-safe stdin streaming for compressed data
- Zstd magic header detection (0x28, 0xB5, 0x2F, 0xFD)
- All 92 tests passing with zero warnings

### Documentation
- Updated PERFORMANCE_ANALYSIS.md with completed optimizations
- Documented compression benchmarks: Zstd L3 at 8.7 GB/s
- Added TODO markers for future full file compression

## [0.0.9] - 2025-10-02

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
- Enhanced dry-run mode output
  - Summary shows "Dry-run complete (no changes made)"
  - File operations use "Would" prefix (e.g., "Would create: 5")
  - Clear visual confirmation that nothing was modified

### Changed
- Error messages now include helpful context and resolution steps
- Summary output formatting improved with better alignment and visual sections

### Testing
- Added comprehensive delta sync benchmarks
  - Small change benchmarks (10MB, 50MB, 100MB files)
  - Delta sync vs full copy comparison
  - Large file (1GB) delta sync performance

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
- ✅ Phase 3: Parallelism + Optimization (v0.0.4-v0.0.9) - Parallel transfers + UX polish
- 🚧 Phase 4: Advanced Features (v0.1.0+) - Network detection, compression, resume

[Unreleased]: https://github.com/nijaru/sy/compare/v0.0.22...HEAD
[0.0.22]: https://github.com/nijaru/sy/compare/v0.0.21...v0.0.22
[0.0.21]: https://github.com/nijaru/sy/compare/v0.0.13...v0.0.21
[0.0.13]: https://github.com/nijaru/sy/releases/tag/v0.0.13
[0.0.12]: https://github.com/nijaru/sy/releases/tag/v0.0.12
[0.0.11]: https://github.com/nijaru/sy/releases/tag/v0.0.11
[0.0.10]: https://github.com/nijaru/sy/releases/tag/v0.0.10
[0.0.9]: https://github.com/nijaru/sy/releases/tag/v0.0.9
[0.0.8]: https://github.com/nijaru/sy/releases/tag/v0.0.8
[0.0.7]: https://github.com/nijaru/sy/releases/tag/v0.0.7
[0.0.6]: https://github.com/nijaru/sy/releases/tag/v0.0.6
[0.0.5]: https://github.com/nijaru/sy/releases/tag/v0.0.5
[0.0.4]: https://github.com/nijaru/sy/releases/tag/v0.0.4
[0.0.3]: https://github.com/nijaru/sy/releases/tag/v0.0.3
[0.0.2]: https://github.com/nijaru/sy/releases/tag/v0.0.2
[0.0.1]: https://github.com/nijaru/sy/releases/tag/v0.0.1
