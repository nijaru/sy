# Status

_Last Updated: 2025-10-22_

## Current State
- Version: v0.0.39 (in development)
- Phase: Bandwidth Utilization JSON output complete!
- Test Coverage: 339 tests passing (329 lib + 8 checksumdb + 1 verification + 1 performance)
- Build: Passing (all tests green)
- Performance: 1.3x - 8.8x faster than rsync (see docs/PERFORMANCE.md)

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

## What Didn't Work
- QUIC transport: 45% slower than TCP on fast networks (>600 Mbps) - documented in DESIGN.md
- Compression on local/high-speed: CPU bottleneck negates benefits above 4Gbps
- Initial sparse file tests: Had to make filesystem-agnostic due to varying FS support

## Active Work
- ✅ Completed Bandwidth Utilization JSON Output (v0.0.39)
  - Added Performance event to JSON output
  - Includes all PerformanceMetrics fields
  - Bandwidth utilization now accessible for automation
  - Feature was already working in --perf mode

## Next Steps
- Bandwidth utilization JSON output complete!
- Future enhancement: Thread CLI compression detection mode through transport (currently hardcoded to Auto)
- Future enhancement: Remote checksum support for Phase 5a/5b (backlog)
- Next major work from backlog: Symbolic link chain detection OR sparse file optimization improvements

## Blockers
None currently

## Performance Metrics
- Local→Local: 1.3x - 8.8x faster than rsync
- Delta sync (100MB file): ~4x faster (320 MB/s vs 84 MB/s)
- COW strategy (APFS): 5-9x faster than rsync
- Parallel transfers: Scales well with concurrent operations

See docs/PERFORMANCE.md and docs/EVALUATION_v0.0.28.md for detailed benchmarks.
