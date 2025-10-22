# Troubleshooting Guide

Common issues and solutions for sy.

## Table of Contents

- [Installation Issues](#installation-issues)
- [Sync Failures](#sync-failures)
- [Performance Issues](#performance-issues)
- [Permission Errors](#permission-errors)
- [SSH/Remote Sync Issues](#sshremote-sync-issues)
- [Feature-Specific Issues](#feature-specific-issues)
- [Getting Help](#getting-help)

## Installation Issues

### "Command not found: sy"

**Problem**: sy installed but command not available

**Solutions**:
```bash
# Check if cargo bin is in PATH
echo $PATH | grep .cargo/bin

# If not, add to shell config:
# For bash (~/.bashrc) or zsh (~/.zshrc):
export PATH="$HOME/.cargo/bin:$PATH"

# For fish (~/.config/fish/config.fish):
set -gx PATH $HOME/.cargo/bin $PATH

# Reload shell or:
source ~/.bashrc  # or ~/.zshrc or ~/.config/fish/config.fish
```

### Compilation Errors

**Problem**: Build fails during `cargo install sy`

**Common causes**:
- Outdated Rust toolchain
- Missing system dependencies

**Solutions**:
```bash
# Update Rust
rustup update

# Check Rust version (needs 1.70+)
rustc --version

# Clean and retry
cargo clean
cargo install sy
```

## Sync Failures

### "Permission denied (os error 13)"

**Problem**: Cannot write to destination

**Check**:
1. Destination directory exists and is writable
2. File permissions on parent directory
3. Disk space available

**Solutions**:
```bash
# Check permissions
ls -la /path/to/destination

# Check disk space
df -h /path/to/destination

# Create destination if needed
mkdir -p /path/to/destination

# Fix permissions
chmod u+w /path/to/destination
```

### "No space left on device (os error 28)"

**Problem**: Destination filesystem full

**Solutions**:
```bash
# Check disk space
df -h /path/to/destination

# Use --dry-run to see space requirements first
sy /source /dest --dry-run --diff

# Free up space or use different destination
```

### Multiple Errors During Sync

**Problem**: Sync completes but reports errors at end

**What sy does** (v0.0.34+):
- Collects all errors during parallel execution
- Shows comprehensive error report at end
- You see ALL problems at once

**Example output**:
```
⚠️  Errors occurred during sync:

  1. [update] /path/to/file.txt
     Permission denied (os error 13)

  2. [create] /path/to/other.txt
     Disk quota exceeded

Total errors: 2
```

**Solutions**:
1. Review each error in the report
2. Fix underlying issues (permissions, disk space, etc.)
3. Re-run sync to complete remaining files

## Performance Issues

### Slow Sync Performance

**Problem**: Sync slower than expected

**Diagnostic** (v0.0.33+):
```bash
# Use --perf flag to see bottlenecks
sy /source /dest --perf

# Example output shows where time is spent:
# Scanning:      0.05s (62%)  ← Bottleneck is scanning
# Planning:      0.01s (5%)
# Transferring:  0.03s (33%)
```

**Solutions based on bottleneck**:

**If scanning is slow** (>50% of time):
- Use `--ignore-template` to skip large directories (node_modules, etc.)
- Check if `.gitignore` is overly complex
- Consider using `--use-cache` for incremental syncs

**If transferring is slow**:
- Increase parallelism: `sy /source /dest -j 20`
- For local sync, ensure both are on fast drives (not over network mounts)
- For remote sync, check network bandwidth

**If planning is slow**:
- Usually not a bottleneck unless millions of files
- May indicate slow filesystem (network mount, etc.)

### Transfer Speed Lower Than Expected

**Check**:
1. Network bandwidth (for remote syncs)
2. Disk I/O speed (for local syncs)
3. Compression overhead

**Solutions**:
```bash
# Check actual speeds with --perf
sy /source user@host:/dest --perf

# For slow networks, bandwidth limiting is working as designed
sy /source user@host:/dest --bwlimit 1MB --perf

# For local syncs, check disk speed
dd if=/dev/zero of=/tmp/test.img bs=1M count=1024
rm /tmp/test.img
```

### Wasted Bandwidth on Unchanged Files

**Problem**: Files are being re-transferred even though content hasn't changed

**This happens when**:
- Files have been "touched" (mtime updated but content unchanged)
- Build systems update timestamps
- Version control operations change mtimes

**Solution** (v0.0.35+):
```bash
# Use --checksum flag to compare content before transfer
sy /source /dest --checksum

# Short form
sy /source /dest -c

# Preview what would be skipped
sy /source /dest -c --dry-run --diff

# Combine with performance monitoring
sy /source /dest -c --perf
```

**How it works**:
- Computes xxHash3 checksums for both source and dest (15 GB/s, ~5% overhead)
- Skips transfer if checksums match, even if mtime differs
- Transfers only if checksums actually differ
- Detects bit rot (content changed but mtime unchanged)

**Example output**:
```
# Files with matching checksums are skipped:
✓ file1.txt (checksum match, skipped)
→ file2.txt (checksum differs, transferring)
```

**When to use**:
- Re-syncing after build operations
- Syncing after git checkout
- Periodic backups where files may be touched but not modified
- Detecting bit rot or silent corruption

## Permission Errors

### "Operation not permitted" for Extended Attributes

**Problem**: Cannot copy xattrs (macOS Finder info, security contexts, etc.)

**Note**: Extended attributes require special permissions

**Solutions**:
```bash
# Don't preserve xattrs (removes -X flag)
sy /source /dest

# Or run with appropriate permissions
# On macOS for full system backup:
sudo sy /source /dest -X
```

### "Operation not permitted" for ACLs

**Problem**: Cannot copy POSIX ACLs

**Solutions**:
```bash
# Don't preserve ACLs (removes -A flag)
sy /source /dest

# Or run with appropriate permissions
sudo sy /source /dest -A
```

## SSH/Remote Sync Issues

### "SSH connection failed"

**Problem**: Cannot connect to remote host

**Check**:
```bash
# Test SSH connection manually
ssh user@host

# Check SSH config
cat ~/.ssh/config

# Verify host key
ssh-keyscan -t rsa host
```

**Solutions**:
- Ensure SSH key authentication is set up
- Check firewall settings
- Verify hostname/IP is correct
- Try with verbose SSH: `ssh -v user@host`

### "SFTP subsystem not available"

**Problem**: Remote server doesn't support SFTP

**Solutions**:
```bash
# Check if SFTP is available
ssh user@host "which sftp-server"

# If not, install on remote:
# Debian/Ubuntu:
sudo apt install openssh-sftp-server

# RHEL/CentOS:
sudo yum install openssh-sftp-server
```

### Slow Remote Syncs

**Problem**: Remote sync much slower than expected

**Solutions**:
```bash
# Use SSH ControlMaster for connection reuse (2.5x faster)
# Add to ~/.ssh/config:
Host *
  ControlMaster auto
  ControlPath ~/.ssh/control-%h-%p-%r
  ControlPersist 600

# Enable compression for slow networks
sy /source user@host:/dest --compress

# Use parallel transfers
sy /source user@host:/dest -j 10
```

## Feature-Specific Issues

### Sparse Files Not Preserved

**Problem**: Sparse file becomes fully allocated after sync

**Note**: Sparse file support is filesystem-dependent

**Check**:
```bash
# Before sync
ls -lsh /source/sparse-file.img
# 100M -rw-r--r-- 1 user group 10G sparse-file.img
#     ↑ allocated size

# After sync - if not sparse:
ls -lsh /dest/sparse-file.img
# 10G -rw-r--r-- 1 user group 10G sparse-file.img
#    ↑ fully allocated
```

**Explanation** (from sy v0.0.34):
- sy attempts to preserve sparseness using SEEK_HOLE/SEEK_DATA
- Falls back to block-based zero detection
- Some filesystems don't support sparse files (ext4 on older kernels, some network mounts)
- Content is always correct; sparseness is best-effort optimization

**Filesystems with good sparse support**:
- ext4 (modern Linux)
- XFS
- BTRFS
- APFS (macOS)
- NTFS (Windows)

### Delta Sync Not Activating

**Problem**: Expected delta sync but full file transferred

**Check**:
```bash
# Use --perf to see if delta was used
sy /source /dest --perf

# Look for delta sync messages in output
```

**sy delta sync activates when**:
- File exists at destination
- File size differs between source and destination
- File size > 1MB (configurable threshold)

**Doesn't activate when**:
- Creating new file (no destination to delta against)
- File sizes are identical (no need for delta)
- File is too small (<1MB default threshold)

### Verification Failed

**Problem**: xxHash3 or BLAKE3 checksums don't match

**This is SERIOUS** - indicates data corruption

**Steps**:
1. Do NOT ignore this error
2. Check source file integrity
3. Check destination file integrity
4. Re-run sync with `--verify` (BLAKE3 cryptographic verification)
5. If problem persists:
   - Check disk health (SMART status)
   - Check RAM (memtest)
   - Check network (for remote syncs)

```bash
# Verify with cryptographic checksums
sy /source /dest --verify

# Paranoid mode (verifies every block written)
sy /source /dest --mode paranoid
```

## Getting Help

### Enable Debug Logging

```bash
# Set RUST_LOG environment variable
RUST_LOG=debug sy /source /dest

# More verbose
RUST_LOG=trace sy /source /dest

# Log to file
RUST_LOG=debug sy /source /dest 2> sy-debug.log
```

### Performance Profiling

```bash
# Get detailed performance breakdown
sy /source /dest --perf

# Combine with dry-run to test without changes
sy /source /dest --dry-run --diff --perf
```

### Collect Information for Bug Reports

When reporting issues, include:

1. **sy version**:
   ```bash
   sy --version
   ```

2. **System information**:
   ```bash
   uname -a  # OS/kernel version
   rustc --version  # Rust compiler version
   ```

3. **Error output** with debug logging:
   ```bash
   RUST_LOG=debug sy /source /dest 2> error.log
   ```

4. **Performance metrics** (if relevant):
   ```bash
   sy /source /dest --perf
   ```

5. **File/directory structure** (if relevant):
   ```bash
   tree -L 3 /source  # Or ls -R
   ```

### Where to Get Help

- **GitHub Issues**: https://github.com/nijaru/sy/issues
- **Discussions**: https://github.com/nijaru/sy/discussions
- **Documentation**: See [DESIGN.md](../DESIGN.md) for technical details

### Common "Not a Bug" Scenarios

**"Sync shows warnings but completes"**
- This is expected - sync continues for successful files
- v0.0.34+ shows comprehensive error report at end
- You can fix errors and re-run for remaining files

**"Sparseness not preserved on some filesystems"**
- Expected - filesystem-dependent optimization
- Content is always correct, sparseness is best-effort
- See "Sparse Files Not Preserved" section above

**"Slower than rsync in some cases"**
- sy optimizes for different scenarios than rsync
- Use `--perf` to identify bottlenecks
- Some rsync optimizations not yet implemented (see roadmap)

---

**Last Updated**: v0.0.35 (2025-10-21)
