# Filesystem Support in sy

This document details how `sy` behaves on different filesystems and operating systems, particularly for delta sync operations.

## Quick Reference

| Filesystem | OS | COW Support | Delta Sync Strategy | Performance |
|------------|-----|-------------|---------------------|-------------|
| APFS | macOS | ✅ Yes | COW (clone + selective writes) | **Excellent** |
| BTRFS | Linux | ✅ Yes | COW (clone + selective writes) | **Excellent** |
| XFS | Linux | ✅ Yes | COW (clone + selective writes) | **Excellent** |
| ext4 | Linux | ❌ No | In-place (full rebuild) | **Good** |
| NTFS | Windows | ❌ No* | In-place (full rebuild) | **Good** |
| HFS+ | macOS | ❌ No | In-place (full rebuild) | **Good** |
| FAT32 | All | ❌ No | In-place (full rebuild) | **Fair** |
| exFAT | All | ❌ No | In-place (full rebuild) | **Fair** |

*ReFS on Windows supports COW but is rare and not yet detected by sy.

## What is COW (Copy-on-Write)?

Copy-on-Write is a filesystem feature that allows instant file cloning by sharing data blocks between files until they are modified. When a block is modified, only then is it copied.

**Benefits for delta sync**:
- Instant file cloning (~1ms for 100MB file)
- Only changed blocks consume disk space
- Significantly faster for small changes in large files

**Example**: Updating 1MB in a 100MB file
- **COW strategy**: Clone (1ms) + write 1MB = ~60ms total
- **In-place strategy**: Write entire 100MB = ~200ms total
- **Speedup**: 3.3x faster with COW

## Delta Sync Strategies

### COW Strategy (APFS/BTRFS/XFS)

**How it works**:
1. Clone destination file using COW reflink (instant)
2. Compare source and destination blocks
3. Only overwrite blocks that changed
4. Rename clone to destination (atomic)

**Performance characteristics**:
- Clone operation: ~1ms for any file size
- Write operations: Only changed blocks
- Disk usage: Only changed blocks consume space
- **Best for**: Small changes in large files

**System calls**:
- macOS: `clonefile()` syscall
- Linux: `ioctl(FICLONE)` syscall

**Code path**: `src/transport/local.rs` lines 260-370

### In-place Strategy (ext4/NTFS/others)

**How it works**:
1. Create empty temporary file
2. Allocate full file size
3. Compare source and destination blocks
4. Write ALL blocks to temporary file (changed + unchanged)
5. Rename temp to destination (atomic)

**Performance characteristics**:
- Create/allocate: Minimal overhead
- Write operations: Full file size
- Disk usage: Full file size required
- **Best for**: Filesystems without COW support

**Why not use fs::copy()?**
On non-COW filesystems, `fs::copy()` is much slower than block-by-block writing. Benchmarks show 2x slower on ext4.

**Code path**: `src/transport/local.rs` lines 376-470

## Strategy Selection

### Automatic Selection

`sy` automatically chooses the optimal strategy based on:

1. **Filesystem COW support** (detected via `statfs`)
2. **Same filesystem** (source and dest on same device)
3. **Hard link status** (nlink > 1)

### Selection Logic

```rust
let use_cow_strategy =
    supports_cow_reflinks(&dest) &&     // Filesystem supports COW
    same_filesystem(&source, &dest) &&  // Same device
    !has_hard_links(&dest);             // No hard links
```

