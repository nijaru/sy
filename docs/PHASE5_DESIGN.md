# Phase 5 Design - Verification & Reliability

**Status**: Planning (2025-10-06)
**Target**: v0.2.0
**Timeline**: 2 weeks
**Dependencies**: Phase 4 complete (v0.0.13)

---

## Overview

Phase 5 focuses on multi-layer integrity verification and crash recovery, making sy suitable for critical data transfers where correctness is paramount. While Phase 1-4 focused on performance and usability, Phase 5 adds verifiable correctness guarantees.

**Core Principle**: Speed vs Reliability should be a user choice, not a hidden tradeoff.

---

## 1. BLAKE3 End-to-End Verification

### Goal
Add cryptographic verification option for critical transfers where speed can be sacrificed for guaranteed correctness.

### Requirements
- Optional BLAKE3 checksums for complete file verification
- `--verify` flag to enable
- Per-file verification after transfer
- Paranoid mode for block-level verification
- Minimal overhead in non-verify modes

### Design

**Current State** (v0.0.13):
- xxHash3 used for delta sync block checksums (fast, non-cryptographic)
- No end-to-end verification after transfer
- Trust in TCP checksums (known to miss 1 in 16M-10B corrupted packets)

**Proposed Changes**:

```rust
// src/integrity/mod.rs (new module)
pub enum ChecksumType {
    None,           // Trust TCP + filesystem
    Fast,           // xxHash3 (current default)
    Cryptographic,  // BLAKE3
}

pub struct IntegrityVerifier {
    checksum_type: ChecksumType,
    verify_on_write: bool,  // Paranoid mode
}

impl IntegrityVerifier {
    pub fn compute_file_checksum(&self, path: &Path) -> Result<Checksum> {
        match self.checksum_type {
            ChecksumType::None => Ok(Checksum::None),
            ChecksumType::Fast => self.compute_xxhash3(path),
            ChecksumType::Cryptographic => self.compute_blake3(path),
        }
    }

    pub fn verify_transfer(&self, source: &Path, dest: &Path) -> Result<bool> {
        let source_sum = self.compute_file_checksum(source)?;
        let dest_sum = self.compute_file_checksum(dest)?;
        Ok(source_sum == dest_sum)
    }
}
```

**Usage**:
```bash
# Current default (trust TCP)
sy /src /dst

# Fast checksums (xxHash3)
sy /src /dst --mode standard

# Cryptographic verification (BLAKE3)
sy /src /dst --verify
sy /src /dst --mode verify  # Equivalent

# Paranoid mode (verify every block during transfer)
sy /src /dst --mode paranoid
```

### Implementation Tasks

1. **Create integrity module** (`src/integrity/`)
   - `mod.rs` - Public API
   - `blake3.rs` - BLAKE3 wrapper
   - `xxhash3.rs` - xxHash3 wrapper (refactor from delta/)
   - `verifier.rs` - Verification orchestration

2. **Add verification to transfer flow**
   - Compute source checksum before transfer (optional)
   - Compute destination checksum after transfer
   - Compare and report mismatch
   - Retry on verification failure (configurable)

3. **Performance optimization**
   - Parallel checksum computation (rayon)
   - Stream checksums during transfer when possible
   - Cache checksums in resume state

4. **Error handling**
   - New error type: `IntegrityError::ChecksumMismatch`
   - Configurable behavior: fail vs warn
   - Logging of all verification failures

### Testing

- Unit tests: Checksum computation correctness
- Integration tests:
  - Detect intentional corruption
  - Verify large files (>1GB)
  - Parallel checksum computation
- Performance tests:
  - Baseline vs --verify overhead
  - Ensure xxHash3 path has no regression

---

## 2. Verification Modes

### Goal
Provide clear, named modes for different speed/reliability tradeoffs.

### Requirements
- Named modes instead of flag combinations
- Clear documentation of what each mode does
- Backward compatible with existing flags

### Design

**Mode Hierarchy**:
```
fast      → Size + mtime only (fastest, least reliable)
standard  → + xxHash3 checksums (good balance)
verify    → + BLAKE3 end-to-end (slow, cryptographic)
paranoid  → BLAKE3 + verify every block during transfer (slowest, maximum reliability)
```

