# Release Plan: v0.1.0 (Production Readiness)

**Status**: Planning
**Trigger**: [Issue #11 comment](https://github.com/nijaru/sy/issues/11#issuecomment-3573509820) - defaults differ from rsync

## v0.1.0 Goals

1. **Breaking Changes**: Bundle API-breaking changes (gitignore defaults)
2. **rsync Compatibility**: Match user expectations for sync tools
3. **Production Readiness**: Clear migration path, no silent data loss risks

## Breaking Change: Gitignore Default Flip

### The Problem

| Behavior | Current (v0.0.x) | rsync/cp | v0.1.0 Target |
|----------|------------------|----------|---------------|
| `.gitignore` | Respected (skip) | Ignored (copy all) | **Copy all** |
| `.git/` dirs | Excluded | Included | **Include** |

**Risk**: Silent exclusions cause unexpected data loss for users expecting rsync behavior.

### Implementation

#### Phase 1: Code Changes

| File | Change | Lines |
|------|--------|-------|
| `src/sync/scanner.rs` | Flip `ScanOptions::default()` | 168-175 |
| `src/cli.rs` | Add `--gitignore` flag (opt-in) | ~377 |
| `src/cli.rs` | Update `scan_options()` logic | 598-612 |
| `src/cli.rs` | Update help text | 364-385 |

#### Phase 2: Test Updates

| File | Tests to Update |
|------|-----------------|
| `src/cli.rs` | `test_scan_options_default`, `test_scan_options_archive_mode` |
| `tests/archive_mode_test.rs` | `test_default_respects_gitignore`, `test_default_excludes_git` |

#### Phase 3: Documentation

- README.md: Update default behavior
- CHANGELOG.md: Breaking change + migration guide
- --help text: Update flag descriptions

### New CLI Behavior

```
Default:        Copy all files (rsync-compatible)
--gitignore     Respect .gitignore rules (opt-in for dev workflows)
--exclude-vcs   Exclude .git directories (opt-in)
-a/--archive    Unchanged (already copies everything)

REMOVED (no deprecation, just remove):
--no-gitignore  (now default behavior)
--include-vcs   (now default behavior)
```

### Migration Guide

```markdown
## v0.1.0 Breaking Changes

### Default Behavior Changed (rsync-compatible)

**Before**: sy respected .gitignore and excluded .git by default
**After**: sy copies all files by default (like rsync)

**If you need the old behavior**:
  sy source dest --gitignore --exclude .git
```

## Completed (v0.0.61-v0.0.65)

- ✅ Auto-deploy `sy-remote` (zero-config remote sync)
- ✅ Optional SSH/ACL features (minimal dependencies)
- ✅ Streaming pipeline (75% memory reduction)
- ✅ Parallel chunk transfers over SSH
- ✅ Adaptive compression
- ✅ 527+ tests passing
- ✅ Integration tests for CLI flags

## Other v0.1.0 Considerations

### Keep As-Is (Good Defaults)

| Feature | Default | Keep? |
|---------|---------|-------|
| Resume support | Enabled | ✅ Yes |
| Verification | Standard | ✅ Yes |
| Parallel workers | 10 | ✅ Yes |
| Symlinks | Preserve | ✅ Yes |
| Delete threshold | 50% | ✅ Yes |

### Deferred (Post-v0.1.0)

- Windows support (sparse files, ACLs)
- russh migration (SSH agent blocker)
- S3 bidirectional sync

## Intentional Differences from rsync (Keep)

| Behavior | rsync | sy | Rationale |
|----------|-------|-----|-----------|
| Verification | size+mtime | xxHash3 | sy's value prop: "verification-first" |
| Recursion | Explicit `-r` | Implicit | Better UX, less confusing |
| Resume | None | Auto-enabled | Enhancement over rsync |
| Error limit | Implicit | Configurable `--max-errors` | Better control |

Users who want rsync-speed can use `--mode fast`.

## Resolved Questions

1. ~~Deprecation warnings?~~ → **No.** Remove flags entirely (0.0.x → 0.1.0 expects breaking changes)
2. Archive mode (`-a`)? → **Unchanged** (already copies everything)
3. Verification default? → **Keep Standard** (xxHash3 is sy's differentiator)

## Checklist

### Code Changes
- [ ] Flip `ScanOptions::default()` in `src/sync/scanner.rs:168-175`
- [ ] Add `--gitignore` flag (opt-in to respect .gitignore)
- [ ] Add `--exclude-vcs` flag (opt-in to exclude .git)
- [ ] Remove `--no-gitignore` flag
- [ ] Remove `--include-vcs` flag
- [ ] Update `scan_options()` logic in `src/cli.rs`
- [ ] Update archive mode (`-a`) - may need adjustment

### Tests
- [ ] Update `test_scan_options_default` (flip assertions)
- [ ] Update `test_scan_options_archive_mode`
- [ ] Update `tests/archive_mode_test.rs` tests
- [ ] Add tests for new `--gitignore` flag
- [ ] Add tests for new `--exclude-vcs` flag
- [ ] Run full test suite: `cargo test`

### Documentation
- [ ] Update README.md (default behavior section)
- [ ] Update CHANGELOG.md (breaking changes + migration)
- [ ] Update --help text in cli.rs

### Release
- [ ] Bump version to 0.1.0 in Cargo.toml
- [ ] Final `cargo test && cargo clippy`
- [ ] Tag and release
