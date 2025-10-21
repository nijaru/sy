# sy v0.0.23 - Performance & Correctness Release

**Release Date**: 2025-10-20

## Summary

Major performance improvements (1.3x - 8.8x faster than rsync) through filesystem-aware delta sync optimization. Critical correctness fixes for hard link preservation and file truncation.

## Performance Improvements

### Benchmark Results (macOS M3 Max, APFS)

| Scenario | sy | rsync | Speedup |
|----------|-----|-------|---------|
| 1000 small files (1-10KB) | 0.117s | 0.180s | **1.53x** |
| 100 medium files (100KB) | 0.021s | 0.051s | **2.44x** |
| 1 large file (100MB) | 0.036s | 0.320s | **8.82x** |
| Deep tree (200 files) | 0.035s | 0.044s | **1.27x** |
| Delta sync (1MB Δ in 100MB) | 0.058s | 0.330s | **5.70x** |

### Key Optimizations

1. **COW-based file copy** (src/transport/local.rs)
   - Use `fs::copy()` instead of manual read/write loop
   - Platform-specific optimizations:
     - macOS: `clonefile()` for instant COW reflinks on APFS
     - Linux: `copy_file_range()` for zero-copy I/O
   - **Impact**: 47% faster full file copy (73ms → 39ms for 100MB)

2. **Block comparison for local delta sync**
   - Replaced rsync algorithm with simple block-by-block comparison
   - Rationale: Both files available locally, no need for rolling hash
   - **Impact**: 6x faster delta sync before COW optimization

3. **COW-based delta sync**
   - Clone destination file (instant with COW on APFS/BTRFS/XFS)
   - Only write changed blocks to clone
   - **Impact**: 33% faster delta sync (92ms → 61ms for 1MB Δ in 100MB)

4. **Filesystem-aware strategy selection** (NEW: src/fs_util.rs)
   - Automatic detection of COW support (APFS, BTRFS, XFS)
   - Fallback to in-place strategy for ext4/NTFS
   - Prevents performance regression on non-COW filesystems
   - **Impact**: No performance loss on common Linux filesystems

## Correctness Fixes

### Critical Bug Fixes

1. **File truncation in delta sync** (HIGH PRIORITY)
   - **Issue**: When source file is smaller than destination, old data remained at end
   - **Fix**: Added `set_len()` to truncate temp file to source size
   - **Impact**: Prevents data corruption when syncing smaller files over larger ones

2. **Hard link preservation** (HIGH PRIORITY)
   - **Issue**: COW clone would break hard link relationships
   - **Fix**: Detect hard links (nlink > 1) and use in-place strategy
   - **Impact**: Preserves hard link integrity
   - **Requires**: `--preserve-hardlinks` flag

3. **xattr handling**
   - **Issue**: `fs::copy()` preserves xattrs on macOS even when not requested
   - **Fix**: Strip xattrs after copy, let Transferrer re-add selectively
   - **Impact**: Respects `--preserve-xattrs` setting

## New Features

### Filesystem Utilities Module (src/fs_util.rs)

- `supports_cow_reflinks()` - Detects APFS (macOS), BTRFS/XFS (Linux)
- `same_filesystem()` - Checks if paths are on same device
- `has_hard_links()` - Detects files with nlink > 1

### Intelligent Strategy Selection

**COW Strategy** (APFS/BTRFS/XFS):
- Clone file instantly using COW reflinks
- Selectively overwrite changed blocks
- **Use case**: Same filesystem, no hard links, COW supported

**In-place Strategy** (ext4/NTFS):
- Create temp file, write all blocks
- Avoids slow `fs::copy()` on non-COW
- **Use case**: Non-COW filesystem OR hard links OR cross-filesystem

## Testing

### New Tests (tests/delta_sync_test.rs)

- ✅ Delta sync file shrinking (truncation test)
- ✅ Delta sync file growing (expansion test)
- ✅ Delta sync correctness (bit-identical output)
- ✅ Hard link preservation
- ✅ Hard link updates preserve link relationship

**Total**: 290 tests passing (up from 285)

## Documentation

### Updated

- `.claude/CLAUDE.md` - Updated to v0.0.23 with new module docs
- `docs/PERFORMANCE.md` - Latest benchmark results
- `README.md` - Performance numbers (if applicable)

### New

- `docs/EVALUATION_v0.0.23.md` - Comprehensive analysis
- `RELEASE_NOTES_v0.0.23.md` - This file

## Platform Support

**Production Ready**:
- ✅ macOS (APFS) - Full COW optimization
- ✅ Linux (BTRFS/XFS) - Full COW optimization
- ✅ Linux (ext4) - In-place strategy (no regression)

**Needs Testing**:
- ⚠️ Windows (NTFS) - Should work with in-place strategy

## Breaking Changes

None - all changes are backward compatible.

## Known Limitations

- No change ratio heuristics (may be slower on files with >75% changes)
- Temp file cleanup on interruption not yet implemented
- Some debug tracing missing for strategy selection

## Migration Guide

No migration needed - upgrade is seamless.

For users with hard links:
- Add `--preserve-hardlinks` flag to preserve hard link relationships
- Without this flag, hard links will be copied as independent files

## Contributors

- Nick Russo <nick@nijaru.dev>
- Claude (Anthropic) - Performance analysis and optimization

## Next Steps

See `docs/EVALUATION_v0.0.23.md` for future improvements planned for v0.0.24+.

---

**Full Changelog**: https://github.com/nijaru/sy/compare/v0.0.22...v0.0.23