**CLI Integration**:
```rust
// src/cli.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum VerificationMode {
    /// Size and mtime only (fastest)
    Fast,

    /// Add xxHash3 checksums (default)
    Standard,

    /// BLAKE3 end-to-end verification
    Verify,

    /// BLAKE3 + verify every block (slowest)
    Paranoid,
}

pub struct Cli {
    // ...

    #[arg(long, value_enum, default_value = "standard")]
    pub mode: VerificationMode,

    // Legacy flag support
    #[arg(long)]
    pub verify: bool,  // Shortcut for --mode verify
}
```

**Behavior Matrix**:

| Mode | Size/mtime | xxHash3 | BLAKE3 | Block Verify | Use Case |
|------|------------|---------|--------|--------------|----------|
| fast | ✓ | ✗ | ✗ | ✗ | Dev sync, trusted network |
| standard | ✓ | ✓ | ✗ | ✗ | **Default**, good balance |
| verify | ✓ | ✓ | ✓ | ✗ | Critical data, backups |
| paranoid | ✓ | ✓ | ✓ | ✓ | Untrusted network, max safety |

### Implementation Tasks

1. **Add VerificationMode enum** to CLI
2. **Map mode to behavior**
   - Create IntegrityConfig from VerificationMode
   - Pass config through sync engine
3. **Update documentation**
   - README examples for each mode
   - DESIGN.md explanation of tradeoffs
4. **Legacy flag mapping**
   - `--verify` → `--mode verify`
   - Warn on deprecated flags

### Testing

- Unit tests: Mode to config mapping
- Integration tests: Each mode produces expected behavior
- Documentation: Performance comparison table

---

## 3. Crash Recovery

### Goal
Gracefully handle interruptions and filesystem errors with automatic recovery.

### Requirements
- Transaction log for multi-file operations
- Detect incomplete operations on restart
- Rollback or complete partial operations
- Self-healing for corrupted state files

### Design

**Current State** (v0.0.13):
- Resume support tracks completed files
- No transaction log
- Atomic file operations (temp + rename)
- No handling of corrupted state files

**Proposed Changes**:

#### Transaction Log

**Location**: `{destination}/.sy-transaction.log`

**Format** (JSON Lines):
```json
{"ts":"2025-10-06T12:00:00Z","op":"start","file":"dir/file1.txt"}
{"ts":"2025-10-06T12:00:05Z","op":"write","file":"dir/file1.txt","temp":".sy.tmp.abc123"}
{"ts":"2025-10-06T12:00:10Z","op":"commit","file":"dir/file1.txt","temp":".sy.tmp.abc123"}
{"ts":"2025-10-06T12:00:15Z","op":"complete","file":"dir/file1.txt"}
```

**Recovery Logic**:
```rust
// src/sync/recovery.rs
pub struct RecoveryManager {
    log_path: PathBuf,
}

impl RecoveryManager {
    pub fn scan_incomplete_operations(&self) -> Result<Vec<IncompleteOp>> {
        // Read transaction log
        // Find operations with "start" but no "complete"
        // Return list of incomplete operations
    }

    pub fn rollback(&self, op: &IncompleteOp) -> Result<()> {
        // Delete temp file if exists
        // Log rollback
    }

    pub fn resume(&self, op: &IncompleteOp) -> Result<()> {
        // Verify temp file checksum
        // Complete the rename if valid
        // Log completion
    }
}
```

**On Startup**:
1. Check for `.sy-transaction.log`
2. If exists, scan for incomplete operations
3. Ask user or auto-decide (based on `--auto-recover` flag):
   - Rollback (delete temp files)
   - Resume (complete pending operations)
   - Abort (manual intervention)

#### State File Corruption Detection

