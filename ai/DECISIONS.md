# Decisions

## 2025-10-21: Hash Function Selection

**Context**: Selecting hash functions for rolling hash, block checksums, and end-to-end verification

**Decisions**:
- **Adler-32**: Rolling hash for rsync algorithm
- **xxHash3**: Block checksums (fast, non-cryptographic)
- **BLAKE3**: End-to-end verification (cryptographic)

**Rationale**:
- Adler-32 is mathematically required for rsync's rolling hash algorithm
- xxHash3 provides fast block verification (faster than alternatives)
- BLAKE3 provides cryptographic guarantees for paranoid mode
- Research shows 5% of 100 Gbps transfers have corruption TCP doesn't detect

**Critical Constraint**: xxHash3 is NOT a rolling hash and cannot replace Adler-32 in delta sync

**References**: DESIGN.md:79-128

---

## 2025-10-20: Local Delta Sync Optimization

**Context**: Optimizing delta sync for local→local file synchronization

**Decision**: Use simple block comparison instead of rsync algorithm for local sync

**Rationale**:
- Both files available locally, no need for rolling hash overhead
- Can read both files in parallel and compare blocks directly
- Measured 5-9x performance improvement over rsync

**Implementation**: src/transport/local.rs

**References**: docs/EVALUATION_v0.0.23.md, docs/PERFORMANCE.md

---

## 2025-10-20: COW-Aware Sync Strategies

**Context**: Handling Copy-on-Write filesystems efficiently

**Decisions**:
1. **COW Strategy** (APFS/BTRFS/XFS):
   - Clone using COW reflinks (instant)
   - Only write changed blocks

2. **In-place Strategy** (ext4/NTFS or hard links):
   - Create empty temp file
   - Write all blocks

**Rationale**:
- COW cloning is instant (~1ms for 100MB file)
- Hard links MUST use in-place to preserve link semantics
- Automatic detection prevents corruption

**Tradeoffs**: More complex logic, but 5-9x faster on COW filesystems

**Critical**: Hard link detection (nlink > 1) forces in-place strategy

**References**: src/fs_util.rs, DESIGN.md

---

## 2025-10-20: Transport Protocol Selection

**Context**: Choosing network transport protocols

**Decision**: Custom binary protocol over SSH > SFTP > local I/O

**Rationale**:
- SSH ControlMaster provides 2.5x throughput boost
- TCP with BBR: 2-25x faster under packet loss vs CUBIC
- QUIC is 45% SLOWER on fast networks (>600 Mbps)

**Rejected Alternative**: QUIC for LAN/WAN (measured performance regression)

**References**: DESIGN.md:252-322

---

## 2025-10-20: Compression Strategy

**Context**: When to apply compression during file transfer

**Decision**: Adaptive compression based on network speed
- **>500 MB/s (4Gbps)**: No compression (CPU bottleneck)
- **100-500 MB/s**: LZ4 only
- **<100 MB/s**: Adaptive zstd
- **Local**: NEVER compress

**Rationale**: CPU compression overhead exceeds benefits on fast networks/disks

**Hardware Assumptions**: 2024+ hardware with modern CPUs

**References**: DESIGN.md:143-181

---

## 2025-10-21: Performance Monitoring Architecture

**Context**: Adding --perf flag for detailed performance metrics

**Decision**: Use Arc<Mutex<PerformanceMonitor>> with AtomicU64 counters

**Rationale**:
- Thread-safe metric collection during parallel sync
- Atomic operations minimize lock contention
- Optional Arc avoids overhead when --perf not set

**Tradeoffs**: Slight complexity vs valuable diagnostic information

**Implementation**: src/perf.rs, integrated in v0.0.33

---

## 2025-10-21: Error Collection Strategy

**Context**: Users need to see all errors, not just first failure

**Decision**: Collect errors in Vec<SyncError> during parallel execution

**Structure**:
```rust
pub struct SyncError {
    pub path: PathBuf,
    pub error: String,
    pub action: String,
}
```

**Rationale**:
- Users fix problems more efficiently seeing all failures
- Sync continues for successful files up to max_errors threshold
- Detailed context (path + action + error) aids debugging

**Implementation**: Added in v0.0.34

---

## 2025-10-21: Documentation Organization

**Context**: Separating agent working context from project documentation

**Decision**: Create ai/ directory following agent-contexts/PRACTICES.md patterns

**Structure**:
- ai/ → Agent working context (TODO, STATUS, DECISIONS, RESEARCH)
- docs/ → Project documentation (user and developer facing)
- AGENTS.md → AI entry point
- .claude/CLAUDE.md → Legacy compatibility, references AGENTS.md

**Rationale**:
- Standardized structure across projects
- Clear separation of concerns
- Token-efficient context loading

**References**: ~/github/nijaru/agent-contexts/PRACTICES.md
