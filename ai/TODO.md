# TODO

## v0.1.0 Breaking Changes - COMPLETED ✅

[Issue #11](https://github.com/nijaru/sy/issues/11) - Defaults now match rsync behavior.

### Completed

- [x] Flip `ScanOptions::default()` in `src/sync/scanner.rs`
- [x] Add `--gitignore` flag (opt-in)
- [x] Add `--exclude-vcs` flag (opt-in)
- [x] Remove `--no-gitignore` and `--include-vcs` flags
- [x] Remove `-b` short flag (rsync conflict)
- [x] Add `-z` short flag for `--compress`
- [x] Add `-u` / `--update` (skip if dest newer)
- [x] Add `--ignore-existing` (skip if dest exists)
- [x] Update `scan_options()` logic
- [x] Update all tests for new defaults
- [x] Update README.md
- [x] Update CHANGELOG.md with migration guide
- [x] Bump version to 0.1.0

### Ready for Release

All breaking changes implemented. Ready for final testing and release.

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
