# TODO

## Active Release: v0.1.0 (Production Readiness)

### 1. Quality & Safety
- [ ] **Clippy Cleanup** [src/main.rs, src/cli/mod.rs]
  - Address remaining `unwrap()` and `expect()` calls in non-critical paths (CLI, hooks)
  - Goal: Zero warnings with strict configuration
- [ ] **Error Handling Audit** [src/error.rs, src/sync/mod.rs]
  - Ensure all user-facing errors have actionable suggestions
  - Audit `anyhow` vs `thiserror` usage (libraries should use `thiserror`)

### 2. Platform Support
- [ ] **Windows Support** [src/fs_util.rs, src/metadata.rs]
  - Implement sparse file detection (DeviceIoControl)
  - Verify ACL mapping
  - Test path handling edge cases (UNC paths, drive letters)

### 3. Future Features (Post-v0.1.0)
- [ ] **SIMD Optimization** [src/integrity/xxhash.rs, src/integrity/blake3.rs]
  - Re-evaluate SIMD for checksums if bottlenecks reappear
- [ ] **russh Migration** [src/transport/ssh.rs]
  - Replace `libssh2` with pure Rust implementation for better portability
- [ ] **S3 Bidirectional Sync** [src/transport/s3.rs]
  - Enable full two-way sync for cloud storage

---

## Icebox / Blocked

- [ ] **russh Migration** [Blocked] [src/transport/ssh.rs]
  - **Reason**: SSH agent authentication requires significant custom protocol implementation (~300 LOC).
  - **Status**: Work preserved in `feature/russh-migration` branch.
  - **Decision**: Use `ssh2` (libssh2) until resources allow for full custom implementation.

## Backlog

- [ ] Zero-copy optimizations where possible
- [ ] GUI Frontend (maybe?)
