# v0.0.51 - Automatic Transfer Resume

sy now automatically resumes interrupted large file transfers! If a network interruption or user cancellation occurs during a large file transfer, sy will automatically pick up where it left off on the next sync.

## What's New

### Automatic Resume for Interrupted Transfers

Large file transfers are now resilient to interruptions:

- **Checkpoint Saves**: Progress saved every 10MB
- **Automatic Resume**: sy detects interrupted transfers and resumes from last checkpoint
- **Bidirectional Support**: Works for both Remote→Local and Local→Remote transfers
- **Smart Detection**: Rejects stale resume state if source file was modified
- **User Feedback**: Shows resume progress percentage

### How It Works

When transferring a large file:
1. sy saves checkpoint every 10MB to `~/.cache/sy/resume/<hash>.json`
2. If transfer is interrupted (Ctrl+C, network failure, etc.), checkpoint is preserved
3. Next time you run the same sync, sy detects the incomplete transfer
4. Transfer resumes from last checkpoint using SFTP seek operations
5. On success, resume state is automatically cleaned up

### Example

```bash
# Start large file transfer (100GB file)
sy user@host:/large-file.iso /local/backup/

# ... transfer runs to 45% (45GB transferred) ...
# ... network interruption or Ctrl+C ...

# Resume transfer (automatically picks up at 45%)
sy user@host:/large-file.iso /local/backup/
# Output: "Resuming transfer of /large-file.iso from offset 48318382080 (45.0% complete)"
```

### Technical Details

**Remote→Local Resume** (commit: e155c00)
- SFTP streaming with `remote_file.seek(SeekFrom::Start(offset))`
- Local file opened in append mode
- Works with `copy_file_streaming` operation

**Local→Remote Resume** (commit: 1ca5b5e)
- Local file seek + remote SFTP seek
- Remote file opened with `sftp.open_mode(WRITE)` and seeked to resume offset
- Works with `copy_file` SFTP path (Compression::None)

**Resume State Management**
- Stored in `~/.cache/sy/resume/` (XDG-compliant)
- BLAKE3-based unique IDs (hash of source + dest + mtime)
- Atomic writes (temp + rename pattern)
- Automatic cleanup on successful completion
- Staleness detection via mtime comparison

### What's Not Included

Resume currently works for:
- ✅ SFTP streaming transfers (large files, uncompressed)

Resume does NOT work for:
- ❌ Compressed transfers (would require decompression state)
- ❌ Delta sync transfers (complex state requirements)
- ❌ Sparse file transfers (would need region tracking)

These may be added in future releases if there's demand.

## Performance

No performance overhead when transfers complete successfully. Resume state cleanup is automatic and instantaneous.

## Full Changelog

See [CHANGELOG.md](CHANGELOG.md) for complete changes.

## Installation

### From crates.io
```bash
cargo install sy
```

### From GitHub releases
Download the appropriate binary for your platform from the [releases page](https://github.com/nijaru/sy/releases/tag/v0.0.51).

## Previous Releases

**v0.0.50**: Network Recovery Activation - All SSH/SFTP operations now automatically retry on network failures
**v0.0.49**: Network Interruption Recovery Infrastructure - Added retry logic and resume state tracking
**v0.0.48**: Remote→Remote bidirectional sync + .gitignore support outside git repos
