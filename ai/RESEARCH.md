# Research

## File Transfer Corruption Studies (researched 2025-10)

**Sources**: ScienceDirect 2021, ACM studies on high-speed networking

**Key Findings**:
- 5% of 100 Gbps transfers have corruption TCP doesn't detect
- Multi-layer verification essential for data integrity
- Block-level checksums catch errors file-level hashing misses

**Applied**:
- Three-layer verification: TCP → xxHash3 → BLAKE3
- Block checksums in addition to file-level hashes
- Verification modes (fast/standard/verify/paranoid)

**References**: DESIGN.md:79-128

---

## rsync vs rclone Benchmarks (researched 2025-10)

**Sources**: Jeff Geerling (2025) performance comparisons

**Key Findings**:
- rsync single-threaded performance limits
- rclone parallel transfers show significant speedup
- Local sync has different optimization opportunities than remote

**Applied**:
- Parallel file transfers (--parallel flag)
- Separate optimization for local vs remote sync
- Measured 1.3x - 8.8x improvement over rsync

**References**: docs/PERFORMANCE.md

---

## QUIC Network Performance (researched 2025-10)

**Sources**: ACM 2024 - "QUIC is not Quick Enough over Fast Internet"

**Key Findings**:
- QUIC 45% slower than TCP on fast networks (>600 Mbps)
- QUIC benefits primarily low-latency, high-packet-loss scenarios
- TCP with BBR congestion control superior for file transfer

**Decision**: Don't use QUIC for LAN, TCP preferred

**References**: DESIGN.md:252-322

---

## Copy-on-Write Filesystems (researched 2025-10)

**Sources**: APFS, BTRFS, XFS documentation and benchmarks

**Key Findings**:
- COW reflinks are instant operations (~1ms for 100MB file)
- Must detect filesystem type to leverage COW features
- Hard links break with COW strategy (link semantics violated)

**Applied**:
- Filesystem detection using statfs
- COW strategy for APFS/BTRFS/XFS
- In-place strategy fallback for ext4/NTFS/hard links

**Implementation**: src/fs_util.rs

**References**: docs/EVALUATION_v0.0.23.md

---

## Hash Function Performance (researched 2025-10)

**Sources**: xxHash3, BLAKE3 official benchmarks

**Key Findings**:
- xxHash3: 10+ GB/s on modern CPUs (non-cryptographic)
- BLAKE3: 1-3 GB/s (cryptographic, parallelizable)
- Adler-32: Required for rsync rolling hash (not replaceable)

**Applied**:
- xxHash3 for fast block checksums (standard mode)
- BLAKE3 for cryptographic verification (verify/paranoid modes)
- Adler-32 for rolling hash in delta sync algorithm

**References**: DESIGN.md:79-128

---

## Compression Algorithms 2024+ (researched 2025-10)

**Sources**: zstd and LZ4 documentation, modern hardware benchmarks

**Key Findings**:
- LZ4: 400-500 MB/s compression speed
- zstd: Adaptive levels, good for varied network speeds
- Compression overhead exceeds benefits on >4Gbps connections

**Decision**: Adaptive thresholds based on network speed

**References**: DESIGN.md:143-181

---

## Sparse File Handling (researched 2025-10)

**Sources**: Unix SEEK_HOLE/SEEK_DATA documentation, filesystem testing

**Key Findings**:
- SEEK_HOLE/SEEK_DATA not universally supported
- set_len() doesn't guarantee sparse file creation on all FSes
- Filesystem-specific behavior requires graceful fallback

**Applied**:
- Try SEEK_HOLE/SEEK_DATA first (fast path)
- Fall back to block-based zero detection
- Tests verify correctness, log sparseness preservation

**Implementation**: src/transport/local.rs

**References**: tests/delta_sync_test.rs

---

## Atomic File Operations (researched 2025-10)

**Sources**: POSIX atomicity guarantees, filesystem documentation

**Key Findings**:
- rename() is atomic on same filesystem
- Write to temp, verify, rename pattern prevents corruption
- fsync() required before rename for durability

**Applied**:
- All file operations use temp → verify → rename
- fsync() calls before atomic rename
- TOCTOU detection for concurrent modification

**References**: DESIGN.md:548-1036

---

## Massive Scale Profiling (researched 2025-11)

**Sources**: Local profiling of sy v0.0.60 with 100k small files

**Key Findings**:
- **Memory Bottleneck**: Accumulating `Vec<FileEntry>` for 100k files consumed ~400MB heap.
- **Path Allocations**: `PathBuf` overhead is significant per file.
- **Execution Latency**: Planning phase blocked execution until full scan completed.

**Applied**:
- **Streaming Pipeline**: `scan_streaming()` iterator feeds directly into planner.
- **Memory Reduction**: 530MB → 133MB (75% reduction) for 100k file dataset.
- **Constant Footprint**: Memory usage now effectively constant regardless of file count (bounded by channel capacity).

**References**: ai/STATUS.md (v0.0.61)

---

## Open Questions

- [ ] Optimal cache eviction strategy for large syncs
- [ ] Best practices for SSH multiplexing in 2025
- [ ] Modern filesystem feature detection methods beyond statfs
- [ ] State-of-the-art error recovery strategies for network failures
