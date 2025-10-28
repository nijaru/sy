# sy v0.0.50 - Network Recovery Activation

**Release Date**: 2025-10-28

## Overview

Version 0.0.50 activates the network recovery infrastructure built in v0.0.49, making automatic retry functional for all SSH/SFTP operations. sy now automatically recovers from network interruptions without user intervention.

## What's New

### Automatic Network Recovery for All SSH Operations

All 14 SSH/SFTP operations now use automatic retry with exponential backoff:

**Command Operations:**
- `scan` - Remote directory scanning
- `exists` - Path existence checks
- `create_dir_all` - Directory creation
- `remove` - File/directory deletion
- `create_hardlink` - Hard link creation
- `create_symlink` - Symbolic link creation

**SFTP Operations:**
- `read_file` - File reading
- `write_file` - File writing
- `get_mtime` - Modification time queries
- `file_info` - File metadata queries
- `copy_file_streaming` - Streaming file transfers

**Transfer Operations:**
- `copy_file` - Full file transfer with compression
- `copy_sparse_file` - Sparse file optimization
- `sync_file_with_delta` - Delta sync

### How It Works

**Automatic Retry with Exponential Backoff:**
- Default: 3 attempts with 1s initial delay
- Retry sequence: 1s → 2s → 4s → 8s (capped at 30s max)
- Configurable via `--retry` and `--retry-delay` flags

**Intelligent Error Classification:**
- Retries only on: NetworkTimeout, NetworkDisconnected, NetworkRetryable
- Fails immediately on: PermissionDenied, SourceNotFound, NetworkFatal

**Zero Overhead:**
- No performance impact when operations succeed
- Reactive recovery only activates on actual failures

## Technical Details

### Implementation

**Pattern Conversion:**
```rust
// Before (v0.0.49)
tokio::task::spawn_blocking(move || {
    Self::execute_command(session, &cmd)
}).await??

// After (v0.0.50)
retry_with_backoff(&self.retry_config, || {
    async move {
        tokio::task::spawn_blocking(move || {
            Self::execute_command(session, &cmd)
        }).await?
    }
}).await?
```

**Commits:**
- `cc9f9aa` - Activated retry for basic operations
- `ff3372b` - Activated retry for file operations
- `b16399a` - Activated retry for SFTP and transfer operations

### Testing

- All 957 tests passing (444 library + integration tests)
- Zero regression - all existing functionality preserved
- Build passing with 0 warnings

## Usage

Network recovery works automatically with existing flags:

```bash
# Default retry (3 attempts, 1s initial delay)
sy /local user@host:/remote

# Custom retry configuration
sy /local user@host:/remote --retry 5 --retry-delay 2
# Retry sequence: 2s → 4s → 8s → 16s → 30s (capped)

# Disable retry (fail immediately)
sy /local user@host:/remote --retry 0
```

## Breaking Changes

None. All changes are backward compatible.

## Migration

No migration needed. Retry behavior is enabled by default and transparent to users.

## Future Enhancements

**Deferred to v0.0.51+:**
- Resume state integration for chunked file transfer recovery
- Connection pool health checks (reactive retry via v0.0.50 is sufficient)

## Comparison to v0.0.49

| Feature | v0.0.49 | v0.0.50 |
|---------|---------|---------|
| Retry infrastructure | ✅ Built | ✅ Built |
| Error classification | ✅ Available | ✅ Available |
| Resume state tracking | ✅ Available | ✅ Available |
| **Automatic retry in operations** | ❌ Not used | ✅ **Activated** |
| Network failure recovery | ❌ Manual | ✅ **Automatic** |

## Download

```bash
# Install from crates.io
cargo install sy

# Or from source
git clone https://github.com/nijaru/sy.git
cd sy
git checkout v0.0.50
cargo install --path .
```

## Full Changelog

See [CHANGELOG.md](CHANGELOG.md) for complete details.

---

**Questions or Issues?** Please report them at: https://github.com/nijaru/sy/issues
