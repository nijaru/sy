# TODO

## Active: v0.1.0 Breaking Changes

### Breaking Change: Gitignore Default Flip

[Issue #11](https://github.com/nijaru/sy/issues/11) - Defaults differ from rsync

- [ ] Flip `ScanOptions::default()` in `src/sync/scanner.rs:168-175`
- [ ] Add `--gitignore` flag (opt-in)
- [ ] Add `--exclude-vcs` flag (opt-in)
- [ ] Remove `--no-gitignore` and `--include-vcs` flags
- [ ] Update `scan_options()` logic

### Breaking Change: `-b` Flag Conflict

**CRITICAL**: rsync `-b` = backup, sy `-b` = bidirectional

- [ ] Change `-b` → `-B` (or remove short flag)
- [ ] Document in migration guide

### CLI Compatibility

- [ ] Add `-z` short flag for `--compress`
- [ ] Add `-u` / `--update` (skip if dest newer)
- [ ] Add `--ignore-existing` (skip if dest exists)
- [ ] Consider `-l` for symlinks
- [ ] Consider `-P` for progress

### Tests

- [ ] Update default behavior tests
- [ ] Add tests for new flags
- [ ] Run full test suite

### Documentation

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