**Priority**:
1. Hard links detected → Always use in-place (preserves hard link integrity)
2. Different filesystems → Always use in-place (COW doesn't work cross-filesystem)
3. No COW support → Use in-place (avoids slow fs::copy)
4. COW supported → Use COW strategy

### Observing Strategy Selection

Enable info-level logging to see which strategy is selected:

```bash
RUST_LOG=sy=info sy source.dat dest.dat
```

**COW strategy output**:
```
INFO Delta sync strategy: COW (clone + selective writes) - filesystem supports COW reflinks
```

**In-place strategy output**:
```
INFO Delta sync strategy: in-place (full file rebuild) - filesystem does not support COW reflinks
INFO Delta sync strategy: in-place (full file rebuild) - source and dest on different filesystems
INFO Delta sync strategy: in-place (full file rebuild) - destination has hard links (preserving link integrity)
```

## Filesystem Detection

### macOS

**Detection method**: `statfs` → `f_fstypename` field

**APFS detection**:
```rust
let fs_type = statfs.f_fstypename; // "apfs"
supports_cow = (fs_type == "apfs");
```

**Other filesystems**:
- HFS+: `"hfs"` - No COW support
- FAT32: `"msdos"` - No COW support
- exFAT: `"exfat"` - No COW support

**Code**: `src/fs_util.rs` lines 42-93

### Linux

**Detection method**: `statfs` → `f_type` (magic number)

**Filesystem magic numbers**:
```rust
const BTRFS_SUPER_MAGIC: i64 = 0x9123683E;
const XFS_SUPER_MAGIC: i64 = 0x58465342;

supports_cow = matches!(stat.f_type,
    BTRFS_SUPER_MAGIC | XFS_SUPER_MAGIC);
```

**Other filesystems**:
- ext4: `0xEF53` - No COW support
- ext3: `0xEF53` - No COW support
- tmpfs: `0x01021994` - No COW support (RAM-backed)

**Code**: `src/fs_util.rs` lines 95-118

### Windows

**Current status**: Not yet implemented

**Planned detection**: Check for ReFS filesystem
- ReFS supports `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (similar to COW)
- NTFS does not support COW
- Default to in-place strategy until detection implemented

**Code**: `src/fs_util.rs` lines 120-125

## Performance Comparison

### Benchmark Results (macOS M3 Max, APFS)

**Full file copy (100MB)**:
- `fs::copy()` with COW: **39ms** ✅
- Manual read/write loop: **73ms** (1.87x slower)
- rsync: **320ms** (8.2x slower)

**Delta sync (1MB change in 100MB)**:
- COW strategy: **58ms** ✅
- In-place strategy: ~**92ms** (1.6x slower)
- rsync: **330ms** (5.7x slower)

**Delta sync (identical 100MB files)**:
- COW strategy: **36ms** ✅ (no writes needed)
- rsync: **320ms** (8.9x slower)

### Linux Performance (ext4 vs BTRFS)

**ext4 (no COW)**:
- In-place strategy: **~200ms** for 100MB file ✅
- fs::copy() fallback: **~400ms** (2x slower)
- rsync: **~350ms**

**BTRFS (with COW)**:
- COW strategy: **~50ms** for 1MB change ✅
- In-place strategy: **~180ms** (3.6x slower)
- rsync: **~320ms**

**Conclusion**: Strategy selection prevents regressions on any filesystem.

## Cross-Filesystem Operations

### Behavior

**Scenario**: Source on ext4, destination on BTRFS
- **Detection**: `same_filesystem()` returns false (different device IDs)
- **Strategy**: In-place (COW reflinks don't work across filesystems)
- **Performance**: Same as single-filesystem in-place

**Why it matters**:
- Attempting COW reflink across filesystems fails with `EXDEV` error
- Automatic detection prevents this error
- Falls back to working in-place strategy

### Testing Cross-Filesystem

See `tests/delta_sync_test.rs::test_cross_filesystem_uses_inplace_strategy` for manual testing instructions.

**Example setup** (macOS):
```bash
# Create ramdisk
hdiutil attach -nomount ram://204800  # 100MB
diskutil erasevolume APFS "TestFS" /dev/disk4

# Set env variable
export CROSS_FS_PATH=/Volumes/TestFS

# Run test
cargo test test_cross_filesystem_uses_inplace_strategy -- --ignored --nocapture

# Cleanup
hdiutil detach /dev/disk4
```

## Hard Link Handling

### Why Hard Links Matter

**Problem**: COW clone creates a new inode, breaking hard link relationship

**Example**:
```bash
# file1 and file2 are hard linked (share inode)
$ stat file1.txt | grep Inode
Inode: 12345  Links: 2

# After COW clone-based delta sync
$ stat file1.txt | grep Inode
Inode: 67890  Links: 1  # NEW INODE - hard link broken!
```

**Solution**: Detect hard links and use in-place strategy

### Detection

**Method**: Check `nlink` field in file metadata
```rust
let metadata = fs::metadata(&dest)?;
let has_hardlinks = metadata.nlink() > 1;
```

**Strategy selection**:
- `nlink == 1`: Normal file → COW strategy OK
- `nlink > 1`: Hard linked → Force in-place strategy

**Code**: `src/fs_util.rs` lines 210-223

### Preservation

**Flag**: `--preserve-hardlinks`
- Enables hard link tracking across sync operations
- Ensures hard-linked files remain hard-linked at destination
- Uses inode tracking to preserve relationships

**Without flag**: Hard links are copied as independent files

**Code**: `src/sync/hardlinks.rs` (hardlink tracking module)

## Delta Sync Threshold

### Size Threshold

**Current**: 10MB minimum for delta sync
- Files smaller than 10MB: Full copy (faster)
- Files 10MB or larger: Delta sync (beneficial)

**Rationale**:
- Delta sync has overhead (block comparison, strategy selection)
- For small files, full copy is faster
- Benchmarks show crossover point around 10MB

**Tuning**: May adjust based on future benchmarks

**Code**: `src/transport/local.rs` line 195

### Change Ratio (Future)

**Planned**: Skip delta sync if >75% of blocks changed
- Detection: Compare blocks, count changes
- Fallback: Use full copy if too many changes
- **Status**: Not yet implemented (v0.1.0)

## Error Handling

### COW Strategy Failures

**Common errors**:
1. **Cross-filesystem**: `EXDEV` error when attempting reflink
2. **No space**: `ENOSPC` error when cloning
3. **Permissions**: `EACCES` error

**Error message example**:
```
Delta sync failed for /path/to/file.sy.tmp
Strategy: COW (clone + selective writes)
Cause: No space left on device
COW file cloning failed. This may happen if:
  - Filesystem doesn't support reflinks (needs APFS, BTRFS, or XFS)
  - Cross-filesystem operation detected
  - Insufficient disk space
  Falling back to in-place strategy may help.
```

**Recovery**: Automatic detection prevents most errors; manual fallback to `--no-delta` if needed

**Code**: `src/error.rs` lines 35-41

### In-place Strategy Failures

**Common errors**:
1. **No space**: `ENOSPC` when allocating temp file
2. **Write errors**: I/O errors during block writes
3. **Permissions**: `EACCES` on temp file creation

**Error message example**:
```
Delta sync failed for /path/to/file.sy.tmp
Strategy: in-place (full file rebuild)
Cause: No space left on device
Failed to allocate 100.00 MB for temporary file.
  Check available disk space on destination.
```

## Best Practices

### For Maximum Performance

1. **Use COW filesystems when possible**:
   - macOS: APFS (default on modern macOS)
   - Linux: BTRFS or XFS
   - Avoid: ext4 for large file syncs

2. **Keep source and destination on same filesystem**:
   - Enables COW optimization
   - Faster than cross-filesystem sync

3. **Avoid hard links unless necessary**:
   - Hard links force in-place strategy
   - Use `--preserve-hardlinks` only when needed

4. **Use SSD storage**:
   - Delta sync benefits from fast random I/O
   - COW filesystems work better on SSDs

### For Maximum Compatibility

1. **Use ext4 on Linux** (most common):
   - In-place strategy works well
   - No COW benefits but no penalties

2. **Test on target filesystem**:
   - Check logs to verify expected strategy
   - Benchmark critical use cases

3. **Use `--dry-run` first**:
   - Preview changes before syncing
   - Verify strategy selection

## Troubleshooting

### "Delta sync slower than expected"

**Possible causes**:
1. Non-COW filesystem (ext4, HFS+, NTFS)
   - Expected: In-place strategy is slower than COW
   - Solution: Migrate to APFS/BTRFS/XFS if possible

2. Hard links detected
   - Check: `ls -li` to see link counts
   - Solution: Remove hard links or accept in-place performance

3. Cross-filesystem sync
   - Check: `df` to verify mount points
   - Solution: Move source/dest to same filesystem

### "COW strategy not being used"

**Check filesystem**:
```bash
# macOS
mount | grep apfs

# Linux
df -T | grep -E 'btrfs|xfs'
```

**Check logs**:
```bash
RUST_LOG=sy=info sy source dest 2>&1 | grep strategy
```

**Common reasons**:
- Filesystem is not APFS/BTRFS/XFS
- Cross-filesystem operation
- Hard links present on destination
- File size below 10MB threshold

### "Errors during delta sync"

**COW clone failed**:
- Verify filesystem supports COW
- Check available disk space
- Try `--no-delta` to force full copy

**In-place strategy failed**:
- Check available disk space (needs full file size)
- Verify write permissions on destination
- Check for read-only mounts

## Platform-Specific Notes

### macOS

**APFS advantages**:
- Default on macOS 10.13+
- Excellent COW support
- Fast metadata operations

**Migration from HFS+**:
- Use Disk Utility to convert
- Backup data first
- Significantly improves sy performance

### Linux

**BTRFS**:
- Best COW support on Linux
- Snapshots and compression available
- Some stability concerns (research your use case)

**XFS**:
- Reliable COW support
- Good performance
- Widely used in enterprise

**ext4**:
- Most common Linux filesystem
- No COW support but fast
- In-place strategy works well

**ZFS**:
- Not yet detected by sy
- Has COW but different API
- Future support possible

### Windows

**Current status**: Limited testing
- NTFS: Works with in-place strategy
- ReFS: COW detection not implemented
- FAT32/exFAT: Works but no optimization

**Future**: ReFS detection and testing planned

## Future Improvements

### Planned Features

1. **ReFS detection** (Windows):
   - Detect ReFS filesystems
   - Use Windows COW APIs
   - Target: v0.2.0

2. **Change ratio detection**:
   - Skip delta if >75% changed
   - Fallback to full copy
   - Target: v0.1.0

3. **Sparse file preservation**:
   - Use `SEEK_HOLE`/`SEEK_DATA`
   - Preserve holes in delta sync
   - Target: v0.1.0

4. **ZFS support** (Linux):
   - Detect ZFS filesystems
   - Use `zfs send/receive` or COW APIs
   - Target: v0.3.0

### Research Areas

- bcachefs support (new Linux COW filesystem)
- F2FS flash-optimized filesystems
- Network filesystems (NFS, CIFS) behavior

## References

### Code

- Filesystem detection: `src/fs_util.rs`
- Delta sync strategies: `src/transport/local.rs`
- Error handling: `src/error.rs`
- Tests: `tests/delta_sync_test.rs`

### Documentation

- Design decisions: `DESIGN.md`
- Performance benchmarks: `docs/PERFORMANCE.md`
- Current status: `docs/STATUS_2025-10-20.md`

### External Resources

- [APFS reference (Apple)](https://developer.apple.com/documentation/foundation/file_system/about_apple_file_system)
- [BTRFS wiki](https://btrfs.wiki.kernel.org/)
- [XFS documentation](https://xfs.wiki.kernel.org/)
- [Copy-on-write (Wikipedia)](https://en.wikipedia.org/wiki/Copy-on-write)

---

**Last Updated**: 2025-10-20
**Version**: v0.0.25
**Maintainer**: Nick Russo <nick@nijaru.dev>
