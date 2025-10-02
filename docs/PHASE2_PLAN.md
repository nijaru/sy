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

### Task 1: Transport Abstraction (Week 1) ✅ **COMPLETE**

**Goal**: Create abstraction layer without breaking Phase 1

- [x] Define `Transport` trait
- [x] Refactor current code to use `LocalTransport`
- [x] Update `SyncEngine` to work with `Transport` trait
- [x] Ensure all Phase 1 tests still pass
- [x] Add integration tests for `LocalTransport`

**Success criteria**:
- ✅ All existing tests pass (55 tests)
- ✅ No performance regression
- ✅ Clean abstraction that works for both local and remote

**Completed**: 2025-10-01
**Commits**: 8c8389b, af520a1

### Task 2: SSH Config Parsing (Week 1-2) ✅ **COMPLETE**

**Goal**: Parse and apply SSH configuration

- [x] Create `ssh_config` module
- [x] Parse `~/.ssh/config` file
- [x] Support key directives:
  - [x] Host, HostName, Port, User
  - [x] IdentityFile
  - [x] ProxyJump
  - [x] ControlMaster, ControlPath, ControlPersist
  - [x] Compression
- [x] Handle pattern matching (wildcards, negation)
- [x] Apply defaults (port 22, current user, etc.)
- [x] Unit tests for config parsing (11 tests)
- [ ] Integration test with real SSH config (deferred to Task 3)

**Dependencies**:
- ✅ Implemented custom parser (no external SSH config parser needed)
- ✅ Added whoami, dirs, regex

**Completed**: 2025-10-01
**Commits**: ede17ba

### Task 3: Basic SSH Connection (Week 2) ✅ **COMPLETE**

**Goal**: Establish SSH connection to remote host

- [x] Add SSH library dependency (ssh2)
- [x] Implement connection establishment (TCP + handshake)
- [x] Support authentication methods:
  - [x] SSH key (most common)
  - [x] SSH agent
  - [ ] Password (interactive) - deferred
- [x] Handle connection errors gracefully
- [x] Add timeout handling (30 second default)
- [ ] Test connection to localhost (deferred to Task 9)
- [ ] Test connection to real remote host (deferred to Task 9)

**Dependencies**:
- ✅ `ssh2` crate (bindings to libssh2)
- ✅ `tokio` with `time` feature

**Decision**: ssh2 chosen
- ssh2: Mature, C bindings, widely used
- Consolidated all sync operations in single spawn_blocking for proper async/sync boundary handling

**Completed**: 2025-10-02
**Commits**: c308b45

### Task 4: Remote Scanner (Week 2-3) ✅ **COMPLETE**

**Goal**: Scan remote directory over SSH

**Approach**: Execute helper binary on remote host

```bash
# Remote helper: sy-remote
sy-remote scan /path/to/dir

# Returns JSON:
{
  "entries": [
    {"path": "file.txt", "size": 123, "mtime": 1234567890, "is_dir": false},
    ...
  ]
}
```

- [x] Create `sy-remote` binary (minimal)
- [x] Implement remote scanning via SSH exec
- [x] Parse JSON output from remote
- [x] SshTransport implementation with scan() and exists()
- [ ] Transfer `sy-remote` binary if not present (deferred to v0.3.0)
- [ ] Static linking for sy-remote (deferred to v0.3.0)
- [ ] Handle large directory listings efficiently (deferred to optimization phase)

**Alternative**: Use SFTP readdir (slower but no binary transfer needed) - Task 5

**Completed**: 2025-10-02
**Commits**: 3000126

### Task 5: SFTP Fallback (Week 3)

**Goal**: Implement SFTP transport for compatibility

- [ ] Implement `SftpTransport` using ssh2 SFTP
- [ ] Support all `Transport` trait methods
- [ ] Optimize buffer sizes (262KB)
- [ ] Handle concurrent requests
- [ ] Error handling for SFTP-specific issues
- [ ] Performance testing vs local baseline
- [ ] Fallback logic when custom protocol unavailable

### Task 6: File Transfer (Week 3-4) ✅ **COMPLETE**

**Goal**: Transfer files over SSH

- [x] Implement `SshTransport::copy_file()` for file upload via SFTP
- [x] Implement `SshTransport::create_dir_all()` for remote directory creation
- [x] Implement `SshTransport::remove()` for file/dir deletion
- [x] Preserve modification time on remote files
- [ ] Stream large files (avoid loading into memory) - deferred to optimization phase
- [ ] Progress tracking for network transfers (deferred to Task 8)
- [ ] Resume support (basic - Phase 4 will improve)
- [ ] Error handling improvements (network timeout, disk full, etc.)

**Completed**: 2025-10-02
**Commits**: e8f1f11

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

### Task 9: Integration & Testing (Week 5) ✅ **COMPLETE**

**Goal**: End-to-end testing and polish

- [x] CLI integration for remote paths (sy /local user@host:/remote)
- [x] TransportRouter implementation
- [x] Remote path parsing with Windows drive letter support
- [x] Update CHANGELOG
- [x] DualTransport implementation for mixed operations
- [x] Test with real remote host (Fedora via Tailscale)
- [x] Fix architectural issue with single-transport model
- [ ] Integration tests with SSH localhost (deferred to v0.2.1)
- [ ] Performance comparison: sy vs rsync vs scp (deferred to v0.2.1)
- [ ] Update benchmarks (deferred to v0.2.1)
- [ ] Update documentation (README, DESIGN, CONTRIBUTING) (deferred to Task 10)

**Status**: Core functionality working, end-to-end tests successful

**Testing Results (2025-10-02)**:
- ✅ SSH connection to Fedora via Tailscale successful
- ✅ sy-remote binary works on remote host
- ✅ SSH command execution works
- ✅ Local→remote sync works correctly
- ✅ Remote→local sync works correctly
- ✅ All 77 tests pass
- ✅ Update detection works for existing files

**Architectural Issue Resolved**:
Initial single-transport model couldn't handle mixed local/remote operations.
**Solution**: Implemented DualTransport with separate source/dest transports that routes operations correctly.

**Commit**: cd55c61

**Completed**: 2025-10-02

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
