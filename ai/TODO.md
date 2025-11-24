# TODO

## Active: v0.1.0 (Production Readiness)

### High Priority

- [ ] **Parallel scanner** (`src/sync/scanner.rs`)
  - Use `ignore` crate's `build_parallel()` with `crossbeam-channel`
  - Bridge push-based visitor to pull-based iterator
  - Benefit: 2-4x speedup on multi-core with many subdirectories
  - Deps: Both already transitive (promote to direct)

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
