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

### CHANGELOG Draft

```markdown
## v0.1.0 - Breaking Changes (rsync Compatibility)

### BREAKING: Default Behavior Changes

sy now copies all files by default, matching rsync behavior:

| Behavior | v0.0.x | v0.1.0 |
|----------|--------|--------|
| `.gitignore` rules | Respected (files skipped) | **Ignored (all files copied)** |
| `.git/` directories | Excluded | **Included** |

**Migration**: If you relied on the old behavior:
```bash
# Old (v0.0.x): sy copied only non-ignored files
sy /src /dest

# New (v0.1.0): Use explicit flags for old behavior
sy /src /dest --gitignore --exclude-vcs
```

### BREAKING: Flag Changes

**Removed flags** (no longer needed):
- `--no-gitignore` → Now default behavior
- `--include-vcs` → Now default behavior
- `-b` short flag → Use `--bidirectional` (conflicts with rsync `-b`=backup)

**New flags**:
- `--gitignore` — Opt-in to respect .gitignore rules
- `--exclude-vcs` — Opt-in to exclude .git directories
- `-z` — Short for `--compress` (rsync compatible)
- `-u` / `--update` — Skip files where destination is newer
- `--ignore-existing` — Skip files that already exist in destination

### rsync Compatibility Notes

sy is intentionally NOT a drop-in rsync replacement. Key differences:

| Feature | rsync | sy | Rationale |
|---------|-------|-----|-----------|
| Verification | size+mtime | xxHash3 | Catches silent corruption |
| Recursion | Requires `-r` | Implicit | Better UX |
| Resume | Manual | Automatic | Handles interruptions |
| `-b` flag | Backup | (removed) | Conflict avoidance |

For rsync-like speed without verification: `sy --mode fast`
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

## Critical: `-b` Flag Conflict

| Tool | `-b` means |
|------|-----------|
| rsync | `--backup` (make backups before overwrite) |
| sy | `--bidirectional` (two-way sync) |

**Risk**: User runs `sy -avb /src /dest` expecting rsync behavior, gets bidirectional sync.

**Decision**: Change `-b` to `-B` or remove short flag entirely.

## Missing Short Flags (Add for v0.1.0)

| rsync | sy current | v0.1.0 |
|-------|------------|--------|
| `-z` (compress) | `--compress` only | Add `-z` |
| `-l` (symlinks) | `--links preserve` | Consider `-l` |
| `-P` (progress) | `--per-file-progress` | Consider `-P` |

## Missing rsync Features (Add for v0.1.0)

| rsync flag | Purpose | Complexity | Add? |
|------------|---------|------------|------|
| `--update` / `-u` | Skip if dest is newer | Low | **Yes** |
| `--ignore-existing` | Skip if dest exists | Low | **Yes** |
| `--backup` | Make backups | Medium | No (v0.2.0) |

Implementation in `src/sync/strategy.rs`:
- Add `update_only: bool` and `ignore_existing: bool` to `StrategyPlanner`
- Check in `plan_file_async`: if dest newer/exists → `SyncAction::Skip`

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

### Code Changes - Defaults
- [ ] Flip `ScanOptions::default()` in `src/sync/scanner.rs:168-175`
- [ ] Add `--gitignore` flag (opt-in to respect .gitignore)
- [ ] Add `--exclude-vcs` flag (opt-in to exclude .git)
- [ ] Remove `--no-gitignore` flag
- [ ] Remove `--include-vcs` flag
- [ ] Update `scan_options()` logic in `src/cli.rs`

### Code Changes - CLI Compatibility
- [ ] Change `-b` short flag (bidirectional) → remove (no short flag)
- [ ] Add `-z` short flag for `--compress`
- [ ] Add `-u` / `--update` flag (skip if dest newer)
- [ ] Add `--ignore-existing` flag (skip if dest exists)
- [ ] Consider `-l` for symlinks (complex: --links takes value)
- [ ] Consider `-P` for progress

### Tests
- [ ] Update `test_scan_options_default` (flip assertions)
- [ ] Update `test_scan_options_archive_mode`
- [ ] Update `tests/archive_mode_test.rs` tests
- [ ] Add tests for new `--gitignore` flag
- [ ] Add tests for new `--exclude-vcs` flag
- [ ] Run full test suite: `cargo test`

### Documentation
- [ ] Update README.md (default behavior section)
- [ ] Update --help text in cli.rs
- [ ] **CHANGELOG.md** - Comprehensive breaking changes section:
  - [ ] Default behavior changes (gitignore, .git)
  - [ ] Removed flags (--no-gitignore, --include-vcs, -b short flag)
  - [ ] New flags (--gitignore, --exclude-vcs, -z, -u, --update, --ignore-existing)
  - [ ] Migration guide with before/after examples
  - [ ] Intentional differences from rsync (and why)

### Release
- [ ] Bump version to 0.1.0 in Cargo.toml
- [ ] Final `cargo test && cargo clippy`
- [ ] Tag and release
