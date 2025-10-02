# Phase 2: Network Sync - Implementation Plan

**Goal**: Enable remote sync via SSH with `sy /src remote:/dst`

**Target**: v0.2.0

## Overview

Phase 2 adds network synchronization capabilities while maintaining the performance and UX established in Phase 1. The implementation focuses on SSH transport with SFTP fallback for maximum compatibility.

## Architecture Changes

### Transport Abstraction Layer

```rust
// New trait to abstract local vs remote operations
pub trait Transport {
    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>>;
    async fn read(&self, path: &Path) -> Result<Vec<u8>>;
    async fn write(&self, path: &Path, data: &[u8]) -> Result<()>;
    async fn metadata(&self, path: &Path) -> Result<Metadata>;
    async fn exists(&self, path: &Path) -> Result<bool>;
    async fn create_dir_all(&self, path: &Path) -> Result<()>;
    async fn remove(&self, path: &Path) -> Result<()>;
}

// Implementations
pub struct LocalTransport { /* ... */ }
pub struct SshTransport { /* ... */ }
pub struct SftpTransport { /* ... */ }
```

### Module Structure

```
src/
├── transport/
│   ├── mod.rs              # Transport trait definition
│   ├── local.rs            # Local filesystem (current impl)
│   ├── ssh.rs              # Custom SSH protocol
│   ├── sftp.rs             # SFTP fallback
│   └── network.rs          # Network detection
├── ssh/
│   ├── mod.rs              # SSH session management
│   ├── config.rs           # SSH config parsing (~/.ssh/config)
│   ├── connect.rs          # Connection establishment
│   └── protocol.rs         # Custom binary protocol
```

## Implementation Tasks

### Task 1: Transport Abstraction (Week 1)

**Goal**: Create abstraction layer without breaking Phase 1

- [ ] Define `Transport` trait
- [ ] Refactor current code to use `LocalTransport`
- [ ] Update `SyncEngine` to work with `Transport` trait
- [ ] Ensure all Phase 1 tests still pass
- [ ] Add integration tests for `LocalTransport`

**Success criteria**:
- All existing tests pass
- No performance regression
- Clean abstraction that works for both local and remote

### Task 2: SSH Config Parsing (Week 1-2)

**Goal**: Parse and apply SSH configuration

- [ ] Create `ssh_config` module
- [ ] Parse `~/.ssh/config` file
- [ ] Support key directives:
  - [ ] Host, HostName, Port, User
  - [ ] IdentityFile
  - [ ] ProxyJump
  - [ ] ControlMaster, ControlPath, ControlPersist
  - [ ] Compression
- [ ] Handle pattern matching (wildcards, negation)
- [ ] Apply defaults (port 22, current user, etc.)
- [ ] Unit tests for config parsing
- [ ] Integration test with real SSH config

**Dependencies**:
- Consider using `ssh2` or `russh` crate
- Or implement minimal parser

### Task 3: Basic SSH Connection (Week 2)

**Goal**: Establish SSH connection to remote host

- [ ] Add SSH library dependency (ssh2 or russh)
- [ ] Implement connection establishment
- [ ] Support authentication methods:
  - [ ] SSH key (most common)
  - [ ] SSH agent
  - [ ] Password (interactive)
- [ ] Handle connection errors gracefully
- [ ] Add timeout handling
- [ ] Test connection to localhost
- [ ] Test connection to real remote host

**Dependencies**:
- `ssh2` crate (bindings to libssh2)
- OR `russh` (pure Rust implementation)

**Decision needed**: ssh2 vs russh
- ssh2: Mature, C bindings, widely used
- russh: Pure Rust, async-first, modern

### Task 4: Remote Scanner (Week 2-3)

**Goal**: Scan remote directory over SSH

**Approach**: Execute helper binary on remote host

