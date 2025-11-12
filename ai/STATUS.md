# Status

_Last Updated: 2025-11-12_

## Current State
- Version: v0.0.59 (in progress) 🚧
- Latest Work: **Optional ACL feature** - ACL preservation now optional, zero system dependencies on Linux
- Previous: v0.0.58 - Pure Rust library migrations (fjall + object_store)
- Test Coverage: **465 tests passing** ✅
  - **Library tests**: 465 passing (core functionality)
  - **SSH tests**: 48 tests (12 ignored - require SSH setup)
  - **Platform validation**:
    - macOS: 465 tests passing ✅
    - Linux (Fedora): 462 tests passing ✅ (from v0.0.57)
- Build: Passing (all tests green)
- Performance: 2-11x faster than rsync (see docs/BENCHMARK_RESULTS.md)
- Memory: 100x reduction for large file sets (1.5GB → 15MB for 100K files)

## v0.0.59 (In Progress)

**Optional ACL Feature** ✅

Made ACL preservation optional to eliminate system dependencies on Linux:

1. **Feature flag implementation** ✅
   - ACL support now behind `--features acl` flag
   - Default build requires zero system dependencies
   - Scope: Cargo.toml, src/main.rs, src/sync/scanner.rs, src/transport/mod.rs, src/sync/transfer.rs

2. **Platform support** ✅
   - Linux: Requires `libacl1-dev` (Debian/Ubuntu) or `libacl-devel` (Fedora/RHEL) at build time
   - macOS: Works with native ACL APIs (no external dependencies)
   - Clear runtime error message if `--preserve-acls` used without feature

3. **Testing** ✅
   - Created `scripts/test-acl-portability.sh` for Docker-based testing
   - Validates: default build, ACL build without libs (fails), ACL build with libs (succeeds), runtime errors
   - All 4 portability tests passing in Fedora container

4. **Documentation** ✅
   - Updated README.md with feature installation instructions
   - Updated CONTRIBUTING.md with build options
   - Clarified build vs runtime requirements

**Impact**:
- `cargo install sy` now works on all Linux systems without installing libacl
- Users who need ACL preservation: `cargo install sy --features acl`
- Follows same pattern as S3: opt-in features for advanced use cases

**Branch**: `feat/optional-acls` (ready for PR)

## v0.0.58 Release Notes

**Pure Rust Library Migrations** ✅

Migrated from C dependencies to pure Rust:

1. **rusqlite → fjall** - Pure Rust LSM-tree database, 56% faster writes
2. **aws-sdk-s3 → object_store** - Unified multi-cloud API, 38% code reduction
3. **walkdir removal** - Cleaned up unused dependency

**Dependency Impact**: Net ~18 fewer transitive dependencies

See `ai/research/library-migration-summary.md` for details.

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
- v0.0.59 release (ACL optional feature complete)
- CI/CD infrastructure (macOS + Linux testing)
- Consider SSH optional feature (similar to ACL pattern)
- Performance profiling for large-scale syncs
