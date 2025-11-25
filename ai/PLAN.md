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
--no-gitignore  DEPRECATED (now default)
--include-vcs   DEPRECATED (now default)
-a/--archive    Unchanged (already copies everything)
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

## Open Questions

1. Should deprecated flags emit warnings? → Recommend: yes, for 1-2 releases
2. Archive mode (`-a`) unchanged? → Yes, already copies everything
3. Remove deprecated flags in v0.2.0? → Discuss after user feedback

## Checklist

- [ ] Review all affected code (scanner.rs, cli.rs)
- [ ] Implement `--gitignore` flag
- [ ] Update `ScanOptions::default()`
- [ ] Update `scan_options()` logic
- [ ] Update help text
- [ ] Update unit tests
- [ ] Update integration tests
- [ ] Run full test suite
- [ ] Update README.md
- [ ] Update CHANGELOG.md
- [ ] Bump version to 0.1.0
- [ ] Tag and release