```bash
# Remote helper: sy-remote (statically linked binary)
sy-remote scan /path/to/dir --format json

# Returns JSON:
{
  "entries": [
    {"path": "file.txt", "size": 123, "mtime": 1234567890, "is_dir": false},
    ...
  ]
}
```

- [ ] Create `sy-remote` binary (minimal, statically linked)
- [ ] Implement remote scanning via SSH exec
- [ ] Transfer `sy-remote` binary if not present
- [ ] Parse JSON output from remote
- [ ] Handle errors (permission denied, path not found, etc.)
- [ ] Test with various directory structures
- [ ] Handle large directory listings efficiently

**Alternative**: Use SFTP readdir (slower but no binary transfer needed)

### Task 5: SFTP Fallback (Week 3)

**Goal**: Implement SFTP transport for compatibility

- [ ] Implement `SftpTransport` using ssh2 SFTP
- [ ] Support all `Transport` trait methods
- [ ] Optimize buffer sizes (262KB)
- [ ] Handle concurrent requests
- [ ] Error handling for SFTP-specific issues
- [ ] Performance testing vs local baseline
- [ ] Fallback logic when custom protocol unavailable

### Task 6: File Transfer (Week 3-4)

**Goal**: Transfer files over SSH

- [ ] Implement `SshTransport::write()` for file upload
- [ ] Implement `SshTransport::read()` for file download (less common)
- [ ] Stream large files (avoid loading into memory)
- [ ] Progress tracking for network transfers
- [ ] Resume support (basic - Phase 4 will improve)
- [ ] Error handling (network timeout, disk full, etc.)
- [ ] Test with various file sizes (small, medium, large)
- [ ] Verify data integrity (checksum after transfer)

### Task 7: Network Detection (Week 4)

**Goal**: Auto-detect network type and optimize accordingly

```rust
enum NetworkType {
    Local,      // Same machine
    Lan,        // < 10ms RTT, > 100 Mbps
    Wan,        // Everything else
}
```

- [ ] Implement RTT measurement (ping)
- [ ] Implement bandwidth estimation (small sample transfer)
- [ ] Classify network type
- [ ] Adjust buffer sizes based on network
- [ ] Adjust compression settings
- [ ] Test with localhost (should detect as Local)
- [ ] Test with LAN host
- [ ] Test with WAN host

### Task 8: Error Handling & UX (Week 4)

**Goal**: Helpful errors for network issues

- [ ] SSH connection failures (auth, timeout, host not found)
- [ ] Network timeouts during transfer
- [ ] Disk full on remote
- [ ] Permission denied on remote
- [ ] Remote binary not found or incompatible
- [ ] Progress display for network transfers
- [ ] ETA calculation
- [ ] Bandwidth display
- [ ] User-friendly error messages

### Task 9: Integration & Testing (Week 5)

**Goal**: End-to-end testing and polish

- [ ] Integration tests with SSH localhost
- [ ] Test with real remote hosts (LAN and WAN)
- [ ] Performance comparison: sy vs rsync vs scp
- [ ] Update benchmarks
- [ ] Update documentation (README, DESIGN, CONTRIBUTING)
- [ ] Update CHANGELOG
- [ ] Fix any bugs found in testing

### Task 10: Documentation & Release (Week 5)

**Goal**: Document Phase 2 and prepare release

- [ ] Update README with network sync examples
- [ ] Update docs/PERFORMANCE.md with network benchmarks
- [ ] Write migration guide (Phase 1 → Phase 2)
- [ ] Update CONTRIBUTING.md with Phase 2 architecture
- [ ] Tag v0.2.0 release
- [ ] Publish release notes

## Technical Decisions

### SSH Library: ssh2 vs russh

**Recommendation**: Start with ssh2, consider russh later

**Rationale**:
- ssh2: Mature, battle-tested, used by cargo and other tools
- russh: Pure Rust, async-first, but less mature
- Can switch later if needed (abstraction layer makes this easier)

**Decision**: Use ssh2 for Phase 2

### Remote Execution: Helper Binary vs SFTP

