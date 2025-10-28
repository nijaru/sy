# Network Recovery Activation - v0.0.50

## Goal
Activate the retry and resume infrastructure built in v0.0.49, making it functional in production.

## Current State (v0.0.49)
- ✅ Error classification (retryable vs fatal)
- ✅ Retry logic with exponential backoff (`retry_with_backoff`)
- ✅ Resume state infrastructure (`TransferState`)
- ✅ Retry config passed through transport layers
- ❌ `execute_command_with_retry` exists but unused (marked `#[allow(dead_code)]`)
- ❌ Direct `execute_command` calls throughout SSH transport
- ❌ No session health checks or auto-reconnect
- ❌ Resume state not integrated into file transfers

## Challenge: Async/Blocking Boundary

**Current Pattern:**
```rust
tokio::task::spawn_blocking({
    let session = self.connection_pool.get_session();
    let cmd = command.clone();
    move || Self::execute_command(session, &cmd)
})
```

**Issues:**
- `execute_command_with_retry` is `async fn` requiring `&self`
- `spawn_blocking` closure needs `move` semantics
- `execute_command` uses blocking `std::sync::Mutex`

## Solution: Direct Async Calls

**Replace with:**
```rust
self.execute_command_with_retry(
    self.connection_pool.get_session(),
    &command
)
.await?
```

**Rationale:**
- `retry_with_backoff` handles async retry loop
- Each retry calls `execute_command` synchronously
- SSH2 Session operations are inherently blocking anyway
- Runtime can yield between retries during backoff delays
- Simpler code, no spawn_blocking overhead

## Implementation Plan

### Phase 1: Activate Retry in SSH Commands

**Operations to convert:**
1. `scan()` - Directory scanning
2. `exists()` - Path existence check
3. `create_dir_all()` - Directory creation
4. `file_info()` - Metadata queries
5. File transfer commands (read/write operations)

**Steps:**
1. Remove `#[allow(dead_code)]` from `execute_command_with_retry`
2. Replace `spawn_blocking` + `execute_command` pattern with direct `execute_command_with_retry` calls
3. Test that retries actually happen on network failures

### Phase 2: Connection Pool Health Checks

**Add to ConnectionPool:**
```rust
impl ConnectionPool {
    /// Check if a session is still alive
    fn is_session_healthy(session: &Session) -> bool {
        // Try a lightweight command (echo test)
        session.channel_session().is_ok()
    }

    /// Get a session, reconnecting if stale
    pub async fn get_session_with_health_check(&self) -> Arc<Mutex<Session>> {
        let session = self.get_session();

        // Check health before returning
        if !Self::is_session_healthy(&session.lock().unwrap()) {
            // Reconnect logic
        }

        session
    }
}
```

**Integration:**
- Use `get_session_with_health_check()` instead of `get_session()`
- Detect stale sessions before command execution
- Auto-reconnect on session failures

### Phase 3: Resume State Integration

**Hook into file transfer:**
```rust
async fn copy_file_with_resume(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
    let metadata = source.metadata()?;
    let mtime = metadata.modified()?;

    // Try to load existing resume state
    if let Some(state) = TransferState::load(source, dest, mtime)? {
        if !state.is_stale(mtime) {
            // Resume from checkpoint
            return self.resume_transfer(source, dest, state).await;
        }
    }

    // Start new transfer with state tracking
    self.copy_file_with_checkpoints(source, dest).await
}
```

**Checkpointing:**
- Save state every N chunks (e.g., every 10 MB)
- Atomic state writes
- Clear state on successful completion

## Testing Strategy

### Unit Tests
- Test retry on various error types
- Test session health detection
- Test resume state save/load

### Integration Tests
1. **Retry Test:** Simulate network failure mid-operation, verify retry
2. **Health Check Test:** Kill session, verify auto-reconnect
3. **Resume Test:** Interrupt large file transfer, verify resume

### Manual Testing
```bash
# Test retry
sy /local user@host:/remote --retry 5 --retry-delay 1

# Interrupt during transfer (Ctrl+C)
sy /large-file user@host:/dest --retry 3

# Resume
sy /large-file user@host:/dest --resume-only
```

## Success Criteria

✅ All direct `execute_command` calls replaced with retry version
✅ Network failures trigger automatic retry with backoff
✅ Stale sessions detected and reconnected
✅ Large file transfers can resume after interruption
✅ All existing tests still pass
✅ New tests validate retry/resume behavior

## Non-Goals (Future Work)

- Parallel chunk transfers within files (v0.0.51+)
- Progress reporting during retries (v0.0.51+)
- Configurable checkpoint frequency (v0.0.51+)
- Resume across different machines (requires sync state)

## Risks & Mitigation

**Risk:** Retry logic adds latency
**Mitigation:** Only retry on actual failures, exponential backoff prevents hammering

**Risk:** Resume state gets corrupted
**Mitigation:** Atomic writes, staleness detection, clear on source modification

**Risk:** Health checks add overhead
**Mitigation:** Lightweight check (channel creation), only on session acquisition

## Implementation Order

1. Phase 1 Step 1-2: Convert scan/exists/create_dir_all operations (low risk)
2. Test thoroughly
3. Phase 1 Step 3: Convert file transfer operations (higher risk)
4. Test thoroughly
5. Phase 2: Add health checks
6. Phase 3: Add resume capability

## Timeline Estimate

- Phase 1: 2-3 hours (convert operations + test)
- Phase 2: 1-2 hours (health checks)
- Phase 3: 2-3 hours (resume integration)
- Total: 5-8 hours for complete activation

## References

- v0.0.49 retry infrastructure: `src/retry.rs`, `src/error.rs`
- v0.0.49 resume infrastructure: `src/resume.rs`
- SSH transport: `src/transport/ssh.rs`
- Connection pool: `src/transport/ssh.rs` (ConnectionPool struct)
