# Status

## Current State
- Version: v0.0.63 (released 2025-11-24)
- **Next Release Goal**: v0.1.0 (Production Readiness)
- Test Coverage: **475 tests passing** ✅ (Cross-platform verified)
- **Current Build**: 🟢 PASSING

## Feature Flags
- SSH: Optional (enabled by default)
- Watch: Optional (disabled by default)
- ACL: Optional (Linux requires libacl-dev, macOS works natively)
- S3: Optional (disabled by default)

## Recent Work (v0.0.63)
- **Code Review Fixes**: Bisync timestamp overflow, size parsing, CLI flags
- **Safety**: Removed unnecessary unsafe code (Adler-32 safe indexing identical perf)
- **Cleanup**: Removed dead code (verify_only field), updated comments

## Next Up
- **Parallel Scanner**: Use `ignore` crate's `build_parallel()` with channel adapter
  - Plan: crossbeam-channel to bridge push→pull (already transitive dep)
  - Estimated: 2-4x speedup on multi-core with many subdirs

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
