# TODO

## High Priority
- [ ] Phase 5: Verification enhancements
  - [ ] Pre-transfer checksums
  - [ ] Checksum database for future verification
  - [ ] --verify-only mode

## In Progress
- Documentation reorganization (ai/ structure)

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
- [ ] Add --perf flag examples to README
- [ ] Document error reporting in user guide
- [ ] Update performance comparison charts
- [ ] Create troubleshooting guide

## Testing
- [ ] Add tests for sparse file edge cases
- [ ] Add tests for error collection with max_errors threshold
- [ ] Add performance monitoring accuracy tests
- [ ] Add COW strategy selection tests for various filesystems
