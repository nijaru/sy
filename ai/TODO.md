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
  - [ ] Phase 5c: --verify-only mode (v0.0.36) - NEXT

## In Progress
- Phase 5b complete! Ready for Phase 5c or other tasks

## Backlog (from docs/MODERNIZATION_ROADMAP.md)
- [ ] Compression auto-detection (file type awareness)
- [ ] Enhanced progress display (current file, real-time speed, ETA)
- [ ] Bandwidth utilization metrics
- [ ] Symbolic link chain detection
- [ ] Sparse file optimization improvements
- [ ] macOS-specific features (Finder tags, resource forks)
- [ ] Windows-specific features (file attributes, ACLs)
- [ ] Multi-destination sync
- [ ] Bidirectional sync
- [ ] Cloud storage backends
- [ ] Plugin system

## Technical Debt
- [ ] Remove --mode flag placeholder (not yet implemented)
- [ ] Implement actual bandwidth limiting (currently placeholder)
- [ ] Add directory creation tracking to perf monitor (method exists but unused)
- [ ] Add peak speed tracking to perf monitor (method exists but unused)

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
