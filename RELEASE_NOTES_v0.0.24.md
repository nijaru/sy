# sy v0.0.24 - Polish & Reliability Release

**Release Date**: 2025-10-20

## Summary

Polish and reliability improvements for delta sync. Better error messages, automatic temp file cleanup, and improved robustness.

## Improvements

### Better Error Messages

**New error type**: `DeltaSyncError` with strategy-specific context and actionable hints.

**Before**:
```
Failed to copy file: /path/to/file.sy.tmp
Cause: No space left on device
Check disk space and write permissions on the destination.
```

**After**:
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

**Benefits**:
- Understand which strategy was being used
- Clear explanation of why it failed
- Actionable suggestions for resolution

### Automatic Temp File Cleanup

**New module**: `src/temp_file.rs` with RAII-based cleanup

**Problem solved**: Temp files (`.sy.tmp`) were left behind when:
- Program panics
- User interrupts with Ctrl+C
- Errors occur during delta sync

**Solution**: `TempFileGuard` automatically deletes temp files on drop

```rust
// Before: Manual cleanup (could leak on error/panic)
let temp_path = dest.with_extension("sy.tmp");
fs::copy(&dest, &temp_path)?;
// ... do work ...
fs::rename(&temp_path, &dest)?;
// If error occurs here, temp_path leaks!

// After: Automatic cleanup via RAII
let temp_path = dest.with_extension("sy.tmp");
let temp_guard = TempFileGuard::new(&temp_path);
fs::copy(&dest, &temp_path)?;
// ... do work ...
fs::rename(&temp_path, &dest)?;
temp_guard.defuse(); // Success - prevent cleanup
// If error/panic occurs, Drop trait automatically cleans up
```

**Benefits**:
- No temp file leaks on error or interrupt
- Cleaner file system state
- More predictable behavior

## Code Quality

### New Tests

- `test_temp_file_guard_cleans_up` - Verifies cleanup on drop
- `test_temp_file_guard_defuse` - Verifies defuse prevents cleanup
- `test_temp_file_guard_nonexistent_file` - Verifies no panic on missing file
- `test_temp_file_guard_path` - Verifies path accessor

**Total tests**: 294 (up from 290)

### Documentation

- Comprehensive inline documentation for `TempFileGuard`
- Usage examples in doc comments
- Clear explanation of RAII pattern

## API Changes

**New public API**:
- `sy::temp_file::TempFileGuard` - RAII temp file cleanup
- `sy::error::format_bytes()` - Format bytes for human-readable display
- `sy::error::SyncError::DeltaSyncError` - Delta sync specific errors

**Backward compatible**: No breaking changes

## Performance

No performance impact - error handling and cleanup are negligible overhead.

## Testing

**All tests passing**: 294 tests ✅

**Verified scenarios**:
- Normal delta sync (temp file cleaned up after rename)
- Error during delta sync (temp file cleaned up on error)
- Panic during delta sync (temp file cleaned up via Drop)
- Missing temp file (no panic during cleanup)

## Platform Support

Same as v0.0.23:
- ✅ macOS (APFS)
- ✅ Linux (BTRFS/XFS/ext4)
- ⚠️ Windows (NTFS) - needs testing

## Known Limitations

Same as v0.0.23:
- No change ratio detection
- Sparse file preservation not guaranteed in delta sync
- Windows NTFS not yet tested

## Migration Guide

No migration needed - upgrade is seamless. All changes are internal improvements.

## Contributors

- Nick Russo <nick@nijaru.dev>

## Next Steps (v0.0.25)

**Remaining from evaluation**:
- Add COW vs non-COW test coverage
- Add cross-filesystem tests
- Create docs/FILESYSTEM_SUPPORT.md

---

**Full Changelog**: https://github.com/nijaru/sy/compare/v0.0.23...v0.0.24
