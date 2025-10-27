# Network Interruption Recovery Design

**Date**: 2025-10-27
**Version**: v0.0.49 planning
**Status**: Planning

## Problem

Currently, if SSH connection drops mid-transfer:
- Transfer fails completely
- No resume capability
- User loses partial progress
- Need to restart entire sync

This is the **most likely real-world failure mode** for remote syncing.

## Goals

1. **Graceful Failure**: Clear error messages, no data corruption
2. **Resume Capability**: Continue where we left off
3. **Retry Logic**: Automatically retry with backoff
4. **Progress Preservation**: Don't lose partial work

## Current State Analysis

### Where Network Errors Occur

**SSH Transport Layer** (`src/transport/ssh.rs`):
- `copy_file()` - Full file transfers
- `sync_file_with_delta()` - Delta sync operations
- `scan()` - Directory scanning
- `exists()`, `metadata()` - Metadata operations
- `write_file()` - Direct writes (bisync)

**Network Error Types**:
1. Connection timeout (idle too long)
2. Network disconnect (WiFi loss, VPN drop)
3. SSH session killed (server restart)
4. Bandwidth issues (slow/stalled transfers)

### Current Error Handling

```rust
// ssh.rs uses ssh2 crate which returns std::io::Error
// These bubble up as SyncError::Io
```

**Issues**:
- No distinction between retryable vs fatal errors
- No partial transfer tracking
- Connection pool doesn't handle reconnection

## Design Approach

### 1. Error Classification

Add to `src/error.rs`:
```rust
pub enum NetworkError {
    Timeout { duration: Duration },
    Disconnected { reason: String },
    Retryable { inner: std::io::Error, attempts: u32 },
    Fatal { inner: std::io::Error },
}

impl NetworkError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, NetworkError::Timeout { .. }
               | NetworkError::Disconnected { .. }
               | NetworkError::Retryable { .. })
    }

    pub fn should_reconnect(&self) -> bool {
        matches!(self, NetworkError::Disconnected { .. })
    }
}
```

### 2. Retry Logic with Exponential Backoff

```rust
pub struct RetryConfig {
    max_attempts: u32,          // Default: 3
    initial_delay: Duration,    // Default: 1s
    max_delay: Duration,        // Default: 30s
    backoff_multiplier: f64,    // Default: 2.0
}

pub async fn retry_with_backoff<F, T, E>(
    config: &RetryConfig,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut() -> Pin<Box<dyn Future<Output = Result<T, E>>>>,
{
    let mut attempt = 0;
    let mut delay = config.initial_delay;

    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) if attempt >= config.max_attempts => return Err(e),
            Err(e) if is_retryable(&e) => {
                attempt += 1;
                log::warn!("Attempt {}/{} failed, retrying in {:?}...",
                          attempt, config.max_attempts, delay);
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(
                    Duration::from_secs_f64(delay.as_secs_f64() * config.backoff_multiplier),
                    config.max_delay
                );
            }
            Err(e) => return Err(e), // Non-retryable
        }
    }
}
```

### 3. Resume Capability

**Approach**: Chunked transfers with progress tracking

```rust
pub struct TransferState {
    file_path: PathBuf,
    total_size: u64,
    bytes_transferred: u64,
    chunk_size: usize,
    checksum: Option<Vec<u8>>, // For verification
}

// Store in ~/.cache/sy/resume/<hash>.json
// Hash = blake3(source_path + dest_path + mtime)
```

**Implementation**:
- Break large files into chunks (default: 1MB)
- Track completed chunks
- On resume, skip completed chunks
- Verify integrity with checksums

### 4. Connection Pool Resilience

**Enhance `SshConnectionPool`** (`src/transport/ssh.rs`):

```rust
impl SshConnectionPool {
    // Add reconnection capability
    pub async fn get_session_or_reconnect(&self) -> Result<Arc<Mutex<Session>>> {
        let session = self.get_session();

        // Check if session is alive
        if !self.is_session_alive(&session) {
            log::warn!("Session dead, reconnecting...");
            self.reconnect_session(&session).await?;
        }

        Ok(session)
    }

    fn is_session_alive(&self, session: &Arc<Mutex<Session>>) -> bool {
        // Try a lightweight operation (like getting server banner)
        // Return false if it fails
    }

    async fn reconnect_session(&self, old_session: &Arc<Mutex<Session>>) -> Result<()> {
        // Create new SSH session with same config
        // Replace in pool
    }
}
```

### 5. User-Facing Features

**CLI Flags**:
```bash
--retry <N>              # Max retry attempts (default: 3)
--retry-delay <SECS>     # Initial retry delay (default: 1)
--no-resume              # Disable resume capability
--resume-only            # Only resume, don't start new transfers
--clear-resume-state     # Clear all resume state
```

**Progress Output**:
```
Transferring file.dat... 45% (450 MB / 1 GB)
Network error: Connection timeout
Retrying (1/3) in 1s...
Reconnecting...
Resuming from 450 MB...
```

## Implementation Plan

### Phase 1: Error Classification (v0.0.49)
1. Add NetworkError enum
2. Classify ssh2 errors into retryable/fatal
3. Add tests for error classification

### Phase 2: Retry Logic (v0.0.49)
1. Implement RetryConfig
2. Add retry_with_backoff helper
3. Wrap SSH operations with retry
4. Add CLI flags (--retry, --retry-delay)
5. Test with simulated network failures

### Phase 3: Resume Capability (v0.0.50)
1. Add TransferState struct
2. Implement chunked transfers
3. Store/load resume state
4. Add CLI flags (--no-resume, --resume-only, --clear-resume-state)
5. Test with interrupted large file transfers

### Phase 4: Connection Pool Resilience (v0.0.50)
1. Add is_session_alive check
2. Implement reconnect_session
3. Use get_session_or_reconnect in all operations
4. Test with SSH session kills

## Testing Strategy

### Unit Tests
- Error classification logic
- Retry backoff calculation
- TransferState serialization/deserialization
- Chunk boundary handling

### Integration Tests
- Simulate network drops (kill SSH process mid-transfer)
- Test resume from various failure points
- Verify checksums after resumed transfers
- Test retry exhaustion behavior

### Real-World Testing
- Test over unstable WiFi
- Test with VPN disconnects
- Test with large files (1GB+) over slow connections
- Measure overhead of resume state tracking

## Risks & Mitigations

**Risk**: Resume state file corruption
- **Mitigation**: Atomic writes, checksums, validation on load

**Risk**: Performance overhead from chunking
- **Mitigation**: Make chunk size configurable, benchmark impact

**Risk**: Stale resume state accumulation
- **Mitigation**: TTL on resume files (7 days), auto-cleanup

**Risk**: Concurrent transfers interfering
- **Mitigation**: Lock per transfer, unique hash per file+path pair

## Success Criteria

1. Network drops during transfer don't lose progress
2. Automatic retry succeeds within 3 attempts (95% case)
3. Resume works for files interrupted at any point
4. Performance overhead < 5% for small files
5. Clear user feedback on retry/resume actions

## References

- rsync: Uses `--partial` and `--timeout` flags
- rclone: Has comprehensive retry logic with exponential backoff
- Syncthing: Connection management with auto-reconnect
- DESIGN.md: Current SSH transport architecture
