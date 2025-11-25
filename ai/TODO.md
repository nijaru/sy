# TODO

## Active: v0.1.0 Breaking Changes

### Breaking Change: Gitignore Default Flip

[Issue #11](https://github.com/nijaru/sy/issues/11) - Defaults differ from rsync

- [ ] **Phase 1: Code Changes**
  - [ ] Flip `ScanOptions::default()` in `src/sync/scanner.rs:168-175`
  - [ ] Add `--gitignore` flag to `src/cli.rs`
  - [ ] Update `scan_options()` logic in `src/cli.rs:598-612`
  - [ ] Update help text for deprecated flags

- [ ] **Phase 2: Test Updates**
  - [ ] Update `test_scan_options_default` in `src/cli.rs`
  - [ ] Update `test_scan_options_archive_mode` in `src/cli.rs`
  - [ ] Update tests in `tests/archive_mode_test.rs`
  - [ ] Add tests for new `--gitignore` flag

- [ ] **Phase 3: Documentation**
  - [ ] Update README.md
  - [ ] Update CHANGELOG.md with migration guide
  - [ ] Bump version to 0.1.0

See `ai/PLAN.md` for full implementation details.

### ✅ Integration Test Coverage (Complete)

- [x] `tests/archive_mode_test.rs` - 10 tests
- [x] `tests/filter_cli_test.rs` - 11 tests
- [x] `tests/comparison_modes_test.rs` - 8 tests
- [x] `tests/size_filter_test.rs` - 9 tests

### Deferred (Post-v0.1.0)

- [ ] Windows support (sparse files, ACLs, path edge cases)
- [ ] russh migration (SSH agent blocker)
- [ ] S3 bidirectional sync
- [ ] SIMD optimization (if bottlenecks reappear)
