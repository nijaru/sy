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
- [ ] macOS BSD File Flags (v0.0.41) - 60% complete
  - [x] Research macOS-specific features (comprehensive analysis complete)
  - [x] Add bsd_flags field to FileEntry struct
  - [x] Implement BSD flags capture in scanner (using st_flags())
  - [x] Add --preserve-flags (-F) CLI flag
  - [x] Add preserve_flags to Transferrer struct
  - [ ] Wire preserve_flags through SyncEngine and all Transferrer::new() calls
  - [ ] Implement set_bsd_flags() function in LocalTransport
  - [ ] Handle immutable flags (UF_IMMUTABLE/SF_IMMUTABLE with temp clear)
  - [ ] Add tests for BSD flags preservation
  - [ ] Update documentation (README, MACOS_SUPPORT.md)

## Recently Completed
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
- [ ] Sparse file optimization improvements
- [ ] macOS-specific features (Finder tags, resource forks)
- [ ] Windows-specific features (file attributes, ACLs)
- [ ] Multi-destination sync
- [ ] Bidirectional sync
- [ ] Cloud storage backends
- [ ] Plugin system

## Technical Debt
- [ ] Remove --mode flag placeholder (not yet implemented)
- ~~[ ] Implement actual bandwidth limiting (currently placeholder)~~ - **DONE!** Already fully implemented
- ~~[ ] Add directory creation tracking to perf monitor~~ - **DONE!** Already tracked
- ~~[ ] Add peak speed tracking to perf monitor~~ - **DONE!** Already tracked via update_peak_speed()

## Research Needed
- [ ] Modern SSH multiplexing best practices (2025)
- [ ] Latest filesystem feature detection methods
- [ ] State-of-the-art compression algorithms for file sync

## Documentation
- [x] Add --perf flag examples to README
- [x] Document error reporting in user guide
- [x] Update performance comparison charts
- [x] Create troubleshooting guide

## Testing
- [ ] Add tests for sparse file edge cases
- [ ] Add tests for error collection with max_errors threshold
- [ ] Add performance monitoring accuracy tests
- [ ] Add COW strategy selection tests for various filesystems
