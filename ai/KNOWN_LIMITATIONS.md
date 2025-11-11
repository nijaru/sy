# Known Limitations

This document tracks known limitations and performance issues that are not critical bugs.

## Delta Sync: Remote→Local Not Supported

**Status**: Known limitation (not a bug)
**Severity**: Low (performance only, correctness unaffected)
**Discovered**: 2025-11-10 (during issue #3 audit)

### Description

Delta sync (rsync algorithm) does not work for remote→local file updates. The system always falls back to full file copy.

### Impact

- Remote→local file updates always transfer the entire file
- Performance degradation for updating large files from remote servers
- No impact on correctness - files are still transferred correctly
- Minimal real-world impact - most file updates rewrite the entire file anyway

### Technical Details

**Location**: `src/transport/dual.rs:81-125` (`sync_file_with_delta`)

**Root Cause**: Transport implementations assume unidirectional data flow:
- `SshTransport.sync_file_with_delta()` assumes source is LOCAL (line 1060 in ssh.rs)
- `LocalTransport.sync_file_with_delta()` assumes both paths are LOCAL

For remote→local sync:
1. `DualTransport` tries `dest.sync_file_with_delta(remote_path, local_path)`
   - LocalTransport can't read remote path → fails
2. Falls back to `source.sync_file_with_delta(remote_path, local_path)`
   - SshTransport tries `std::fs::metadata(remote_path)` → fails
3. Falls back to full `copy_file()` → **works correctly**

### Workaround

None needed - automatic fallback to full copy works correctly.

### Fix Complexity

**High** - Would require implementing reverse rsync protocol:

1. Download block checksums from remote file
2. Read local file to find matching blocks
3. Request only non-matching blocks from remote
4. Reconstruct file locally

Estimated effort: 1-2 days
Estimated benefit: Low (most updates are full rewrites)

### Recommendation

**Do not fix** unless users report specific pain:
- "Updating large files from remote is slow"
- "Network usage is high for small changes to large files"

### Related Issues

- Issue #3: Fixed related bug in `copy_file()` (same root cause, but critical)
- This limitation discovered during issue #3 audit

### See Also

- `bug-cross-transport-audit.md` - Full technical analysis
