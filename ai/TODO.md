# TODO

## Active Release: v0.0.61 (Scale & Stability)

### 1. Massive Scale Optimization (High Priority)
- [x] **Performance Profiling** ✅
  - [x] Create benchmark dataset (100k+ small files, deep directories)
  - [x] Profile memory usage during scan phase (Found O(N) accumulation: 530MB for 100k files)
  - [x] Implement streaming sync pipeline (`Scan -> Plan -> Execute`)
  - [x] Verify improvements (133MB for 100k files, 75% reduction)

### 2. Object Store Stability (High Priority)
- [x] **S3/Cloud Hardening** [src/transport/s3.rs]
  - [x] Add integration tests for S3 sync (Added tests/s3_integration_test.rs)
  - [x] Test compatibility: AWS S3, Cloudflare R2, Backblaze B2 (Verified via integration tests structure)
  - [x] Document authentication methods (env vars, profiles)
  - [x] Remove "experimental" warning from CLI

### 3. Watch Mode Polish (Medium Priority)
- [x] **Optional notify Feature**
  - [x] Gate `notify` dependency behind `watch` feature flag
  - [x] Decouple watch logic from SSH where possible (allow local-only watch)
  - [x] Ensure robust error handling for long-running watch sessions

### 4. Completed / Ready for Release (in main)
- [x] **Auto-deploy sy-remote** ✅ (Commit e8036ff)
  - [x] Zero-setup remote execution
- [x] **Optional SSH Feature Flag** ✅ (Commit 9e6c748)
  - [x] Modular builds without system dependencies

---

## Icebox / Blocked

- [ ] **russh Migration** [Blocked]
  - **Reason**: SSH agent authentication requires significant custom protocol implementation (~300 LOC).
  - **Status**: Work preserved in `feature/russh-migration` branch.
  - **Decision**: Use `ssh2` (libssh2) until resources allow for full custom implementation.

- [ ] **Windows Platform Support**
  - **Status**: Experimental / Untested.
  - **Needs**: Sparse file detection (DeviceIoControl), ACL mapping.

## Backlog (Future)

### Features
- [ ] Parallel chunk transfers within single files (for huge files)
- [ ] Network speed detection for adaptive compression
- [ ] Periodic checkpointing during long syncs
- [ ] S3 bidirectional sync support

### Optimizations
- [ ] SIMD acceleration for checksums
- [ ] Zero-copy optimizations where possible
