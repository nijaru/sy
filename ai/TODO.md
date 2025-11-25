# TODO

## Active: v0.1.0 (Production Readiness)

### Completed

- [x] **Parallel scanner** (`src/sync/scanner.rs`) - 5befa94
  - Uses `ignore::WalkParallel` with `crossbeam-channel` bridge
  - Automatic parallel when threads > 1, sequential fallback otherwise

### Low Priority

- [ ] **Sequential filter loading** (`src/main.rs`) - LOW VALUE
  - Filter files typically small, parallelization adds complexity

- [ ] **SSH BSD flags** - NOT FIXABLE
  - Can't set BSD flags remotely without protocol extension

### Platform Support

- [ ] Implement sparse file detection (Windows DeviceIoControl)
- [ ] Verify ACL mapping across platforms
- [ ] Test path handling edge cases (UNC paths, drive letters)

### Future Features (Post-v0.1.0)

- [ ] Re-evaluate SIMD for checksums if bottlenecks reappear
- [ ] Replace `libssh2` with pure Rust implementation
- [ ] Enable full two-way sync for cloud storage
