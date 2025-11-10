# sy Codebase Optimization Review

**Date**: 2025-10-31
**Version**: v0.0.55
**Goal**: Identify non-optimal implementation choices and recommend improvements

## Current State Analysis

### Shell Command Usage in SSH Transport

**Current Approach**: Using shell commands via SSH for file operations
**Files**: `src/transport/ssh.rs`

| Operation | Current Method | Lines | Shell-Agnostic? |
|-----------|---------------|-------|-----------------|
| Create directory | `mkdir -p` | 651, 1270, 1321 | ✅ Yes (POSIX) |
| Remove file/dir | `rm -rf`, `rm -f` | 1252-1254 | ✅ Yes (POSIX) |
| Set permissions | `chmod` | TBD | ✅ Yes (POSIX) |
| Set ownership | `chown` | TBD | ✅ Yes (POSIX) |
| Set ACLs | `setfacl` | 2017-2019 | ⚠️ Linux-specific |
| Set xattrs | `setfattr`/`xattr` | 1996 | ⚠️ Platform-specific |
| Disk space | ~~`df` (FIXED)~~ | ~~1847~~ | ✅ Now uses SFTP statvfs |

### Improvement Opportunities

#### 1. Replace Shell Commands with Native SFTP Operations

**Priority: Medium**
**Benefit**: More reliable, type-safe, shell-agnostic

##### mkdir -p (Recursive directory creation)
```rust
// Current: shell command
execute_command("mkdir -p '/path/to/dir'")

// Proposed: Native SFTP
async fn create_dir_all_sftp(sftp: &Sftp, path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        // Ignore error if dir already exists
        let _ = sftp.mkdir(&current, 0o755);
    }
    Ok(())
}
```

**Pros**:
- No shell parsing
- Works with any remote shell
- Type-safe error handling
- Can set permissions atomically

**Cons**:
- More code (but reusable)
- Need to handle existing directories gracefully

##### rm -rf (Recursive removal)
```rust
// Current: shell command
execute_command("rm -rf '/path/to/dir'")

// Proposed: Native SFTP
async fn remove_dir_all_sftp(sftp: &Sftp, path: &Path) -> Result<()> {
    // Use readdir to list contents
    // Recursively delete files/subdirs
    // Finally rmdir the directory
}
```

**Pros**:
- Shell-agnostic
- Better error reporting (know which file failed)
- Type-safe

**Cons**:
- More complex implementation
- Slower than single shell command (multiple round trips)
- rm -rf is well-tested and reliable

**Recommendation**: KEEP shell command for now. rm -rf is POSIX standard and works everywhere. Native SFTP would be slower and more complex.

#### 2. chmod/chown Operations

**Current**: Likely using shell commands (TBD - need to verify)

SFTP supports these via `File::setstat()` and `FileStat`:
```rust
let mut stat = FileStat {
    size: None,
    uid: Some(1000),
    gid: Some(1000),
    perm: Some(0o755),
    atime: None,
    mtime: None,
};
sftp.setstat(path, stat)?;
```

**Recommendation**: Replace with native SFTP if we're using shell commands.

#### 3. ACL Operations (setfacl)

**Current**: Shell command `setfacl -M -` (line 2017)
**Issue**: Linux-specific, not portable to BSD/macOS

**Options**:
a) Use libacl via FFI (C bindings)
b) Keep shell command with platform detection
c) Use exacl crate (already in dependencies!)

**Recommendation**: Use `exacl` crate (line 84) - it's already a dependency!

```rust
use exacl::{setfacl, AclEntry};
// Platform-agnostic ACL handling
```

#### 4. Extended Attributes (xattr)

**Current**: Shell command `setfattr`/`xattr` (line 1996)
**Already have**: `xattr` crate (line 83)

**Recommendation**: Replace shell commands with xattr crate calls!

```rust
// Current (shell):
execute_command("setfattr -n 'user.key' -v 'value' '/path'")

// Better (using xattr crate):
use xattr;
xattr::set("/path", "user.key", b"value")?;
```

This is already available via SFTP extended operations:
```rust
// For remote systems via SFTP
sftp.open(path)?.setstat(stat)?;  // For basic attrs
// For extended attrs, may need custom SFTP extension
```

## Library Assessment

