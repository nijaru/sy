# Status

## Current State
- Version: v0.0.60 ✅
- Latest Release: **v0.0.60** - Critical memory bug fixes + Optional ACL feature
- Test Coverage: **464 tests passing** ✅
  - **Library tests**: 464 passing (core functionality)
  - **SSH tests**: 12 ignored (require SSH setup)
  - **Platform validation**:
    - macOS: tests passing ✅
    - Linux (Fedora): tests passing ✅
- Build: Passing (cargo clippy clean with --all-features)
- Performance: 2-11x faster than rsync
- Memory: 5000x better for large file verification (10GB file: 10GB RAM → 2MB RAM)

## v0.0.60 Release Notes

**Critical Bug Fixes** ✅ (PR #2 - Merged)

Fixed 4 critical bugs causing OOM errors and data safety issues:

1. **Memory bug in file verification (CRITICAL)** ✅
   - Large files (10GB+) loaded entirely into RAM during checksum verification
   - Added streaming checksums with 1MB chunks (10GB file: 10GB RAM → 2MB RAM)
   - Implemented `Transport.compute_checksum()` with streaming support
   - Added `sy-remote file-checksum` command for remote checksum computation
   - Files: src/transport/{mod,ssh}.rs, src/bin/sy-remote.rs, src/sync/mod.rs

2. **Remote checksum failure (HIGH)** ✅
   - `--checksum` mode failed for remote paths (tried to access remote files locally)
   - SSH transport now computes checksums remotely via command execution
   - Updated `compare_checksums()` to be async and use transport layer
   - Supports both fast (xxHash3) and cryptographic (BLAKE3) modes
   - Files: src/transport/ssh.rs, src/bin/sy-remote.rs

3. **Stale resume states (MEDIUM)** ✅
   - Failed syncs left resume state files accumulating indefinitely
   - Added `TransferState::clear_stale_states()` with 7-day auto-cleanup
   - Runs automatically at start of every sync operation
   - Files: src/resume.rs, src/sync/mod.rs

4. **Unsafe force-delete (HIGH)** ✅
   - `--force-delete` bypassed ALL safety checks (no warning for mass deletion)
   - Added catastrophic deletion threshold (10,000 files)
   - Requires explicit confirmation: `"DELETE <count>"` (case-sensitive)
   - Still respects `--quiet` and `--json` for automation
   - Files: src/sync/mod.rs

**Performance Improvements** ✅

5. **DualTransport optimization** ✅
   - Smart delegation avoids buffering when destination supports streaming
   - Local→SSH: 5GB RAM → 2MB RAM (now uses SFTP streaming)
   - Files: src/transport/dual.rs

6. **S3 streaming uploads** ✅
   - Large files (≥5MB) now use multipart upload with 5MB chunks
   - 10GB S3 upload: 10GB RAM → 5MB RAM
   - Files: src/transport/s3.rs, Cargo.toml (added tokio-util)

7. **Compression size limit** ✅
   - Added 256MB limit to prevent OOM on huge files
   - Documented rationale (sy-remote protocol requires buffering)
   - Files: src/compress/mod.rs

8. **Fixed blocking I/O in async context** ✅
   - Wrapped `std::fs::metadata()` in `spawn_blocking`
   - Proper async Rust idioms
   - Files: src/transport/ssh.rs

**Merged**: PR #2 (commit 5d3ce3d)

---

**Optional ACL Feature** ✅ (PR #8 - Merged)

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

**Merged**: PR #8 (commit fb94264)

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
- CI/CD infrastructure (macOS + Linux testing) - Next priority
- Consider SSH optional feature (similar to ACL pattern)
- Performance profiling for large-scale syncs
- Auto-deploy sy-remote on SSH connections