```rust
// src/sync/resume.rs
impl ResumeState {
    pub fn load(destination: &Path) -> Result<Option<Self>> {
        let state_path = destination.join(".sy-state.json");
        if !state_path.exists() {
            return Ok(None);
        }

        match serde_json::from_str::<ResumeState>(&contents) {
            Ok(state) => {
                // Verify state integrity
                if state.verify_integrity()? {
                    Ok(Some(state))
                } else {
                    // Corrupted state detected
                    warn!("Resume state corrupted, ignoring");
                    Self::delete(destination)?;
                    Ok(None)
                }
            }
            Err(e) => {
                // JSON parse error - corrupted file
                warn!("Failed to parse resume state: {}", e);
                Self::delete(destination)?;
                Ok(None)
            }
        }
    }

    fn verify_integrity(&self) -> Result<bool> {
        // Check version is supported
        if self.version != CURRENT_VERSION {
            return Ok(false);
        }

        // Check paths are absolute
        if !self.source.is_absolute() || !self.destination.is_absolute() {
            return Ok(false);
        }

        // Check timestamps are reasonable
        if self.started_at > chrono::Utc::now() {
            return Ok(false);
        }

        Ok(true)
    }
}
```

### Implementation Tasks

1. **Create recovery module** (`src/sync/recovery.rs`)
   - Transaction log writer
   - Incomplete operation scanner
   - Rollback/resume logic

2. **Integrate with sync engine**
   - Log operations to transaction log
   - Check for incomplete ops on startup
   - Prompt user or auto-recover

3. **Add CLI flags**
   - `--auto-recover` - Automatically resume incomplete operations
   - `--no-recover` - Skip recovery check
   - `--clean-state` - Delete all state files before starting

4. **State file hardening**
   - Add integrity checks to ResumeState
   - Graceful degradation on corruption
   - Better error messages

### Testing

- Unit tests: Transaction log parsing
- Integration tests:
  - Simulate crash mid-transfer
  - Verify recovery completes correctly
  - Test corrupted state file handling
- Stress tests: Recovery with 1000s of files

---

## 4. Atomic Operations

### Goal
Document and test existing atomic operations, add opt-out for special filesystems.

### Requirements
- Verify atomic operations work correctly
- Document guarantees
- Add `--no-atomic` for filesystems without rename support (NFS v2, some FUSE)

### Design

**Current State** (v0.0.13):
- Files written to `.sy.tmp.XXXXXX` tempfile
- fsync() before rename (on supported platforms)
- Rename to final destination (atomic on POSIX)

**Proposed Changes**:

```rust
// src/sync/transfer.rs
pub struct TransferOptions {
    pub atomic: bool,  // Default: true
    pub fsync_before_rename: bool,  // Default: true (if supported)
}

impl FileTransfer {
    pub async fn write_file(&self, content: &[u8], dest: &Path, opts: &TransferOptions) -> Result<()> {
        if opts.atomic {
            // Current behavior: temp + rename
            let temp_path = self.generate_temp_path(dest)?;
            fs::write(&temp_path, content)?;

            if opts.fsync_before_rename {
                self.fsync_file(&temp_path)?;
            }

            fs::rename(&temp_path, dest)?;
        } else {
            // Direct write (non-atomic)
            // WARNING: Can leave partial file on crash
            fs::write(dest, content)?;
        }

        Ok(())
    }

    fn fsync_file(&self, path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let file = fs::File::open(path)?;
            file.sync_all()?;
        }
        Ok(())
    }
}
```

**CLI**:
```bash
# Default (atomic)
sy /src /dst

# Non-atomic (for filesystems without rename)
sy /src /dst --no-atomic

# Skip fsync (faster but less safe)
sy /src /dst --no-fsync
```

### Implementation Tasks

1. **Add TransferOptions struct**
2. **Add CLI flags** (`--no-atomic`, `--no-fsync`)
3. **Document guarantees**
   - DESIGN.md: When atomic operations are safe
   - README: Filesystem compatibility
4. **Testing**
   - Test atomic rename on various filesystems
   - Verify fsync is called when enabled
   - Test --no-atomic fallback

### Testing

- Unit tests: Verify temp file cleanup
- Integration tests:
  - Atomic behavior (interrupt during rename)
  - Non-atomic fallback
- Platform tests: macOS, Linux, Windows

---

## 5. CLI Changes Summary

New flags:
```bash
--mode <MODE>        # fast | standard | verify | paranoid
--verify             # Shortcut for --mode verify
--auto-recover       # Automatically resume incomplete operations
--no-recover         # Skip recovery check on startup
--clean-state        # Delete all state files before starting
--no-atomic          # Direct write instead of temp+rename
--no-fsync           # Skip fsync before rename (faster, less safe)
```

