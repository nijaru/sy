# TODO

## Active Release: v0.1.0 (Production Readiness)

### 1. Quality & Safety
  - Address remaining `unwrap()` and `expect()` calls in non-critical paths (CLI, hooks)
  - Goal: Zero warnings with strict configuration
  - Ensure all user-facing errors have actionable suggestions
  - Audit `anyhow` vs `thiserror` usage (libraries should use `thiserror`)

### 2. Platform Support
  - Implement sparse file detection (DeviceIoControl)
  - Verify ACL mapping
  - Test path handling edge cases (UNC paths, drive letters)

### 3. Future Features (Post-v0.1.0)
  - Re-evaluate SIMD for checksums if bottlenecks reappear
  - Replace `libssh2` with pure Rust implementation for better portability
  - Enable full two-way sync for cloud storage