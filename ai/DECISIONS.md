# Decisions

## 2025-10-27: Release Versioning Strategy

**Context**: Planning version progression for a file synchronization tool where data safety is critical

**Decision**: Stay on 0.0.x until proven in production, never jump to 0.1.0+ based on test results alone

**Versioning Philosophy**:
- **0.0.x** (Current): "Works great in testing, use at your own risk"
  - For: Early adopters, testing, non-critical data
  - Continue: Until 3-6 months of real-world usage without data loss

- **0.1.0** (Future): "Production-ready, proven in the wild"
  - Requires: Months of 0.0.x releases, user testimonials, no data loss reports
  - Signals: API stabilizing, safe for production use

- **1.0.0** (Distant future): "Stable, widely trusted, battle-tested"
  - Years away, like rsync's maturity level

**Rationale**:
- File sync tools that lose data destroy trust forever
- No amount of testing replaces diverse real-world usage
- Edge cases emerge from actual environments that tests can't predict
- Tests show what we checked, not what we missed
- Conservative versioning protects users and reputation

**Current Status**: v0.0.48 with 411 tests passing, 23/23 SSH bisync scenarios pass, but zero months of production validation

**References**: .claude/CLAUDE.md, COMPREHENSIVE_TEST_REPORT.md

---

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

---

## 2025-10-21: Reorganize docs/ following agent-contexts v0.1.1

**Context**: Updated recommendations in agent-contexts added comprehensive directory organization

**Decision**: Reorganize documentation with subdirectories
- **docs/architecture/** - System design, technical specs, roadmaps
- **ai/research/archive/** - Historical snapshots

**Changes**:
- Moved DESIGN.md to docs/architecture/ (symlink at root for compatibility)
- Moved phase plans and roadmaps to docs/architecture/
- Moved old STATUS files to ai/research/archive/
- Updated AGENTS.md with Decision Flow diagram

**Rationale**:
- Clearer separation: permanent docs (docs/) vs evolving context (ai/)
- Architecture docs grouped together in docs/architecture/
- Historical snapshots preserved but separated
- Knowledge graduation path: ai/ → docs/ when permanent
- Follows standardized agent-contexts v0.1.1 patterns

**Tradeoffs**: More directory depth, but better organization

**References**: https://github.com/nijaru/agent-contexts (v0.1.1)

---

## 2025-11-11: Database Evaluation Framework

**Context**: Evaluating pure Rust database migrations (fjall, russh, object_store) against actual performance requirements

**Decision**: Evaluate on performance merit, not ideology. Migrate when real benefits exist.

**Results**:
- **fjall (LSM-tree, pure Rust)**: 56.8% faster writes than rusqlite on checksumdb workload → KEEP
- **object_store (multi-cloud)**: Cleaner API, multi-cloud support → KEEP as optional feature
- **russh (pure Rust SSH)**: SSH agent auth blocker (needs 200-300 LOC custom protocol code) → REJECT, use ssh2-rs

**Rationale**:
- Benchmarking shows fjall's 56% write advantage is material for large syncs (checksumdb is write-heavy)
- Reads are rare (only when metadata matches), so don't measure perf impact
- russh fails architectural requirements despite being pure Rust
- Pure Rust changes should be judged on outcomes, not philosophy

**Validation**: Created benches/checksumdb_bench.rs comparing fjall vs rusqlite (1,000 checksums)
- fjall write: 340.17 ms
- rusqlite write: 533.54 ms (56.8% slower)

**References**: ai/research/database-comparisons.md

---

## 2025-11-11: seerdb Evaluation (Rejected)

**Context**: Evaluated research-grade LSM (seerdb with learned indexes, WiscKey, Dostoevsky) against fjall

**Benchmark Results** (1K checksum operations):
- **fjall**: 328-342 ms write, 256-258 ms read
- **seerdb**: 18.0-18.4 ms write (18.2x faster), 6.3-8.5 ms read (30-43x faster)

**Decision**: Keep fjall as primary

**Reasons for Rejection**:
1. **Nightly-only**: seerdb requires Rust nightly (std::simd)
   - Creates deployment complexity
   - CI/release pipeline issues
   - Potential incompatibility with stable toolchains

2. **Experimental status**: README states "Not recommended for production use"
   - Checksumdb is durability-critical (data loss = re-hashing entire sync)
   - No production track record

3. **Workload mismatch**: seerdb advantages (18ms/1K) don't translate to real-world sync performance
   - Network/disk I/O dominates checksumdb queries
   - Typical sync has ~10K checksums, not benchmarks at 1M scale
   - 18ms improvement is sub-millisecond in sync context

**Future consideration**: If sy ever supports multi-TB syncs with millions of files, add optional `checksumdb-seerdb` feature for nightly builds with documentation warning

**References**: ai/research/database-comparisons.md, benches/seerdb_comparison_bench.rs
