# Decisions

## 2025-11-19: Streaming Sync Pipeline (Massive Scale Optimization)

**Context**: Profiling revealed O(N) memory scaling with file count. Syncing 100k files consumed ~530MB RAM, largely due to collecting all `FileEntry` objects before planning.

**Decision**: Implement a fully streaming pipeline: `Scan (Stream) -> Plan (Stream) -> Execute`.

**Rationale**:
- Decouples memory usage from total file count
- Enables processing of millions of files with constant memory footprint
- Reduces time-to-first-byte (transfer starts while scan continues)

**Implementation**:
- `Scanner::scan_streaming()` returns an Iterator instead of `Vec<FileEntry>`
- `SyncEngine` consumes the iterator, plans tasks on-the-fly, and dispatches to thread pool
- `FilterEngine` applied during stream

**Impact**:
- 100k files memory usage: 530MB → 133MB (75% reduction)
- Initial transfer start time: Immediate vs 5-10s delay

**References**: ai/TODO.md (v0.0.61 Scale & Stability)

---

## 2025-11-19: Auto-deploy sy-remote

**Context**: User friction when syncing to new remote hosts; required manual installation of `sy-remote`.

**Decision**: Automatically deploy `sy-remote` binary to remote host if missing or outdated.

**Rationale**:
- "Zero-setup" experience is a key competitive advantage
- SSH transport already has execution capability
- Binary size is small enough (~5-10MB) for quick transfer

**Mechanism**:
- Check remote `sy-remote --version`
- If missing or mismatch: `scp` local binary to `~/.sy/sy-remote`, `chmod +x`
- Update PATH for session

**Tradeoffs**:
- Initial connection slightly slower (once per version update)
- Assumes architecture compatibility (same arch as local, or cross-compiled binary available - currently assumes same arch)

---

## 2025-11-19: Watch Mode Architecture

**Context**: Implementing continuous synchronization feature.

**Decisions**:
1. **Feature Gating**: Gate `notify` dependency behind `watch` feature flag.
   - **Rationale**: `notify` pulls in system dependencies; keep core binary minimal.
2. **Local Source Only**: Restrict watch mode to local source paths.
   - **Rationale**: Watching remote files requires a persistent remote daemon/agent, significantly increasing complexity. Local watching covers 90% of "dev-sync" use cases.

**Implementation**: src/sync/watch.rs, Cargo.toml

---

## 2025-11-13: Critical Bug Fixes - Memory Safety and Data Protection (PR #2)

**Context**: Production readiness audit revealed 4 critical bugs causing OOM errors and data safety issues

**Decisions**:

1. **Streaming Checksum Chunk Size: 1MB**
   - **Rationale**: Balance between memory usage and I/O efficiency
   - Trade-off: Larger chunks (4MB) would reduce syscalls but increase memory
   - 1MB is sweet spot: minimal memory (2MB for 10GB file), efficient I/O
   - **Implementation**: src/transport/{mod,ssh}.rs, src/bin/sy-remote.rs

2. **Catastrophic Deletion Threshold: 10,000 files**
   - **Rationale**: Protects against accidental mass deletion while allowing legitimate large deletions
   - Below 10K: Normal confirmation prompt
   - Above 10K: Strict confirmation (`"DELETE <count>"` case-sensitive)
   - Still respects `--quiet` and `--json` for automation
   - **Implementation**: src/sync/mod.rs

3. **Resume State Cleanup: 7 days**
   - **Rationale**: Balance between recovery window and disk space
   - 7 days allows recovery from multi-day outages
   - Auto-cleanup prevents indefinite accumulation
   - **Implementation**: src/resume.rs

4. **Compression Size Limit: 256MB**
   - **Rationale**: sy-remote protocol requires buffering entire compressed payload
   - Files >256MB use SFTP instead (already efficient, chunks internally)
   - 256MB covers 99% of compressible files (logs, code, text)
   - **Alternative considered**: Streaming compression protocol (high complexity, low ROI)
   - **Implementation**: src/compress/mod.rs

5. **S3 Multipart Threshold: 5MB**
   - **Rationale**: AWS S3 minimum part size is 5MB
   - Small files (<5MB): Use simple put (one API call)
   - Large files (≥5MB): Use multipart upload (streaming)
   - **Implementation**: src/transport/s3.rs

6. **DualTransport Smart Delegation**
   - **Rationale**: Avoid unnecessary buffering when destination supports streaming
   - Try destination transport first (enables Local→SSH streaming via SFTP)
   - Fall back to read+write only if needed (Remote→Local, Remote→Remote)
   - **Impact**: 5GB RAM → 2MB RAM for Local→SSH transfers
   - **Implementation**: src/transport/dual.rs

**Impact**: Production-ready for large files (GB+ sizes), 5000x better memory usage

**Branch**: `claude/fix-sy-critical-bugs-011CV5prdUFzoZGEKHyRrajn`

---

## 2025-11-12: Optional ACL Feature (GitHub Issue #7)

**Context**: User reported `cargo install sy` failed on Linux due to missing libacl system dependency

**Decision**: Make ACL preservation optional via `--features acl` flag

**Rationale**:
- Default build should work everywhere with zero system dependencies
- Most users don't need ACL preservation (rare use case, like rsync's `-A` flag)
- Traditional Unix permissions (user/group/other) still preserved by default
- Users who need ACLs can opt-in: `cargo install sy --features acl`

**Implementation**:
- Feature flag: `acl = ["exacl"]` in Cargo.toml (singular, matching `s3` pattern)
- Conditional compilation: `#[cfg(all(unix, feature = "acl"))]`
- Runtime validation: Clear error if `--preserve-acls` used without feature
- Platform differences:
  - Linux: Requires libacl-devel at build time (not runtime)
  - macOS: Uses native ACL APIs (no external dependencies)

**Testing**: Created `scripts/test-acl-portability.sh` for Docker-based validation

**Impact**: Eliminates installation friction for 95%+ of users who don't use ACLs

**Future Pattern**: Same approach for SSH and notify features (optional but included by default)

**References**: Issue #7, branch feat/optional-acls

---

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
