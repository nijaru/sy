# TODO

## High Priority
- [ ] Phase 5: Verification enhancements
  - [x] Design (see ai/research/phase5_verification_design.md)
  - [x] Phase 5a: Pre-transfer checksums (v0.0.35) - COMPLETE ✅
    - [x] Add checksum fields to SyncTask
    - [x] Implement checksum computation in planner
    - [x] Add tests (3 new tests, all 317 passing)
    - [x] Update documentation (README + TROUBLESHOOTING)
    - [x] End-to-end CLI testing (verified working)
    - [ ] Remote checksum support (deferred to follow-up)
  - [x] Phase 5b: Checksum database (v0.0.35) - COMPLETE ✅
    - [x] Add rusqlite dependency
    - [x] Implement ChecksumDatabase module with SQLite backend
    - [x] Add CLI flags (--checksum-db, --clear-checksum-db, --prune-checksum-db)
    - [x] Integrate with SyncEngine and StrategyPlanner
    - [x] Store checksums after successful transfers
    - [x] Handle prune flag for stale entries
    - [x] Add tests (8 new tests, all 325 passing)
    - [x] End-to-end CLI testing (verified 10-100x speedup)
    - [x] Update documentation (comprehensive README coverage)
  - [x] Phase 5c: --verify-only mode (v0.0.36) - COMPLETE ✅
    - [x] Add --verify-only CLI flag with validation
    - [x] Create VerificationResult struct
    - [x] Implement verify() async method in SyncEngine
    - [x] Add compare_checksums() helper method
    - [x] Integrate with main.rs (human-readable output)
    - [x] Implement exit codes (0=match, 1=mismatch, 2=error)
    - [x] Add JSON output support (VerificationResult event)
    - [x] Add test for JSON serialization (1 new test, 326 passing)
    - [x] End-to-end CLI testing (all scenarios verified)
    - [x] Update documentation (comprehensive README coverage)

## In Progress
None currently - ready for next feature!

## Recently Completed
- [x] SSH Bidirectional Sync (v0.0.46) - COMPLETE ✅
  - [x] Refactor BisyncEngine to use Transport abstraction
  - [x] Make sync() async for remote operations
  - [x] Support local↔local, local↔remote, and remote↔remote
  - [x] Update CLI with transport creation logic
  - [x] Performance profiling (no bottlenecks found)
  - [x] All 410 tests passing, 0 warnings
