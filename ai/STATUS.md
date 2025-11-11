# Status

_Last Updated: 2025-11-10_

## Current State
- Version: v0.0.58 (in progress) 🚧
- Latest Work: **Pure Rust library migrations** - fjall + object_store complete
- Previous: v0.0.57 - Documentation overhaul, trailing slash semantics, nested file fixes
- Test Coverage: **465 tests passing** ✅
  - **Library tests**: 465 passing (core functionality)
  - **SSH tests**: 48 tests (12 ignored - require SSH setup)
  - **Platform validation**:
    - macOS: 465 tests passing ✅
    - Linux (Fedora): 462 tests passing ✅ (from v0.0.57)
- Build: Passing (all tests green)
- Performance: 2-11x faster than rsync (see docs/BENCHMARK_RESULTS.md)
- Memory: 100x reduction for large file sets (1.5GB → 15MB for 100K files)

## v0.0.58 (In Progress)

**Pure Rust Library Migrations** ✅

Migrated from C dependencies to pure Rust for easier cross-compilation, smaller binaries, and better developer experience:

1. **rusqlite → fjall** ✅
   - Replaced SQLite (C library) with fjall (pure Rust LSM-tree)
   - Scope: src/sync/checksumdb.rs (443 lines)
   - Better write performance (LSM-tree optimized for writes)
   - File location: `.sy-checksums.db` → `.sy-checksums/` (directory)
   - Testing: 11 tests passing

2. **aws-sdk-s3 → object_store** ✅
   - Unified cloud storage API (pure Rust)
   - Scope: src/transport/s3.rs (454 → ~280 lines, 38% code reduction)
   - Multi-cloud support: AWS S3, Cloudflare R2, Backblaze B2, Wasabi, GCS, Azure
   - Simpler API with automatic multipart uploads
   - Testing: Compiles cleanly with `--features s3`

3. **walkdir removal** ✅
   - Removed unused direct dependency

4. **SyncPath pattern fixes** ✅
   - Fixed broken S3 feature after PR #5 changes

**Dependency Impact**:
- Removed: rusqlite, aws-sdk-s3, aws-config, aws-smithy-types, walkdir (4 deps)
- Added: fjall, object_store, bytes (2 deps + utility)
- Net: ~18 fewer transitive dependencies

**Pure Rust Status**:
- ✅ Database: fjall (pure Rust)
- ✅ Cloud storage: object_store (pure Rust)
- ❌ SSH: ssh2 (C bindings) → russh migration planned for v0.0.59
- ✅ Directory traversal: ignore (pure Rust)
- ✅ Compression: zstd, lz4_flex (pure Rust)
- ✅ Hashing: xxhash-rust, blake3 (pure Rust)

See `ai/library-migration-summary.md` for full migration details.

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
- v0.0.58 release (library migrations complete)
- russh migration for v0.0.59 (pure Rust SSH)
- CI/CD infrastructure (simplified 3-platform testing)
- Performance profiling for large-scale syncs
