# Auto-Deploy sy-remote Implementation Plan

## Goal
Detect when `sy-remote` is not found on remote server and auto-deploy the binary from the local machine, eliminating the "command not found" error.

## Design

### 1. Architecture
- **Detection**: Catch exit code 127 ("command not found") from failed sy-remote execution
- **Deployment**: Copy sy-remote binary from `$CARGO_HOME/bin/` or `./target/release/sy-remote` to remote `~/.sy/bin/sy-remote`
- **Retro-exec**: Re-execute original command using deployed binary's path
- **Caching**: Once deployed in session, reuse without re-checking (same SFTP session)

### 2. Implementation Steps

#### Phase 1: Binary Location (simple)
- [x] sy-remote is built as release binary: `target/release/sy-remote` (4.0M)
- [x] Determine binary location at runtime:
  1. `env::current_exe().parent() / "sy-remote"` (installed via cargo install)
  2. `./target/release/sy-remote` (development)
  3. Fallback to searching PATH

#### Phase 2: Remote Deployment (core feature)
- **File**: src/transport/ssh.rs
- **Function**: Add `deploy_sy_remote()` method to SshTransport
- **Steps**:
  1. Check if exit_status == 127 in execute_command()
  2. Read local sy-remote binary
  3. Create remote `~/.sy/bin` directory via SSH
  4. Upload binary via SFTP with mode 0o755
  5. Retry command using full path: `/home/user/.sy/bin/sy-remote ...`
  6. Return result or error if deployment fails

#### Phase 3: Robustness
- **Platform detection**: Determine remote OS/arch (for future multi-arch support)
- **Verification**: Optional checksum verification (sha256) before execution
- **Logging**: Detailed logs when deploying binary
- **Error handling**: Clear messages if deployment fails (permissions, disk space, etc.)

### 3. Code Changes

#### src/transport/ssh.rs
```rust
impl SshTransport {
    // New method
    async fn deploy_sy_remote(&self, session: Arc<Mutex<Session>>) -> Result<String> {
        // 1. Find local binary
        // 2. Read binary data
        // 3. Create remote dir via SSH
        // 4. Upload via SFTP
        // 5. Set permissions
        // 6. Verify (optional)
        // 7. Log success
    }
    
    // Modify execute_command() to detect 127 and auto-deploy
    fn execute_command(
        session: Arc<Mutex<Session>>,
        command: &str,
    ) -> Result<String> {
        // ... existing code ...
        if exit_status == 127 && command.contains("sy-remote") {
            // Deploy and retry
        }
        // ... existing code ...
    }
}
```

#### Add detection in execute_command
- Parse stderr for "command not found" pattern
- If sy-remote command and exit_status == 127, call deploy_sy_remote()
- Reconstruct command using deployed path
- Re-execute command

### 4. Edge Cases
1. **Already deployed**: If ~\.sy\bin\sy-remote exists, skip deployment
2. **Permission denied**: Clear error if can't write to ~/.sy/bin
3. **Disk full**: Error if no space for 4MB binary
4. **Binary mismatch**: Log warning if remote arch doesn't match local (warn only, try anyway)
5. **Concurrent deploys**: Use file locking to prevent race conditions

### 5. Testing
- [ ] Unit test: Binary location detection (3 scenarios)
- [ ] Integration test: Deploy over SSH, verify binary works
- [ ] Error test: Permission denied, disk full, network errors
- [ ] End-to-end: Full sync with auto-deployed sy-remote

### 6. UX Flow
```
User runs: sy /local /remote
↓
First sy-remote call: "bash: sy-remote: command not found"
↓
sy detects exit code 127
↓
sy uploads binary to ~/.sy/bin/sy-remote (4MB, ~200ms)
↓
sy retries command with full path
↓
Sync proceeds normally
↓
Next call: Uses cached path (no re-upload)
```

### 7. Version
- Will ship in v0.0.61 (after CI/CD was v0.0.60)
- Breaking change: No
- Backwards compatible: Yes

## Risk Assessment
- **Low risk**: No breaking changes, purely additive feature
- **Mitigation**: Detect deployment failures and provide clear error messages
