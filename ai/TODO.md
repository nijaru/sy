# TODO

_Last Updated: 2025-11-10_

## Active Work

### High Priority

- [ ] **Release v0.0.58** (Ready)
  - [x] fjall migration complete
  - [x] object_store migration complete
  - [x] All tests passing
  - [ ] Update CHANGELOG.md
  - [ ] Tag and release

- [ ] **CI/CD Infrastructure** (v0.0.59)
  - [ ] Create simplified CI workflow for macOS + Linux
  - [ ] Run tests on 2 platforms (ubuntu-latest, macos-latest)
  - [ ] Add clippy and rustfmt checks
  - [ ] Keep it simple - no multi-version testing, no coverage reports
  - [ ] Document Windows as untested (experimental support)
  - **Goal**: Catch cross-platform regressions automatically

### Medium Priority

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

## Recently Completed (v0.0.58)

- [x] **Pure Rust Library Migrations** ✅
  - [x] rusqlite → fjall (pure Rust LSM-tree database)
  - [x] aws-sdk-s3 → object_store (unified cloud API)
  - [x] Remove walkdir dependency
  - [x] Fix SyncPath patterns (S3 feature now compiles)
  - [x] Net: 4 dependencies removed, 2 added
  - **See**: `ai/library-migration-summary.md`

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