### Current Dependencies - Are They Best-in-Class?

| Category | Current | Alternatives | Recommendation |
|----------|---------|--------------|----------------|
| SSH/SFTP | ssh2 v0.9 | russh, thrussh | ✅ Keep ssh2 (stable, well-tested) |
| Async runtime | tokio | async-std, smol | ✅ Keep tokio (best ecosystem) |
| Hashing | xxhash3, blake3 | - | ✅ Best choices |
| Compression | zstd, lz4_flex | - | ✅ Optimal |
| CLI parsing | clap v4 | - | ✅ Industry standard |
| Logging | tracing | log, env_logger | ✅ Modern choice |
| Progress | indicatif | - | ✅ Best available |
| File walking | walkdir | jwalk | ⚠️ Consider jwalk |
| Ignore patterns | ignore | - | ✅ Best (from ripgrep) |

### Potential Upgrades

#### walkdir → jwalk
**Current**: `walkdir` (sequential)
**Alternative**: `jwalk` (parallel)

```toml
# Current
walkdir = "2"

# Proposed
jwalk = "0.8"  # Parallel directory walking
```

**Benefits**:
- Parallel directory traversal (faster for large dirs)
- Same API as walkdir (easy migration)
- Built on rayon (we already use it)

**Test Impact**: Measure on large directory before switching

**Recommendation**: Benchmark first, then migrate if >20% improvement

## Non-Optimal Patterns Found

### 1. Scanner Performance

**File**: `src/sync/scanner.rs`

Current scanner uses walkdir (sequential). For large directories, parallel scanning would be faster.

**Test Required**: Benchmark jwalk vs walkdir on >100k files

### 2. ACL Handling in SSH Transport

**File**: `src/transport/ssh.rs:2017-2019`

Uses shell command instead of `exacl` crate that's already in dependencies.

**Action**: Refactor to use exacl library

### 3. XAttr Handling in SSH Transport

**File**: `src/transport/ssh.rs:1996`

Uses shell command instead of proper library.

**Action**:
- For local: Use `xattr` crate (already in deps)
- For remote: Investigate SFTP extended attributes support

### 4. Error Context

Some error messages could be more helpful:
```rust
// Less helpful
.map_err(|e| SyncError::Io(e))?

// More helpful
.map_err(|e| SyncError::Io(std::io::Error::new(
    e.kind(),
    format!("Failed to open {} for reading: {}", path.display(), e)
)))?
```

**Action**: Audit error messages for clarity

## Summary of Recommended Changes

### High Priority (Do Now)
1. ✅ DONE: Replace df shell command with SFTP statvfs
2. ✅ ALREADY DONE: ACL handling uses exacl crate for local operations
3. ✅ ALREADY DONE: xattr handling uses xattr crate for local operations

**Note**: ACL and xattr operations via SSH must use shell commands because
SFTP protocol doesn't support extended attributes or ACLs natively. This is
the correct implementation.

### Medium Priority (Next Release)
4. Benchmark jwalk vs walkdir for large directory scanning
5. Consider chmod/chown via SFTP setstat instead of shell (if currently using shell)
6. Audit and improve error messages throughout codebase

### Low Priority (Future)
7. Consider implementing recursive SFTP mkdir/rmdir (but rm -rf is fine for now)
8. Investigate async xattr/ACL operations for better performance

## Decision Log

### Why Not Replace Everything with Native SFTP?

**Shell commands we should keep**:
- `rm -rf`: Well-tested, single round trip, POSIX standard
- `mkdir -p`: Could go either way, but shell is simpler

**Shell commands we should replace**:
- `setfacl`: Have library, more portable
- `setfattr`/`xattr`: Have library, type-safe
- `df`: ✅ Already replaced with statvfs

### Why ssh2 over russh?

ssh2 is based on libssh2 (C library), well-tested, mature ecosystem. russh is pure Rust but less battle-tested for production use. Stick with stability.

### Why tokio over alternatives?

Best ecosystem, most crates support it, excellent documentation. No reason to change.

## Next Steps

1. Implement ACL handling with exacl crate
2. Implement xattr handling with xattr crate / SFTP
3. Benchmark jwalk vs walkdir
4. Test all changes thoroughly
5. Release v0.0.56
