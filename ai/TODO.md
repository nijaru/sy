# TODO

_Last Updated: 2025-11-12_

## Active Work

### High Priority

- [ ] **Release v0.0.59** (Ready)
  - [x] ACL optional feature complete
  - [x] All tests passing (465 tests)
  - [x] Docker portability tests passing
  - [x] Documentation updated (README, CONTRIBUTING)
  - [ ] Create PR for `feat/optional-acls`
  - [ ] Wait for CI to pass
  - [ ] Merge PR
  - [ ] Update CHANGELOG.md
  - [ ] Tag and release

- [ ] **CI/CD Infrastructure** (v0.0.60)
  - [ ] Create simplified CI workflow for macOS + Linux
  - [ ] Run tests on 2 platforms (ubuntu-latest, macos-latest)
  - [ ] Add clippy and rustfmt checks
  - [ ] Keep it simple - no multi-version testing, no coverage reports
  - [ ] Document Windows as untested (experimental support)
  - **Goal**: Catch cross-platform regressions automatically

### Medium Priority

- [ ] **Optional Features for Portability** (v0.0.60)
  - [ ] SSH optional but default (`default = ["ssh"]`)
    - Makes local-only builds possible with zero system deps
    - Requires libssh2 on Linux when enabled
    - Effort: 2-3 hours (similar to ACLs)
  - [ ] notify optional but default (`default = ["ssh", "watch"]`)
    - Pure Rust, no system deps, but allows minimal builds
    - For `--watch` mode continuous sync
    - Effort: 1-2 hours
  - **Goal**: Minimal builds possible, but default includes all common features



- [ ] **Auto-deploy sy-remote on SSH connections** (Future PR)
  - **Problem**: sy fails with "command not found" if sy-remote isn't installed on remote server
  - **Current**: User must manually `cargo install sy` on every remote server first
  - **Solution**: Auto-deploy sy-remote binary over SSH (copy from local machine)
  - **Edge cases to handle**:
    - Remote OS detection (Linux/macOS/BSD)
    - Architecture detection (x86_64/arm64/etc)
    - PATH setup (~/.cargo/bin)
    - Binary compatibility verification
    - Permission handling
  - **Alternative approaches**:
    1. Pre-flight check with helpful error message
    2. Auto-deploy prebuilt binaries (best UX, like rsync)
    3. Auto-build on remote (slow, requires Rust toolchain)
  - **Ref**: src/transport/ssh.rs:268-271 (error handling location)

- [ ] **russh Migration** (v0.0.59) - WIP on `feature/russh-migration` branch
  - [x] Dependencies updated (ssh2 → russh + russh-sftp + russh-keys)
  - [x] Connection handling rewritten
  - [x] Simple SFTP operations converted
  - [ ] SFTP file streaming conversion (~48 errors remaining)
  - [ ] Test SSH sync operations
  - **Branch**: `feature/russh-migration`
  - **Benefit**: 100% pure Rust stack (no C dependencies)
  - **See**: `ai/russh-migration.md` (on feature branch)

- [ ] **Performance Profiling** (Future)
  - [ ] Profile large-scale syncs (100K+ files)
  - [ ] Identify bottlenecks in parallel transfers
  - [ ] Optimize memory usage for massive directories

### Low Priority

- [ ] **S3/Cloud Testing** (Future)
  - [x] Migrate to `object_store` crate ✅
  - [ ] Add integration tests for S3 sync
  - [ ] Test with AWS, Cloudflare R2, Backblaze B2, Wasabi
  - [ ] Document authentication patterns
  - [ ] Remove "experimental" tag once proven stable

- [ ] **Windows Platform Support** (Future)
  - [ ] Implement sparse file detection on Windows
    - Use `DeviceIoControl` with `FSCTL_QUERY_ALLOCATED_RANGES`
    - Currently falls back to regular copy (Unix-only implementation)
  - [ ] Test ACLs on Windows (different from POSIX)
  - [ ] Test NTFS-specific features
  - [ ] Verify extended attributes work correctly

## Recently Completed (v0.0.59)

- [x] **Optional ACL Feature** ✅ (GitHub Issue #7)
  - [x] Made ACL preservation optional via `--features acl`
  - [x] Eliminated libacl system dependency for default builds
  - [x] Platform support: Linux (build-time libacl), macOS (native)
  - [x] Created Docker portability test suite (`scripts/test-acl-portability.sh`)
  - [x] Updated documentation (README, CONTRIBUTING)
  - [x] Clear runtime error messages for missing feature
  - **Impact**: `cargo install sy` now works on all Linux systems without libacl
  - **Branch**: `feat/optional-acls`

- [x] **AI Context Cleanup** ✅
  - [x] Deleted 13 obsolete research docs (completed features)
  - [x] Kept 2 current docs (library-migration-summary, database-comparisons)
  - [x] Updated STATUS.md, TODO.md, DECISIONS.md with ACL work
  - **Result**: Cleaner, more maintainable ai/ directory

## Recently Completed (v0.0.58)

- [x] **Pure Rust Library Migrations** ✅
  - [x] rusqlite → fjall (56% faster writes)
  - [x] aws-sdk-s3 → object_store (multi-cloud support)
  - [x] Removed walkdir dependency
  - **See**: `ai/research/library-migration-summary.md`

## Recently Completed (v0.0.57)

- [x] **Issue #2: Rsync-compatible trailing slash semantics** (PR #5)
  - [x] Implement trailing slash detection for SyncPath
  - [x] Add destination path computation logic
  - [x] Add comprehensive tests (5 tests)
  - [x] Works across local, SSH, and S3 transports

- [x] **Issue #4: Remote sync nested file creation** (PR #4)
  - [x] Fix DualTransport cross-transport copy
  - [x] Add parent directory creation
  - [x] Add regression tests

- [x] **Documentation Overhaul** (v0.0.57)
  - [x] Rewrite README.md (1161 → 198 lines)
  - [x] Create docs/FEATURES.md (861 lines)
  - [x] Create docs/USAGE.md (1139 lines)
  - [x] Mark S3 as experimental
  - [x] Simplify comparison tables (rsync only)

- [x] **OpenSSL Cross-Platform Fix** (v0.0.57)
  - [x] Revert vendored OpenSSL to system OpenSSL
  - [x] Test on macOS (465 tests passing)
  - [x] Test on Fedora (462 tests passing)

## Backlog (Future Versions)

### Features
- [ ] Parallel chunk transfers within single files
- [ ] Network speed detection for adaptive compression
- [ ] Periodic checkpointing during long syncs
- [ ] S3 bidirectional sync support
- [ ] Multi-destination sync (one source → multiple destinations)

### Optimizations
- [ ] SIMD acceleration for checksums
- [ ] Zero-copy optimizations where possible
- [ ] Further memory reduction for massive scale

### Platform Support
- [ ] Windows native builds and testing
- [ ] BSD platform support
- [ ] Android/Termux support

## Archive (Completed Phases)

All Phase 1-11 work is complete and shipped in versions v0.0.1 through v0.0.56. See CHANGELOG.md for full history.

Key completed phases:
- Phase 1: MVP (Local sync)
- Phase 2: Network + Delta (SSH transport, rsync algorithm)
- Phase 3: Parallelism + Optimization
- Phase 4: Advanced Features (hooks, watch mode, config profiles)
- Phase 5: Verification & Integrity
- Phase 6: Metadata Preservation
- Phase 7: Bidirectional Sync
- Phase 8: Production Hardening
- Phase 9: Developer Experience
- Phase 10: Cloud Era (S3 support)
- Phase 11: Scale optimizations
