# sy v0.0.48 - Remote→Remote Sync + .gitignore Fixes

**Release Date**: 2025-10-27
**Type**: Feature + Bug Fix Release

## Summary

v0.0.48 fixes two critical issues discovered during comprehensive testing of v0.0.47:

1. **Remote→Remote Bidirectional Sync** - Now supported! Sync between two SSH hosts.
2. **.gitignore Support** - Now works outside git repositories.

Both issues were discovered through comprehensive testing (23 scenarios, ~2 hours of testing across Mac and Fedora machines).

---

## 🚀 New Features

### Remote→Remote Bidirectional Sync

Sync directly between two SSH hosts without needing a local intermediary:

```bash
# Sync between two remote servers
sy -b user@host1:/data user@host2:/backup

# Example: Production → Staging
sy -b prod@server1:/var/www staging@server2:/var/www
```

**Implementation Details**:
- Dual SSH connection pools (one for source, one for dest)
- Independent SSH configs per host
- Uses existing `DualTransport` infrastructure
- Full bidirectional sync with conflict resolution

**Testing**:
- Initial sync: 3 files, 59 bytes in 472ms
- Bidirectional: changes on both sides propagate correctly
- Works with nested directories

**Before**: Explicitly rejected with error "Remote-to-remote sync not yet supported"
**After**: Fully functional with all bisync features (conflict resolution, state tracking, etc.)

---

## 🐛 Bug Fixes

### .gitignore Support Outside Git Repositories

**Problem**: In v0.0.47, .gitignore patterns were ignored during bisync, syncing unwanted files like `*.tmp`, `*.log`, and `node_modules/`.

**Root Cause**: The `ignore` crate's `git_ignore(true)` setting only works inside git repositories. Non-git directories had no ignore support.

**Fix**: Explicitly add .gitignore file using `WalkBuilder::add_ignore()`, making it work everywhere.

**Before**:
```bash
# In a non-git directory with .gitignore:
# *.tmp
# *.log
# node_modules/

sy -b /source /dest
# Synced: normal.txt, test.tmp, debug.log, node_modules/
# ✗ Ignored files were synced!
```

**After**:
```bash
sy -b /source /dest
# Synced: normal.txt, .gitignore
# Skipped: test.tmp, debug.log, node_modules/
# ✓ Patterns respected correctly
```

**Testing**:
- Added test: `test_scanner_gitignore_without_git_repo`
- Verified: SSH bisync respects patterns (2 files scanned vs 5 before)
- Works with common patterns: `*.tmp`, `*.log`, `node_modules/`, `.DS_Store`

---

## 📊 Testing

**Comprehensive Test Report**: [COMPREHENSIVE_TEST_REPORT.md](COMPREHENSIVE_TEST_REPORT.md)

- **Total scenarios**: 23
- **Pass rate**: 91.3% (21/23) → **100% (23/23)** after fixes
- **Test duration**: ~2 hours across Mac (M3 Max) and Fedora (i9-13900KF)
- **Network**: SSH over Tailscale
- **Unit tests**: 410+ passing

**Previously Failed Tests** (now passing):
1. Test 24: .gitignore patterns - ✓ PASS (2 files synced, 3 ignored)
2. Test 25: Remote→remote sync - ✓ PASS (implemented and working)

---

## 🔄 Migration

**No breaking changes.** Both fixes are fully backward compatible:

- `.gitignore` support is automatic (no flag required)
- Remote→remote syntax follows existing patterns

**Upgrade from v0.0.47**:
```bash
cargo install sy
# or
cargo install --git https://github.com/nijaru/sy --tag v0.0.48
```

All existing bisync state is compatible - no reset needed.

---

## 📝 Technical Details

### Implementation: Remote→Remote Sync

**File**: `src/transport/router.rs`

```rust
// Before (v0.0.47)
(SyncPath::Remote { .. }, SyncPath::Remote { .. }) => {
    Err(SyncError::Io(std::io::Error::other(
        "Remote-to-remote sync not yet supported"
    )))
}

// After (v0.0.48)
(SyncPath::Remote { host: src_host, user: src_user, .. },
 SyncPath::Remote { host: dst_host, user: dst_user, .. }) => {
    let src_config = parse_or_build_config(src_host, src_user)?;
    let dst_config = parse_or_build_config(dst_host, dst_user)?;

    let src_transport = Box::new(SshTransport::with_pool_size(&src_config, pool_size).await?);
    let dst_transport = Box::new(SshTransport::with_pool_size(&dst_config, pool_size).await?);

    Ok(TransportRouter::Dual(DualTransport::new(src_transport, dst_transport)))
}
```

### Implementation: .gitignore Fix

**File**: `src/sync/scanner.rs`

```rust
// Before (v0.0.47)
pub fn scan_streaming(&self) -> Result<StreamingScanner> {
    let mut walker = WalkBuilder::new(&self.root);
    walker
        .git_ignore(true)  // Only works in git repos!
        .git_global(true)
        .git_exclude(true);

    Ok(StreamingScanner { root: self.root.clone(), walker: walker.build() })
}

// After (v0.0.48)
pub fn scan_streaming(&self) -> Result<StreamingScanner> {
    let mut walker = WalkBuilder::new(&self.root);
    walker
        .git_ignore(true)  // Works in git repos
        .git_global(true)
        .git_exclude(true);

    // Also respect .gitignore files even outside git repos
    let gitignore_path = self.root.join(".gitignore");
    if gitignore_path.exists() {
        walker.add_ignore(&gitignore_path);  // Works everywhere!
    }

    Ok(StreamingScanner { root: self.root.clone(), walker: walker.build() })
}
```

---

## 🎯 What's Next

**Remaining Test Gaps** (for v0.0.49+):

**High Priority**:
1. Very large files (100MB-1GB)
2. Massive directory trees (1000+ files)
3. Network interruption recovery
4. Symlink handling (comprehensive)

**Medium Priority**:
5. Sparse files over SSH bisync
6. Hard links
7. Concurrent syncs

**Low Priority**:
8. Extended attributes
9. BSD flags over SSH

See [COMPREHENSIVE_TEST_REPORT.md](COMPREHENSIVE_TEST_REPORT.md) for full gap analysis.

---

## 📦 Installation

```bash
# Via cargo
cargo install sy

# From source
git clone https://github.com/nijaru/sy
cd sy
cargo install --path .

# Specific version
cargo install sy --version 0.0.48
```

**Requirements**:
- Rust 1.70+
- SSH access for remote operations
- Optional: AWS credentials for S3 support

---

## 🙏 Acknowledgments

Thanks to comprehensive testing methodology that uncovered both issues. Testing approach:
- Real multi-machine setup (Mac ↔ Fedora)
- Production-like network (Tailscale VPN)
- Systematic scenario coverage (23 tests)
- Documentation of failures and fixes

**Commits**:
- `6453681` - fix: make .gitignore work outside git repositories
- `6d9474d` - feat: implement remote→remote bidirectional sync
- `f3421a5` - docs: update README with v0.0.48 features

---

**Full Changelog**: [CHANGELOG.md](CHANGELOG.md)
**GitHub Release**: https://github.com/nijaru/sy/releases/tag/v0.0.48
**Documentation**: [README.md](README.md)
