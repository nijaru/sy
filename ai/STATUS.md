# Status

_Last Updated: 2025-10-21_

## Current State
- Version: v0.0.35 (in development)
- Phase: Phase 5a (Pre-Transfer Checksums) - Implementation in progress
- Test Coverage: 317 tests passing
- Build: Passing (all tests green)
- Performance: 1.3x - 8.8x faster than rsync (see docs/PERFORMANCE.md)

## Implemented Features
- ✅ Local and remote (SSH) sync
- ✅ Delta sync with COW optimization (v0.0.23)
- ✅ Filesystem-aware strategy selection
- ✅ Hard link preservation
- ✅ Parallel file transfers
- ✅ Compression (zstd)
- ✅ Progress display with colors
- ✅ Gitignore awareness
- ✅ JSON output
- ✅ Config profiles
- ✅ Watch mode
- ✅ Resume support
- ✅ Performance monitoring (--perf flag, v0.0.33)
- ✅ Comprehensive error reporting (v0.0.34)
- ✅ Pre-transfer checksums (--checksum flag, v0.0.35) - local→local, saves bandwidth!

## What Worked
- **Local delta sync optimization** (v0.0.23): Using simple block comparison instead of rolling hash for local→local sync achieved 5-9x speedup
- **COW-aware strategies**: Automatic filesystem detection and strategy selection prevents data corruption
- **Performance monitoring**: Arc<Mutex<PerformanceMonitor>> with atomic counters provides thread-safe metrics without overhead
- **Error collection**: Collecting errors in Vec<SyncError> during parallel execution gives users comprehensive view of all failures
- **Documentation reorganization**: Following agent-contexts v0.1.1 patterns with docs/architecture/ and ai/ separation provides clear structure and knowledge graduation path
- **Comprehensive documentation**: Documenting new features (--perf, error reporting) immediately after implementation helps users discover and use them
- **Pre-transfer checksums** (v0.0.35): Computing xxHash3 checksums during planning phase before transfer saves bandwidth on re-syncs and detects bit rot

## What Didn't Work
- QUIC transport: 45% slower than TCP on fast networks (>600 Mbps) - documented in DESIGN.md
- Compression on local/high-speed: CPU bottleneck negates benefits above 4Gbps
- Initial sparse file tests: Had to make filesystem-agnostic due to varying FS support

## Active Work
- ✅ Completed Phase 5a: Pre-Transfer Checksums (v0.0.35)
  - ✅ Added checksum fields to SyncTask struct
  - ✅ Implemented checksum computation in StrategyPlanner
  - ✅ Added xxHash3-based comparison (15 GB/s throughput)
  - ✅ Checksums compared during planning to skip identical files
  - ✅ Added 3 comprehensive tests (all 317 tests passing)
  - ✅ Local→local support working
  - ✅ Documentation complete (README + TROUBLESHOOTING)
  - ✅ End-to-end testing verified (checksum comparison working perfectly)
  - 📋 Future: Remote checksum support (via sy-remote extension in Phase 5a+)

## Next Steps
- Phase 5b: Checksum Database (v0.0.36)
  - SQLite-based persistent checksum storage
  - 10-100x speedup for --checksum re-syncs
  - Automatic cache invalidation on mtime/size change
- Phase 5c: --verify-only mode (v0.0.37)
  - Audit file integrity without modification
  - Scriptable with JSON output + exit codes
- Compression auto-detection (backlog)

## Blockers
None currently

## Performance Metrics
- Local→Local: 1.3x - 8.8x faster than rsync
- Delta sync (100MB file): ~4x faster (320 MB/s vs 84 MB/s)
- COW strategy (APFS): 5-9x faster than rsync
- Parallel transfers: Scales well with concurrent operations

See docs/PERFORMANCE.md and docs/EVALUATION_v0.0.28.md for detailed benchmarks.