Updated defaults:
- `--mode standard` (xxHash3 checksums enabled by default)

---

## 6. Testing Strategy

### Unit Tests (Target: 30+ new tests)

**Integrity Module**:
- BLAKE3 checksum correctness
- xxHash3 wrapper correctness
- Checksum comparison logic
- Parallel checksum computation

**Verification Modes**:
- Mode enum to config mapping
- Legacy flag compatibility

**Recovery**:
- Transaction log parsing
- Incomplete operation detection
- Rollback logic
- Resume logic

**Atomic Operations**:
- Temp file generation
- Rename atomicity
- fsync calling

### Integration Tests (Target: 15+ new tests)

**End-to-End Verification**:
- Transfer with --verify detects corruption
- Transfer with --mode paranoid catches block errors
- Performance overhead measurement

**Crash Recovery**:
- Simulate crash mid-transfer
- Verify recovery completes successfully
- Test auto-recover flag

**Atomic Operations**:
- Interrupt during rename
- Verify no partial files left

### Performance Tests

**Verification Overhead**:
- Baseline (no verify): < 500ms for 100 files
- Standard mode: < 600ms (20% overhead acceptable)
- Verify mode: < 2s (4x overhead acceptable)
- Paranoid mode: No hard limit (document actual)

### Property Tests

**Invariants**:
- All transfers are atomic or cleanly rollback
- Checksums never produce false positives
- Recovery never corrupts destination

---

## 7. Documentation Updates

### README.md
- Add verification modes section
- Document --verify flag usage
- Add performance comparison table

### DESIGN.md
- Update integrity section with BLAKE3 design
- Document verification mode tradeoffs
- Add crash recovery architecture

### User Guide (new: docs/USER_GUIDE.md)
- When to use each verification mode
- How crash recovery works
- Filesystem compatibility notes

---

## 8. Migration & Compatibility

**Backward Compatibility**:
- Default behavior unchanged (fast mode → standard mode is minor change)
- All existing flags continue to work
- Resume state format compatible (version field allows migration)

**Breaking Changes**: None

**Deprecations**:
- None (all new features)

---

## 9. Success Criteria

- [ ] BLAKE3 verification works correctly
- [ ] All 4 verification modes implemented and tested
- [ ] Crash recovery detects and handles incomplete operations
- [ ] State file corruption is detected and handled gracefully
- [ ] Atomic operations documented and tested
- [ ] `--no-atomic` fallback works on special filesystems
- [ ] All 150+ tests pass (including 45+ new Phase 5 tests)
- [ ] Documentation updated (README, DESIGN, USER_GUIDE)
- [ ] Zero regressions in existing features
- [ ] Performance overhead < 20% for standard mode

---

## 10. Implementation Plan

**Week 1**:
- Day 1-2: Integrity module + BLAKE3 integration
- Day 3-4: Verification modes + CLI integration
- Day 5: Testing + documentation

**Week 2**:
- Day 1-2: Transaction log + crash recovery
- Day 3: Atomic operations documentation + testing
- Day 4: Integration testing + performance validation
- Day 5: Documentation + release prep

**Estimated Effort**: 2 weeks (10 days)

---

## 11. Risks & Mitigations

**Risk 1**: BLAKE3 performance overhead too high
- Mitigation: Make it opt-in (--verify flag)
- Fallback: Document as "use only for critical transfers"

**Risk 2**: Transaction log adds complexity
- Mitigation: Keep log format simple (JSONL)
- Fallback: Implement basic version first, enhance later

**Risk 3**: Filesystem compatibility issues with atomic operations
- Mitigation: Provide --no-atomic flag
- Testing: Test on NFS, FUSE, SMB filesystems

---

## 12. Future Enhancements (Post-Phase 5)

- Merkle tree for directory-level verification
- Signed checksums (GPG integration)
- Checksum caching for faster re-verification
- Parallel BLAKE3 with SIMD

---

**Last Updated**: 2025-10-06
**Status**: Design complete, ready for implementation
**Next**: Begin implementation of integrity module
