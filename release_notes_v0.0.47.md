# sy v0.0.47 - CRITICAL FIX: SSH Bidirectional Sync

Release date: 2025-10-27

## 🚨 CRITICAL BUG FIX

**v0.0.46 users: Upgrade immediately!** SSH bidirectional sync does not work in v0.0.46.

### The Issue (v0.0.46)

SSH bidirectional sync silently failed:
- `sy -b /local user@host:/remote` reported "✓ Sync complete"
- Files were never actually written to the remote server
- Root cause: `SshTransport` missing `write_file()` implementation

### The Fix (v0.0.47)

Implemented complete `write_file()` for SSH transport:
- ✅ Files now properly written to remote via SFTP
- ✅ Recursive directory creation
- ✅ mtime preservation
- ✅ Comprehensive error handling

### Testing

All 8 SSH bisync scenarios verified (Mac ↔ Fedora over Tailscale):
1. ✅ Basic bidirectional sync with nested directories
2. ✅ Bidirectional changes without conflicts
3. ✅ Conflict resolution (newer strategy)
4. ✅ Deletion propagation (v0.0.46 state bug also fixed)
5. ✅ State persistence across syncs
6. ✅ Large file transfer (10MB @ 8.27 MB/s)
7. ✅ Dry-run mode
8. ✅ Conflict history logging

## 📦 Installation

### From crates.io
```bash
cargo install sy --version 0.0.47
# or upgrade existing installation
cargo install sy --force
```

### From source
```bash
cargo install sy --git https://github.com/nijaru/sy --tag v0.0.47
```

## 🚀 Usage

SSH bidirectional sync now works correctly:

```bash
# Local ↔ Remote
sy -b /local/docs user@host:/remote/docs

# Remote ↔ Remote
sy -b user@host1:/data user@host2:/backup

# With conflict resolution
sy -b /a user@host:/b --conflict-resolve newer

# With safety limits
sy -b /a user@host:/b --max-delete 10
```

## 🔧 Technical Details

### Implementation

**File**: `src/transport/ssh.rs:1244-1332` (89 lines)

**Method**: `write_file(path, data, mtime)`

**Features**:
- Uses SFTP session from connection pool
- Recursive directory creation with proper permissions
- Atomic file write with flush
- mtime preservation via `setstat()`
- Comprehensive error messages with path context
- Debug tracing for troubleshooting

### What Was Broken

The `Transport` trait provides a default `write_file()` implementation that writes to the **local** filesystem. `SshTransport` didn't override this, so:

```rust
// Bisync called this:
to_transport.write_file(remote_path, data, mtime).await

// But it executed the LOCAL implementation:
tokio::fs::File::create(remote_path).await  // ❌ Writes locally!
```

### The Fix

```rust
async fn write_file(&self, path: &Path, data: &[u8], mtime: SystemTime) -> Result<()> {
    // Now properly writes via SFTP to remote server ✅
    let sftp = session.sftp()?;
    let mut remote_file = sftp.create(path)?;
    remote_file.write_all(data)?;
    sftp.setstat(path, FileStat { mtime, .. })?;
}
```

## 📊 Test Results

- **Unit tests**: 410 passing (0 regressions)
- **SSH bisync tests**: 8/8 passing
- **Build**: 0 warnings, 0 clippy warnings
- **Test platforms**: macOS (M3 Max), Fedora (i9-13900KF)
- **Network**: Tailscale (WireGuard)

## 🎯 Who Should Upgrade

**IMMEDIATELY** if you:
- Use SSH bidirectional sync (`sy -b /local user@host:/remote`)
- Are on v0.0.46

**Note**: Regular unidirectional SSH sync works fine in v0.0.46 (uses different code path).

## 📝 Related Fixes

This release also benefits from the v0.0.46 deletion propagation fix:
- State storage now correctly saves both sides after copy operations
- Deletions propagate properly instead of being copied back
- Deletion safety limits work as intended

## 🙏 Contributors

Testing and development on personal infrastructure.

## 📚 Documentation

- [README.md](README.md) - User guide and examples
- [CHANGELOG.md](CHANGELOG.md) - Complete version history
- [ai/STATUS.md](ai/STATUS.md) - Development status

## 🔗 Links

- **GitHub**: https://github.com/nijaru/sy
- **crates.io**: https://crates.io/crates/sy
- **Docs**: https://docs.rs/sy

**Full commit history**: https://github.com/nijaru/sy/compare/v0.0.46...v0.0.47
