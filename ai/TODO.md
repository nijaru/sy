# TODO

## Active: v0.1.0 (Production Readiness)

### ✅ Integration Test Coverage (Complete)

Added 38 new integration tests for CLI flags. See `ai/STATUS.md` for details.

- [x] `tests/archive_mode_test.rs` - 10 tests
- [x] `tests/filter_cli_test.rs` - 11 tests
- [x] `tests/comparison_modes_test.rs` - 8 tests
- [x] `tests/size_filter_test.rs` - 9 tests

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
