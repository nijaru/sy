# sy v0.0.25 - Test Coverage & Quality Release

**Release Date**: 2025-10-20

## Summary

Enhanced test coverage for delta sync strategies with comprehensive testing of COW vs non-COW code paths and filesystem detection.

## Improvements

### Expanded Test Coverage

**New Integration Tests** (tests/delta_sync_test.rs):

1. **COW Strategy Verification** (macOS-specific)
   - Verifies COW strategy is used on APFS
   - Checks strategy selection logging
   - Tests with 15MB files (above delta sync threshold)

2. **In-place Strategy Verification**
   - Verifies in-place strategy is used when hard links detected
   - Checks strategy reason logging
   - Tests with hard-linked files

3. **Strategy Equivalence Testing**
   - Verifies both COW and in-place strategies produce identical output
   - Tests same file modification scenario with different strategies
   - Ensures bit-perfect correctness regardless of strategy

4. **Strategy Correctness Across File Sizes**
   - Tests 1KB, 10KB, 100KB, and 1MB files
   - Verifies correctness across different block sizes
   - Ensures strategy selection doesn't affect output

5. **Cross-Filesystem Test** (manual/ignored)
   - Documents how to test cross-filesystem behavior
   - Provides setup instructions for macOS ramdisk testing
   - Verifies in-place strategy used when crossing filesystems

6. **Same-Filesystem Detection Unit Test**
   - Tests `same_filesystem()` function directly
   - Verifies files in same directory detected correctly
   - Tests file and parent directory detection

### Test Statistics

**Total tests**: 304 (up from 294)
- **Delta sync tests**: 11 (5 new)
  - 10 running automatically
  - 1 ignored (manual cross-filesystem test)

**Coverage improvements**:
- ✅ COW strategy verification
- ✅ In-place strategy verification
- ✅ Strategy equivalence testing
- ✅ Multi-size correctness testing
- ✅ Filesystem detection unit tests
- ⏳ Cross-filesystem (manual test only)

## Quality Improvements

### Better Test Organization

**Strategy-specific tests**:
- Separate tests for COW vs in-place paths
- Clear documentation of platform requirements
- Explicit file size requirements (>10MB for delta sync)

### Comprehensive Documentation

**Test documentation**:
- Inline comments explaining test purpose
- Setup instructions for manual tests
- Platform-specific test guards (#[cfg] attributes)

**Manual test instructions**:
```bash
# Cross-filesystem testing on macOS
hdiutil attach -nomount ram://204800  # 100MB ramdisk
diskutil erasevolume APFS "TestFS" /dev/diskN
export CROSS_FS_PATH=/Volumes/TestFS
cargo test test_cross_filesystem_uses_inplace_strategy -- --ignored --nocapture
hdiutil detach /dev/diskN
```

## Code Quality

**Build status**: ✅ Clean (no warnings, no errors)

**All tests passing**: 304 tests ✅
- 303 running automatically
- 1 manual test (documented)

## Performance

No performance changes - test-only release.

## Platform Support

Same as v0.0.24:
- ✅ macOS (APFS) - COW strategy fully tested
- ✅ Linux (BTRFS/XFS/ext4) - Strategy detection tested
- ⚠️ Windows (NTFS) - needs testing

## Known Limitations

Same as v0.0.24:
- No change ratio detection
- Sparse file preservation not guaranteed in delta sync
- Windows NTFS not yet tested

## Migration Guide

No migration needed - this is a test-only release. All changes are internal test improvements.

## Testing Notes

### Automated Tests (cargo test)
All new tests run automatically except the cross-filesystem test which requires manual setup.

### Manual Tests
The cross-filesystem test is marked `#[ignore]` and requires:
- Multiple mounted filesystems
- Environment variable configuration
- Cleanup after testing

See test documentation for detailed setup instructions.

## Contributors

- Nick Russo <nick@nijaru.dev>

## Next Steps (v0.0.26+)

**Future improvements**:
- Windows CI testing and NTFS verification
- Change ratio detection (skip delta if >75% changed)
- Sparse file preservation in delta sync
- Create docs/FILESYSTEM_SUPPORT.md

---

**Full Changelog**: https://github.com/nijaru/sy/compare/v0.0.24...v0.0.25
