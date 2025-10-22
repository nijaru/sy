# TODO

## High Priority
- [ ] Phase 5: Verification enhancements
  - [x] Design (see ai/research/phase5_verification_design.md)
  - [x] Phase 5a: Pre-transfer checksums (v0.0.35) - Core implementation done
    - [x] Add checksum fields to SyncTask
    - [x] Implement checksum computation in planner
    - [x] Add tests (3 new tests, all 317 passing)
    - [ ] Update documentation
    - [ ] End-to-end CLI testing
    - [ ] Remote checksum support (deferred to follow-up)
  - [ ] Phase 5b: Checksum database (v0.0.36)
  - [ ] Phase 5c: --verify-only mode (v0.0.37)

## In Progress
- Phase 5a implementation - core done, needs docs and e2e testing

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
