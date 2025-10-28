# Resume Integration - v0.0.51

## Goal
Integrate TransferState infrastructure into file transfers to enable automatic resume of interrupted large file transfers.

## Current State (v0.0.50)
- ✅ TransferState infrastructure complete (v0.0.49)
- ✅ Retry logic activated for all operations (v0.0.50)
- ❌ TransferState not used in actual file transfers
- ❌ No checkpointing during transfers
- ❌ Resume state not loaded/saved during transfers

## Design

### Integration Points

**1. SFTP Streaming Operations** (primary target)
- `copy_file_streaming()` - Remote→Local transfers
- `copy_file()` SFTP path - Local→Remote transfers (Compression::None case)

These operations already use chunked reading/writing - perfect for checkpointing.

**2. Checkpoint Strategy**
- Save state every 10MB (10 chunks of 1MB each)
- Atomic state writes (temp + rename)
- Clear state on successful completion
- Staleness detection via mtime

### Implementation Approach

**Step 1: Add Resume to copy_file_streaming (Remote→Local)**
```rust
async fn copy_file_streaming_with_resume(
    &self,
    source: &Path,
    dest: &Path,
    progress_callback: Option<...>,
) -> Result<TransferResult> {
    let source_meta = self.file_info(source).await?;
    let file_size = source_meta.size;
    let mtime = source_meta.modified;

    // Try to load existing resume state
    if let Some(mut state) = TransferState::load(source, dest, mtime)? {
        if !state.is_stale(mtime) {
            // Resume from checkpoint
            return self.resume_streaming_transfer(source, dest, state, progress_callback).await;
        }
    }

    // Start new transfer with checkpointing
    self.streaming_transfer_with_checkpoints(source, dest, file_size, mtime, progress_callback).await
}
```

**Step 2: Implement Checkpoint Logic**
```rust
async fn streaming_transfer_with_checkpoints(...) -> Result<TransferResult> {
    let mut state = TransferState::new(source, dest, file_size, mtime, ...);
    const CHECKPOINT_INTERVAL: usize = 10; // Save every 10 chunks (10MB)

    // SFTP streaming loop with checkpoint saves
    loop {
        // Read chunk
        // Write chunk
        state.update_progress(bytes_read);

        // Save checkpoint every 10 chunks
        if state.total_chunks() % CHECKPOINT_INTERVAL == 0 {
            state.save()?;
        }
    }

    // Clear state on success
    state.clear()?;
}
```

**Step 3: Implement Resume Logic**
```rust
async fn resume_streaming_transfer(..., mut state: TransferState, ...) -> Result<TransferResult> {
    // Seek to resume position
    let resume_offset = state.bytes_transferred;

    // Open source at offset (SFTP seek)
    // Open dest in append mode

    // Continue transfer from checkpoint
    loop {
        // Read chunk from offset
        // Write chunk
        state.update_progress(bytes_read);

        // Checkpoint every 10 chunks
        if state.total_chunks() % CHECKPOINT_INTERVAL == 0 {
            state.save()?;
        }
    }

    // Clear state on success
    state.clear()?;
}
```

## Implementation Plan

### Phase 1: SFTP Streaming Resume (copy_file_streaming)
1. Add `copy_file_streaming_with_resume()` method
2. Implement checkpoint saving (every 10MB)
3. Implement resume from checkpoint
4. Handle SFTP seek/append operations
5. Test with large files (100MB+)

### Phase 2: Local→Remote Resume (copy_file SFTP path)
1. Add checkpointing to SFTP write path
2. Implement resume logic for local→remote
3. Test bidirectional resume

### Phase 3: CLI Integration
1. Add `--resume` flag (default: true)
2. Add `--checkpoint-interval` flag (default: 10MB)
3. Update `--resume-only` to use new resume logic
4. Test end-to-end resume scenarios

## Testing Strategy

### Unit Tests
- Test checkpoint saving/loading
- Test staleness detection
- Test resume offset calculation
- Test state cleanup on success

### Integration Tests
1. **Interrupt & Resume**: Kill transfer mid-way, verify resume
2. **Stale State**: Modify source, verify fresh transfer
3. **Multiple Interrupts**: Interrupt multiple times, verify cumulative progress
4. **Large Files**: Test with 100MB, 500MB, 1GB files

### Manual Testing
```bash
# Start large transfer (interrupt with Ctrl+C)
sy user@host:/large-file /local --retry 3

# Resume automatically
sy user@host:/large-file /local --retry 3

# Resume only (don't start new)
sy user@host:/large-file /local --resume-only

# Clear resume state
sy user@host:/large-file /local --clear-resume-state
```

## Edge Cases

1. **Partial chunk at resume**: Handle when resume offset isn't chunk-aligned
2. **SFTP seek limitations**: Some SFTP implementations may not support seek
3. **Dest file exists but incomplete**: Verify size matches state
4. **Network failure during checkpoint save**: State may be incomplete
5. **Concurrent transfers**: Lock mechanism or unique state IDs

## Success Criteria

✅ Large file transfers automatically resume after interruption
✅ Checkpoint overhead <5% (10MB intervals minimize writes)
✅ Resume works for both Remote→Local and Local→Remote
✅ All existing tests still pass
✅ New resume tests demonstrate functionality

## Non-Goals (Future)

- Resume for compressed transfers (requires decompression state)
- Resume for delta sync (complex state requirements)
- Resume for sparse files (would need region tracking)
- Parallel chunk transfers (v0.0.52+)

## Timeline Estimate

- Phase 1: 3-4 hours (Remote→Local resume)
- Phase 2: 2-3 hours (Local→Remote resume)
- Phase 3: 1-2 hours (CLI integration + testing)
- Total: 6-9 hours

## References

- v0.0.49 resume infrastructure: `src/resume.rs`
- SFTP operations: `src/transport/ssh.rs` (copy_file_streaming, copy_file)
- Retry integration: v0.0.50 implementation
