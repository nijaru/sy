# Status

_Last Updated: 2025-10-22_

## Current State
- Version: v0.0.37 (in development)
- Phase: Compression Auto-Detection implemented!
- Test Coverage: 338 tests passing (328 lib + 8 checksumdb + 1 verification + 1 compression)
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

## What Didn't Work
- QUIC transport: 45% slower than TCP on fast networks (>600 Mbps) - documented in DESIGN.md
- Compression on local/high-speed: CPU bottleneck negates benefits above 4Gbps
- Initial sparse file tests: Had to make filesystem-agnostic due to varying FS support

## Active Work
- ✅ Completed Compression Auto-Detection (v0.0.37)
  - Content-based sampling with LZ4 (64KB samples)
  - 10% compression ratio threshold
  - CLI flag with 4 modes (auto|extension|always|never)
  - Integrated with SSH transport layer
  - 12 new tests (28 total compression tests)
  - Research documented in ai/research/compression_auto_detection.md

## Next Steps
- Compression auto-detection complete!
- Future enhancement: Thread CLI detection mode through transport architecture (currently hardcoded to Auto)
- Future enhancement: Remote checksum support for Phase 5a/5b (backlog)
- Future enhancement: Unit tests for verify() method internals (optional, e2e tests passing)
- Next major work from backlog: Enhanced progress display OR bandwidth utilization metrics

## Blockers
None currently

## Performance Metrics
- Local→Local: 1.3x - 8.8x faster than rsync
- Delta sync (100MB file): ~4x faster (320 MB/s vs 84 MB/s)
- COW strategy (APFS): 5-9x faster than rsync
- Parallel transfers: Scales well with concurrent operations

See docs/PERFORMANCE.md and docs/EVALUATION_v0.0.28.md for detailed benchmarks.
