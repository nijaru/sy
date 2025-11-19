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

- [ ] **Optional Features for Portability** [Cargo.toml, src/main.rs] (Next release)
  - [ ] SSH optional but default (`default = ["ssh"]`)
    - Makes local-only builds possible with zero system deps
    - Requires libssh2 on Linux when enabled
  - [ ] notify optional but default (`default = ["ssh", "watch"]`)
    - Pure Rust, no system deps, but allows minimal builds
    - For `--watch` mode continuous sync
  - **Goal**: Minimal builds possible, but default includes all common features



- [ ] **Optional SSH feature** [Cargo.toml, src/main.rs] (Next release)
  - [ ] Move ssh2, whoami, regex to optional feature
  - [ ] Set `default = ["ssh"]` so it's enabled by default
  - [ ] Update CONTRIBUTING.md with feature flag usage
  - [ ] Test local-only builds work without system deps
  - **Goal**: Support minimal installations, local-only syncs possible
  - **Impact**: No libssh2 needed for local sync (rare but valuable)

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
