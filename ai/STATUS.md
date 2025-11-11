# Status

_Last Updated: 2025-11-10_

## Current State
- Version: v0.0.57 (released 2025-11-10) ✅
- Latest Work: **Documentation overhaul** - README rewrite, comprehensive docs created
- Previous: Issues #2 and #4 fixed (trailing slash semantics, remote nested files)
- Test Coverage: **484 tests total (100% passing)** ✅
  - **Library tests**: 465 passing (core functionality)
  - **Integration tests**: 14 passing (property tests, delta sync, etc.)
  - **Trailing slash tests**: 5 passing (rsync compatibility)
  - **SSH tests**: 48 tests (12 ignored - require SSH setup)
  - **Platform validation**:
    - macOS: 465 tests passing ✅
    - Fedora: 462 tests passing ✅
- Build: Passing (all tests green, 0 warnings)
- Performance: 2-11x faster than rsync (see docs/BENCHMARK_RESULTS.md)
- Memory: 100x reduction for large file sets (1.5GB → 15MB for 100K files)

## v0.0.57 Release Notes

**Fixed**:
1. **Rsync-compatible trailing slash semantics** (Issue #2, PR #5)
   - Without trailing slash: copies directory itself (e.g., `sy /a/dir /target` → `/target/dir/`)
   - With trailing slash: copies contents only (e.g., `sy /a/dir/ /target` → `/target/`)
   - Works consistently across local, SSH, and S3 transports
   - Added comprehensive tests for detection and destination computation

2. **Remote sync nested file creation** (Issue #4, PR #4)
   - Fixed remote sync failures when creating files in nested directories
   - Ensures parent directories exist before file creation on remote destinations
   - Tested with SSH sync to verify proper directory hierarchy creation

**Changed**:
- **Documentation overhaul**
  - Rewrote README.md from 1161 lines to 198 lines (83% reduction)
  - Created comprehensive docs/FEATURES.md (861 lines) with feature categorization
  - Created comprehensive docs/USAGE.md (1139 lines) with real-world examples
  - Simplified comparison tables to only compare against rsync
  - Marked S3/cloud storage as experimental throughout documentation

- **OpenSSL compatibility**
  - Reverted to system OpenSSL for better cross-platform compatibility
  - Vendored OpenSSL broke on Linux builds
  - Tested on macOS (465 tests) and Fedora (462 tests)

## Next Up

See `ai/TODO.md` for active work priorities.

Key items:
- CI/CD infrastructure (simplified 3-platform testing)
- S3 testing and stabilization
- Performance profiling for large-scale syncs