- [x] macOS BSD File Flags (v0.0.41) - COMPLETE ✅
  - [x] Research macOS-specific features (comprehensive analysis complete)
  - [x] Add bsd_flags field to FileEntry struct
  - [x] Implement BSD flags capture in scanner (using st_flags())
  - [x] Add --preserve-flags (-F) CLI flag
  - [x] Add preserve_flags to Transferrer struct
  - [x] Wire preserve_flags through SyncEngine
  - [x] Implement write_bsd_flags() method using chflags()
  - [x] Add tests for BSD flags preservation (2 tests added)
  - [x] Fix test Transferrer::new() and SyncEngine::new() calls
  - [x] Fix test FileEntry initializations (35+ locations)
  - [x] Fix flag preservation behavior (explicitly clear when not preserving)
  - [x] Update documentation (README, MACOS_SUPPORT.md)
  - [x] Fix cross-platform compilation (remove all #[cfg] from preserve_flags usage sites)
  - [ ] Optional: Handle immutable flags (deferred to future version if needed)
- Symlink loop detection (v0.0.40 - follow_links option, walkdir integration, comprehensive tests)
- Bandwidth utilization metrics (v0.0.39 - JSON output complete)
- Enhanced progress display (v0.0.38 - byte-based, speed, current file)
- Compression auto-detection feature (v0.0.37 - content sampling, CLI flags, SSH integration)
- Phase 5 (Verification Enhancements) complete! All sub-phases done: 5a, 5b, 5c

## Backlog (from docs/MODERNIZATION_ROADMAP.md)
- [x] Compression auto-detection (file type awareness) - COMPLETE ✅ (v0.0.37)
- [x] Enhanced progress display (current file, real-time speed, ETA) - COMPLETE ✅ (v0.0.38)
- [x] Bandwidth utilization metrics (% of limit when using --bwlimit) - COMPLETE ✅ (v0.0.39)
- [x] Symbolic link chain detection - COMPLETE ✅ (v0.0.40)
- [x] macOS-specific features (Finder tags, resource forks) - COMPLETE ✅ (v0.0.16 xattr support, v0.0.41 BSD flags)
  - Finder tags preserved via `com.apple.metadata:_kMDItemUserTags` xattr
  - Resource forks preserved via `com.apple.ResourceFork` xattr
  - BSD file flags preserved with `-F` flag (hidden, immutable, nodump, etc.)
- [x] SSH connection pooling - COMPLETE ✅ (v0.0.42)
- [x] SSH sparse file transfer - COMPLETE ✅ (v0.0.42)
- [x] Bidirectional sync - COMPLETE ✅ (v0.0.43-v0.0.46)
  - Text-based state tracking (v0.0.44)
  - SSH support for remote servers (v0.0.46)
- [ ] Sparse file optimization improvements (foundation complete, SSH integration done)
- [ ] Windows-specific features (file attributes, ACLs)
- [ ] Multi-destination sync (deferred - shell loops work fine)
- [ ] Cloud storage backends (AWS S3 basic support done v0.0.22, expansion TBD)
- [ ] Plugin system

## Technical Debt
- ~~[ ] Remove --mode flag placeholder (not yet implemented)~~ - **DONE!** Already fully implemented (VerificationMode enum with fast/standard/verify/paranoid)
- ~~[ ] Implement actual bandwidth limiting (currently placeholder)~~ - **DONE!** Already fully implemented
- ~~[ ] Add directory creation tracking to perf monitor~~ - **DONE!** Already tracked
- ~~[ ] Add peak speed tracking to perf monitor~~ - **DONE!** Already tracked via update_peak_speed()

## Research Needed
- [x] Modern SSH multiplexing best practices (2025) - COMPLETE ✅
  - ControlMaster NOT recommended for parallel file transfers (bottlenecks on one TCP connection)
  - Better: SSH connection pooling (N connections = N workers) for true parallel throughput
  - See ai/research/ssh_multiplexing_2025.md
- [ ] Latest filesystem feature detection methods
- [ ] State-of-the-art compression algorithms for file sync

## Documentation
- [x] Add --perf flag examples to README
- [x] Document error reporting in user guide
- [x] Update performance comparison charts
- [x] Create troubleshooting guide

## Testing
- [x] Add performance monitoring accuracy tests - COMPLETE ✅ (2025-10-23)
  - Added 9 comprehensive accuracy tests in perf.rs (total: 15 tests)
  - Phase duration accuracy, speed calculation, concurrent operations
  - Thread-safety tests (byte/file counting under concurrent load)
  - Edge cases (zero duration, peak speed tracking, bandwidth utilization)
- [x] Add tests for error collection with max_errors threshold - COMPLETE ✅ (2025-10-23)
  - Added 4 threshold behavior tests in sync/mod.rs
  - Tests for: unlimited errors (max=0), abort when exceeded, below threshold continues
  - Verified error message format with count and first error
- [x] Add tests for sparse file edge cases - COMPLETE ✅ (2025-10-23)
  - Added 11 edge case tests in sparse.rs (total: 14 tests)
  - Non-existent file, empty file, leading/trailing holes, multiple regions
  - Large offsets (1GB), single byte, region ordering, boundary conditions
  - Platform-specific: 5 pass everywhere, 7 ignored on macOS APFS
- [x] Add COW strategy selection tests for various filesystems - COMPLETE ✅
  - Added 11 edge case tests in fs_util.rs
  - Non-existent paths, parent/child relationships, symlinks, 3-way hard links
  - All 377 tests passing (370 + 7 ignored APFS sparse tests)
