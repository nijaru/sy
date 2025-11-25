# Status

## Current State
- Version: v0.0.63 (released 2025-11-24)
- **Next Release Goal**: v0.1.0 (Production Readiness)
- Test Coverage: **480 tests passing** ✅ (Cross-platform verified)
- **Current Build**: 🟢 PASSING

## Feature Flags
- SSH: Optional (enabled by default)
- Watch: Optional (disabled by default)
- ACL: Optional (Linux requires libacl-dev, macOS works natively)
- S3: Optional (disabled by default)

## Recent Work
- **Parallel Scanner** (5befa94): Implemented parallel directory scanning
  - Uses `ignore::WalkParallel` with `crossbeam-channel` bridge
  - Automatically parallel when threads > 1 (default: num_cpus)
  - Sequential fallback for threads=0/1
  - 5 new tests for correctness

## Next Up
- Performance benchmarking of parallel scanner
- Consider version bump for parallel scanner feature

## Recent Releases

### v0.0.63 (Bug Fixes)
- Bisync timestamp overflow fix
- Size parsing overflow check
- CLI flag improvements (--no-resume)
- Removed unsafe code in rolling hash (safe version same perf)

### v0.0.62 (Performance)
- Parallel Chunk Transfers over SSH
- Adaptive Compression
- Adler32 Optimization (7x faster)
