# TODO

## Active Work

### High Priority

- [x] **CI/CD Infrastructure** ✅ (Released v0.0.60)
  - [x] Create simplified CI workflow for macOS + Linux
  - [x] Run tests on 2 platforms (ubuntu-latest, macos-latest)
  - [x] Add clippy and rustfmt checks
  - [x] Keep it simple - no multi-version testing, no coverage reports
  - [x] Document Windows as untested (experimental support)
  - **Goal**: Catch cross-platform regressions automatically
  - **Context**: CI pipeline active on push/PR to main

- [x] **Auto-deploy sy-remote on SSH connections** ✅ (Commit e8036ff)
  - [x] Create binary::find_sy_remote_binary() with fallback search
  - [x] Implement binary::read_sy_remote_binary() for in-memory loading
  - [x] Add SshTransport::deploy_sy_remote_locked() for deployment
  - [x] Detect exit code 127 and auto-retry with deployed path
  - [x] Create ~/.sy/bin on remote with proper permissions
  - [x] Upload via SFTP and set 0o755 permissions
  - [x] Test: All 465 tests passing
  - **Impact**: Zero-setup UX for remote servers (no pre-install needed)
  - **Performance**: ~4MB, ~200ms on LAN

### Medium Priority

- [x] **Optional SSH Feature Flag** ✅ (Commit 9e6c748)
  - [x] SSH marked optional with `ssh = ["dep:ssh2", "dep:whoami", "dep:regex"]`
  - [x] Default features include SSH for backward compatibility
  - [x] Library fully supports optional SSH (all gates in place)
  - [x] Router gracefully errors when SSH disabled
  - [x] All tests passing with feature enabled
  - [ ] **Future**: Gate main.rs/watch.rs for true CLI support without SSH
  - **Impact**: Library users can build without system deps
  - **Build example**: `cargo build --no-default-features` or with SSH: `--features ssh`

- [ ] **Optional notify Feature** [Cargo.toml] (Future)
  - [ ] Make watch mode notifications optional (Pure Rust, already low-level)
  - [ ] Set `default = ["ssh", "watch"]`
  - **Goal**: Allow minimal headless builds

- [ ] **russh Migration** [src/transport/ssh.rs] (v0.0.59) - WIP on `feature/russh-migration` branch
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

- [ ] **S3/Cloud Testing** [src/transport/s3.rs, tests/] (Future)
  - [x] Migrate to `object_store` crate ✅
  - [ ] Add integration tests for S3 sync
  - [ ] Test with AWS, Cloudflare R2, Backblaze B2, Wasabi
  - [ ] Document authentication patterns
  - [ ] Remove "experimental" tag once proven stable

- [ ] **Windows Platform Support** [src/transport/local.rs] (Future)
  - [ ] Implement sparse file detection on Windows
    - Use `DeviceIoControl` with `FSCTL_QUERY_ALLOCATED_RANGES`
    - Currently falls back to regular copy (Unix-only implementation)
  - [ ] Test ACLs on Windows (different from POSIX)
  - [ ] Test NTFS-specific features
  - [ ] Verify extended attributes work correctly

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