**Recommendation**: Hybrid approach

**Approach**:
1. Try to use helper binary (`sy-remote`) for scanning
2. Fall back to SFTP if binary not available or incompatible
3. Always use efficient method available

**Benefits**:
- Helper binary is fast (custom protocol)
- SFTP provides compatibility
- Best of both worlds

### Network Detection Strategy

**Simple approach for Phase 2**:
1. Measure RTT with simple ping
2. Transfer 1MB sample to estimate bandwidth
3. Classify as Local/LAN/WAN

**Phase 3** will add more sophisticated detection:
- Packet loss measurement
- Congestion detection
- Dynamic adaptation

## Dependencies to Add

```toml
[dependencies]
# SSH connectivity
ssh2 = "0.9"

# Async runtime (already present, but will use more)
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "time"] }

# Serialization for remote protocol
serde_json = "1"  # For sy-remote communication

# Networking
dns-lookup = "2"  # For hostname resolution
```

## Testing Strategy

### Unit Tests
- SSH config parsing
- Network type detection
- Transport trait implementations

### Integration Tests
- Local SSH (localhost)
- Remote SSH (if available in CI)
- SFTP fallback
- Error scenarios

### Performance Tests
- Compare to rsync for network transfers
- Measure overhead of SSH vs local
- Verify no regression in local performance

### Property-Based Tests
- File contents identical after transfer
- Idempotent network sync
- Partial transfer resumption

## Performance Goals

### Phase 2 Goals (Conservative)
- **Network overhead**: < 10% vs rsync for same-size transfers
- **LAN sync (100 files)**: < 2s
- **WAN sync (100 files)**: Comparable to rsync
- **No regression**: Local sync remains as fast as Phase 1

### Phase 3 will optimize further with parallelism

## Risks & Mitigation

### Risk 1: SSH library compatibility issues
**Mitigation**: Use widely-adopted ssh2, extensive testing

### Risk 2: Performance worse than rsync
**Mitigation**: Benchmark early, optimize incrementally

### Risk 3: Complex SSH configurations (ProxyJump, etc.)
**Mitigation**: Start simple, add features incrementally

### Risk 4: Remote platform compatibility
**Mitigation**: Static linking for sy-remote, test on multiple platforms

## Success Criteria

Phase 2 is successful when:

1. ✅ `sy /local/path user@remote:/remote/path` works reliably
2. ✅ Automatically uses SSH config settings
3. ✅ Falls back to SFTP when custom protocol unavailable
4. ✅ Helpful error messages for common issues
5. ✅ Performance comparable to rsync for network transfers
6. ✅ All Phase 1 functionality still works (no regressions)
7. ✅ Comprehensive tests (unit, integration, performance)
8. ✅ Updated documentation

## Timeline

**Total estimate**: 5 weeks

- Week 1: Transport abstraction + SSH config parsing
- Week 2: SSH connection + Remote scanner
- Week 3: SFTP fallback + File transfer
- Week 4: Network detection + Error handling
- Week 5: Integration testing + Documentation

**Milestone**: v0.2.0 release after Week 5

## Open Questions

1. Should we support `rsync://` URLs or just SSH?
   - **Decision**: SSH only for Phase 2, rsync protocol is complex

2. Should we compress data over network?
   - **Decision**: Phase 2 uses SSH compression setting, Phase 3 adds adaptive compression

3. Should we support concurrent transfers in Phase 2?
   - **Decision**: No, defer to Phase 3 (parallel transfers)

4. Should we verify checksums after transfer?
   - **Decision**: Yes, basic checksum (size + mtime check), full checksums in Phase 5

## Next Steps

1. Review this plan with team/community
2. Begin Task 1: Transport abstraction
3. Set up test infrastructure for SSH testing
4. Start weekly progress updates

---

**Status**: Planning complete, ready to begin implementation
**Created**: 2025-10-01
**Target Release**: v0.2.0 (2025-11-01, ~5 weeks)
