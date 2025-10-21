# Design Document

This document captures key design decisions and technical rationale for `sy`.

## Vision

**sy** is a modern file synchronization tool in the spirit of `eza`, `fd`, and `ripgrep` - not a drop-in rsync replacement, but a reimagining of file sync with better UX, adaptive performance, and verifiable integrity.

## Core Philosophy

### 1. Verifiable, Not "Perfect"
- **Reality**: 100% reliability is physically impossible (TCP misses 1 in 16M-10B packets, 5% of 100Gbps transfers have undetected corruption)
- **Goal**: Multi-layer verification with end-to-end integrity checks
- **Approach**: Detect and recover, not prevent all errors

### 2. Adaptive, Not One-Size-Fits-All
- **Reality**: Local sync, LAN sync, and WAN sync have completely different bottlenecks
- **Goal**: Auto-detect environment and optimize accordingly
- **Approach**: Different strategies for different scenarios

### 3. Transparent Tradeoffs
- **Reality**: Speed and reliability often conflict
- **Goal**: Clear modes letting users choose their priorities
- **Approach**: Explicit flags: `--mode local|lan|wan|verify|paranoid`

---

## Core Decisions

### 1. Parallel Architecture

**Decision**: Implement both parallel file transfers AND parallel chunk transfers for large files.

**Rationale**:
- rsync's bottleneck: transfers one file at a time (~150Mb/s on 1Gb networks)
- rclone achieves 4x speedup with `--multi-thread-streams` (parallel chunks)
- Modern hardware has multi-core CPUs; we should use them
- Network bandwidth often exceeds single-stream throughput

**Implementation approach**:
```rust
// Parallel files
for file in files.chunks(worker_count) {
    tokio::spawn(transfer_file(file));
}

// Parallel chunks within large files
if file.size > CHUNK_THRESHOLD {
    for chunk in file.chunks(CHUNK_SIZE) {
        tokio::spawn(transfer_chunk(chunk));
    }
}
```

**Configurability**:
- `--workers N` - Number of parallel file transfers (default: CPU cores)
- `--chunk-size SIZE` - Chunk size for large files (default: 4MB)
- `--no-parallel` - Disable parallelism (for debugging)

---

### 2. Delta Sync Algorithm

**Decision**: Different algorithms for local vs remote sync - simple comparison for local, rsync algorithm for remote.

**Key Insight**: When both files are available locally (source and destination on same machine), rsync's rolling hash algorithm is unnecessary overhead. The algorithm was designed for the constraint that only ONE file is available locally.

**Local Delta Sync** (v0.0.23+):
```rust
// Both files available locally - direct block comparison
let mut source_file = File::open(&source)?;
let mut dest_file = File::open(&dest)?;

loop {
    let src_bytes = source_file.read(&mut src_buf)?;
    let dst_bytes = dest_file.read(&mut dst_buf)?;

    if src_buf[..src_bytes] != dst_buf[..dst_bytes] {
        // Blocks differ - need to write
        write_block_to_temp(src_buf, src_bytes)?;
    } else {
        // Blocks match - skip write (if using COW strategy)
    }
}
```

**Performance**: 6x faster than rsync algorithm for local sync (no rolling hash computation, no signature generation/lookup).

**Remote Delta Sync** (future):
- Use rsync's rolling hash algorithm (Adler-32)
- Generate signatures on destination, send to source
- Source computes delta using rolling hash
- Send delta operations (Copy/Data) to destination

**Rationale**:
- Research shows rsync is still state-of-the-art for single-round remote protocols
- Multi-round protocols save bandwidth but add latency (not worth it)
- rsync algorithm is battle-tested, well-understood for remote sync
- For local sync, both files available → simpler approach is faster

#### COW-Aware Strategy Selection (v0.0.23+)

**Decision**: Auto-detect filesystem capabilities and choose optimal delta sync strategy.

**Two Strategies**:

1. **COW Strategy** (Copy-on-Write filesystems):
   ```rust
   // Clone destination file (instant COW reflink - ~1ms for 100MB)
   fs::copy(&dest, &temp)?;

   // Only write changed blocks to clone
   if src_block != dst_block {
       temp_file.seek(offset)?;
       temp_file.write_all(src_block)?;
   }
   // Unchanged blocks: skip write, clone already has correct data
   ```

   **Performance**: 33% faster (61ms vs 92ms for 1MB Δ in 100MB file)

   **Used when**:
   - Filesystem supports COW reflinks (APFS, BTRFS, XFS)
   - Source and destination on same filesystem (same device ID)
   - Destination has no hard links (preserves hard link integrity)

2. **In-place Strategy** (non-COW filesystems):
   ```rust
   // Create temp file, allocate space
   let temp = File::create(temp_path)?;
   temp.set_len(source_size)?;

   // Write ALL blocks (changed + unchanged) to build complete file
   loop {
       temp_file.seek(offset)?;
       temp_file.write_all(src_block)?;
   }
   ```

   **Performance**: Avoids 2x regression on ext4 (most common Linux filesystem)

   **Used when**:
   - Filesystem doesn't support COW (ext4, NTFS, HFS+)
   - Cross-filesystem sync (different mount points)
   - Destination has hard links (nlink > 1)

**Filesystem Detection** (src/fs_util.rs):
```rust
// macOS: Check filesystem type name
#[cfg(target_os = "macos")]
pub fn supports_cow_reflinks(path: &Path) -> bool {
    // Use statfs to get f_fstypename field
    // Return true if filesystem is "apfs"
}

// Linux: Check filesystem magic numbers
#[cfg(target_os = "linux")]
pub fn supports_cow_reflinks(path: &Path) -> bool {
    // Use statfs to get f_type field
    // Return true if BTRFS (0x9123683E) or XFS (0x58465342)
}

// Cross-filesystem detection
pub fn same_filesystem(path1: &Path, path2: &Path) -> bool {
    // Compare dev_t device IDs from metadata
    meta1.dev() == meta2.dev()
}

// Hard link detection
pub fn has_hard_links(path: &Path) -> bool {
    // Check nlink field from metadata
    metadata.nlink() > 1
}
```

**Critical for correctness**:
- Hard links MUST use in-place strategy (COW clone creates new inode, breaking link)
- File truncation when source < dest (both strategies use `set_len()`)
- Platform-specific `fs::copy()` optimizations:
  - macOS: `clonefile()` for instant COW reflinks
  - Linux: `copy_file_range()` for zero-copy I/O

**Performance data** (v0.0.23, macOS M3 Max, APFS):
- Full file copy: 47% faster (39ms vs 73ms for 100MB) using `fs::copy()` vs manual loop
- Delta sync (1MB Δ in 100MB): 5.7x faster than rsync (58ms vs 330ms)
- Delta sync (identical files): 8.8x faster than rsync (36ms vs 320ms)

---

### 3. Block-Level Integrity (Inspired by Syncthing)

**Decision**: Block-based checksums with dual hash strategy, not file-level only.

**Rationale**:
- File-level hashes can't detect partial corruption
- Block-level enables precise resume/recovery
- Parallel verification across blocks
- Delta sync at block granularity

**Dual hash approach**:
```rust
struct Block {
    offset: u64,
    size: usize,
    hash_weak: u64,        // xxHash3 - fast comparison, in-flight corruption detection
    hash_strong: [u8; 32], // BLAKE3 - cryptographic end-to-end verification
}

// Rolling hash for delta algorithm (MUST be Adler-32 variant)
// This is different from the block checksums above!
fn rolling_checksum(window: &[u8]) -> u32 {
    adler32_rolling(window)  // Required for rsync algorithm
}
```

**IMPORTANT**: xxHash3 is NOT a rolling hash and cannot replace Adler-32 in the delta sync algorithm. It's used for block verification only.

**Block size strategy**:
```rust
fn block_size(file_size: u64) -> usize {
    match file_size {
        0..=1_MB => 64_KB,
        1_MB..=100_MB => 256_KB,
        100_MB..=1_GB => 1_MB,
        _ => 4_MB,
    }
}
```

**Verification modes**:
- `--mode fast`: xxHash3 only, trust network (default)
- `--mode standard`: xxHash3 + BLAKE3 spot checks (10% of blocks)
- `--mode verify`: BLAKE3 all blocks
- `--mode paranoid`: BLAKE3 + comparison reads + multiple passes

**Performance data**:
- xxHash3: ~10x faster than MD5, 10GB/s single-thread
- BLAKE3: ~10-15x faster than SHA-2, 3-16 GB/s (parallelizable)
- Adler-32: Required for rolling hash (rsync algorithm)

---

### 4. Compression Strategy

**Decision**: Adaptive compression with smart defaults and file-type awareness.

**Rationale**:
- Compression helps on network transfers, wastes CPU on local copies
- Many files are already compressed (.jpg, .mp4, .zip)
- Modern compression is MUCH faster than originally assumed (see benchmarks)

**Benchmarked Performance** (2024 hardware, Rust implementations):
- **LZ4**: 23 GB/s throughput (text data)
- **Zstd level 3**: 8 GB/s throughput (text data)
- **CPU bottleneck**: Only occurs at >64 Gbps transfer speeds (unrealistic)
- **Conclusion**: Network is ALWAYS the bottleneck, CPU never is

**Algorithm** (simplified based on benchmarks):
```rust
fn should_compress(file: &File, connection: &Connection) -> Compression {
    // LOCAL: Never compress (disk I/O is bottleneck, not network/CPU)
    if connection.is_local() {
        return Compression::None;
    }

    // Skip small files (overhead > benefit)
    if file.size < 1_MB {
        return Compression::None;
    }

    // Skip already-compressed formats
    if COMPRESSED_EXTENSIONS.contains(&file.ext) {
        return Compression::None;
    }

    // NETWORK: Always use Zstd (8 GB/s >> any network speed)
    // Even 100 Gbps networks are only 12.5 GB/s, so compression never bottlenecks
    Compression::Zstd
}
```

**Compression options**:
- **zstd level 3** (default): 8 GB/s throughput, best ratio/speed balance
- **lz4** (optional): 23 GB/s throughput, but worse compression ratio than Zstd
- **zstd level 11+** (future): Higher compression for very slow networks
- **none**: Local transfers, small files, pre-compressed formats

**File type skip list**:
```rust
const COMPRESSED_EXTENSIONS: &[&str] = &[
    // Images
    "jpg", "jpeg", "png", "gif", "webp", "avif",
    // Video
    "mp4", "mkv", "avi", "mov", "webm",
    // Audio
    "mp3", "flac", "m4a", "ogg", "opus",
    // Archives
    "zip", "gz", "br", "zst", "7z", "xz", "bz2",
    // Documents
    "pdf", "docx", "xlsx", "pptx",
];
```

**CLI flags**:
- Default: Adaptive (test first chunk, cache decision)
- `--compress`: Force zstd level 3
- `--fast`: Force lz4
- `--max-compression`: Force zstd level 11
- `--no-compress`: Disable compression

---

### 5. CLI Design

**Decision**: New, intuitive interface with optional rsync compatibility.

**Primary interface** (simple, obvious):
```bash
sy <source> <destination>                  # Basic sync
sy <source> <destination> --preview        # Show changes first
sy <source> <destination> --delete         # Remove files not in source
sy <source> <destination> --verify         # Cryptographic checksums
```

**Compatibility layer** (for rsync users):
```bash
sy --rsync-compat -avz <source> <destination>  # Accept rsync flags
```

**Config file support**:
```toml
# ~/.config/sy/config.toml
[[sync]]
name = "docs"
source = "~/Documents"
destination = "backup:/docs"
delete = true
compress = "zstd"

[[sync]]
name = "media"
source = "~/Pictures"
destination = "nas:/media"
compress = false  # Already compressed
```

Then: `sy docs` (uses config)

---

### 6. Transport Protocol Stack

**Decision**: Custom binary protocol over SSH, with SFTP fallback and local optimization.

**Rationale**:
- **QUIC**: 45% slower on fast networks (>600 Mbps), only beneficial for high-latency + packet-loss
- **TCP**: Proven, with BBR for WAN and CUBIC for LAN
- **SSH**: Ubiquitous, secure (note: ControlMaster requires OpenSSH, not available in ssh2 library)
- **Custom > SFTP**: SFTP has packet-encryption overhead; custom protocol more efficient

**Implementation**:
```rust
enum Transport {
    // Local: Kernel optimizations
    Local {
        use_copy_file_range: bool,  // Linux zero-copy
        use_clonefile: bool,        // macOS CoW
        parallel_io: true,
    },

    // Fast path: Custom binary protocol over SSH tunnel
    SshCustom {
        session: SshSession,
        control_master: true,        // Reuse connection
        streams: Vec<ParallelStream>, // Multiple streams in one session
        buffer_size: 262_000,        // 262KB (Linux), 100KB (Windows)
        congestion: Auto,            // BBR or CUBIC based on network
    },

    // Compatibility: Optimized SFTP
    SftpOptimized {
        session: SftpSession,
        buffer_size: 262_000,
        control_master: true,
        concurrent_requests: 64,
    },
}
```

**Network auto-detection**:
```rust
async fn detect_network(remote: &Remote) -> NetworkProfile {
    // 1. Measure RTT
    let rtt = ping(remote).await;

    // 2. Bandwidth test (1MB sample)
    let bandwidth = bandwidth_test(remote, 1_MB).await;

    // 3. Packet loss test
    let packet_loss = loss_test(remote).await;

    NetworkProfile {
        bandwidth,
        latency: rtt,
        packet_loss,
        congestion_control: match (bandwidth, rtt, packet_loss) {
            (_, _, >1.0%) => BBR,           // Packet loss: use BBR
            (_, >50ms, _) => BBR,            // High latency: use BBR
            (>1Gbps, <10ms, _) => CUBIC,    // Fast LAN: use CUBIC
            _ => CUBIC,
        },
    }
}
```

**Protocol selection**:
- Local paths: Direct I/O with kernel optimizations
- SSH available: Custom protocol (fastest)
- SSH unavailable/old: SFTP (compatibility)
- Future: QUIC for mobile/unstable connections

---

### 7. Skip & Resume Logic

**Decision**: Size + mtime check with tolerance, optional quick checksum, then delta sync.

**Rationale**:
- Most files unchanged between syncs
- Quick metadata check avoids expensive delta computation
- mtime has reliability issues (NFS timezone, FAT32 granularity, clock skew)
- Resume support for interrupted transfers

**mtime reliability issues**:
- NFS: Server timezone differences, attribute caching delays
- FAT32/exFAT: 2-second granularity
- Clock skew: Systems with unsynchronized clocks
- Leap seconds: Can cause 1-second differences

**Implementation**:
```rust
fn transfer_strategy(local: &File, remote: &File, mode: Mode) -> Strategy {
    // Quick skip: identical size and similar mtime
    let mtime_tolerance = match remote.filesystem_type {
        FilesystemType::FAT32 | FilesystemType::ExFAT => Duration::from_secs(2),
        FilesystemType::NFS => Duration::from_secs(3),  // Account for caching
        _ => Duration::from_secs(1),  // Clock skew tolerance
    };

    let mtime_match = (local.mtime - remote.mtime).abs() < mtime_tolerance;

    if local.size == remote.size && mtime_match {
        // Paranoid mode: checksum first 4KB even if metadata matches
        if mode == Mode::Paranoid {
            if !quick_checksum_match(local, remote, 4_KB) {
                return Strategy::Delta;
            }
        }
        return Strategy::Skip;
    }

    // Resume: partial transfer exists
    if let Some(partial) = remote.partial_file() {
        if partial.checksum_valid() {
            return Strategy::Resume(partial.offset);
        }
    }

    // Delta: file exists but differs
    if remote.exists() && local.size > 1_MB {
        return Strategy::Delta;
    }

    // Full: new file or small file (delta overhead not worth it)
    Strategy::Full
}
```

**CLI flags**:
- `--ignore-times`: Force checksum comparison (like rsync)
- `--size-only`: Trust size, ignore mtime entirely
- `--checksum`: Always compare checksums (slow but thorough)

---

### 7. Progress & UX

**Decision**: Beautiful, informative progress output using `indicatif`.

**Design**:
```
Syncing ~/src → remote:/dest

[████████████████████----] 75% | 15.2 GB/s | ETA 12s
  ├─ config.json ✓
  ├─ database.db ⣾ (chunk 45/128, 156 MB/s)
  └─ videos/large.mp4 ⏸ (queued)

Files: 1,234 total | 892 synced | 312 skipped | 30 queued
```

**Error handling**:
```
✗ Permission denied: /var/log/secure
  Fix: Run with sudo or check file permissions (ls -la /var/log/secure)

✗ Network timeout: remote.example.com:22
  Fix: Check network connection and SSH access
```

---

## Technology Stack

### Core dependencies
```toml
[dependencies]
clap = "4"              # CLI parsing
tokio = "1"             # Async runtime
indicatif = "0.17"      # Progress bars
xxhash-rust = "0.8"     # Fast hashing
blake3 = "1"            # Cryptographic hashing
zstd = "0.13"           # Compression
lz4-flex = "0.11"       # Fast compression
walkdir = "2"           # Directory traversal
ignore = "0.4"          # gitignore support
serde = "1"             # Config serialization
toml = "0.8"            # Config format
```

### Testing & benchmarking
```toml
[dev-dependencies]
criterion = "0.5"       # Benchmarking
tempfile = "3"          # Test fixtures
proptest = "1"          # Property testing
```

---

## Reliability Considerations

### The Reality of "Reliability"

**No file transfer can be 100% reliable**. Sources of corruption:
1. **Transport layer**: TCP misses errors in 1/16M-10B packets
2. **Hardware**: Memory bitflips, bus errors, NIC bugs
3. **Network**: In-flight corruption not detected by checksums
4. **Storage**: Silent corruption, bit rot
5. **Physics**: Cosmic rays can flip bits

**Research finding** (2024): 5% of 100 Gbps file transfers develop corruption that TCP checksums fail to detect.

### Multi-Layer Defense Strategy

```rust
// Layer 1: Transport reliability (free, weak)
// - TCP checksum (99.99% detection rate)
// - TCP retransmission

// Layer 2: Block integrity (fast, good)
struct Block {
    data: Vec<u8>,
    checksum: u64,  // xxHash3 - detect in-flight corruption
}

// Layer 3: End-to-end verification (cryptographic, strong)
struct File {
    blocks: Vec<Block>,
    hash: [u8; 32],  // BLAKE3 - end-to-end integrity
}

// Layer 4: Verification modes
match mode {
    Fast => {
        // Trust transport + block checksums
        verify_blocks_xxhash3();
    },
    Standard => {
        // Spot check with cryptographic hash
        verify_blocks_xxhash3();
        verify_random_blocks_blake3(0.1);  // 10% sample
    },
    Verify => {
        // Cryptographic verification of all blocks
        verify_all_blocks_blake3();
    },
    Paranoid => {
        // Multiple passes + comparison reads
        verify_all_blocks_blake3();
        comparison_read_after_write();
        verify_all_blocks_blake3();  // Second pass
    },
}
```

### Error Recovery Mechanisms

1. **Block-level resume**: If block N fails, resume from block N (not entire file)
2. **Automatic retry**: Transient errors get 3 retries with exponential backoff
3. **Degradation**: Parallel transfer fails → fall back to sequential
4. **Corruption detection**: Block hash mismatch → re-transfer that block only
5. **Manifest verification**: After sync, verify all files exist with correct sizes/hashes

---

## Research References

### Papers & Articles
1. **"Data Synchronization: A Complete Theoretical Solution for Filesystems"** (2022, MDPI)
   - Theoretical analysis of filesystem synchronization
   - Declaration-based approach vs operation-based

2. **"Efficient File Synchronization: a Distributed Source Coding Approach"** (arXiv:1102.3669)
   - Multi-round protocols for bandwidth optimization
   - Trade-off: bandwidth vs latency

3. **"File Synchronization Systems Survey"** (arXiv:1611.05346)
   - Comprehensive survey of sync algorithms
   - rsync remains practical state-of-the-art

4. **"QUIC is not Quick Enough over Fast Internet"** (ACM Web Conference 2024)
   - QUIC 45% slower on high-bandwidth networks (>600 Mbps)
   - Performance gap increases with bandwidth

5. **"Avoiding data loss and corruption for file transfers"** (ScienceDirect 2021)
   - 5% of 100 Gbps transfers have corruption TCP doesn't detect
   - End-to-end checksums critical for reliability

### Performance Benchmarks (2024-2025)
- **rclone vs rsync**: 4x speedup with parallel transfers (Jeff Geerling, Jan 2025)
- **xxHash3 vs MD5**: 10x faster (10 GB/s vs 1 GB/s single-thread)
- **BLAKE3 vs SHA-2**: 10-15x faster, 3-16 GB/s (parallelizable)
- **QUIC vs TCP**: QUIC 45% slower on fast networks, better for high-latency + packet-loss
- **BBR vs CUBIC**: BBR 2-25x faster under packet loss, CUBIC better for stable LANs
- **zstd vs lz4**: Benchmarked at 8 GB/s (zstd) vs 23 GB/s (lz4), both faster than any network

### Tools Analyzed
- **rclone**: Multi-thread streams, parallel file transfers, cloud focus
- **Syncthing**: Block Exchange Protocol, SHA256 per block, TLS 1.3
- **LuminS**: Fast **local** file sync in Rust (Rayon parallelism)
- **rusync**: Minimalist Rust implementation
- **Mutagen**: Low-latency with filesystem watching

---

## Edge Cases & Design Decisions

### 1. Symlinks, Hardlinks, Special Files

**Symlink handling strategy**:
```rust
enum SymlinkMode {
    Preserve,        // Copy symlink as symlink (default, like rsync -l)
    Follow,          // Copy target, not link (like rsync -L)
    FollowUnsafe,    // Copy targets outside tree (like rsync --copy-unsafe-links)
    IgnoreUnsafe,    // Skip links outside tree (like rsync --safe-links)
}
```

**Hardlink preservation**:
```rust
struct HardlinkMap {
    inodes: HashMap<(DeviceId, InodeId), PathBuf>,
}

// Detect hardlinks by inode, recreate on destination
fn preserve_hardlink(file: &File, map: &mut HardlinkMap) -> Result<()> {
    let key = (file.dev_id, file.inode);
    if let Some(original) = map.get(&key) {
        // Create hardlink instead of copying
        fs::hard_link(original, file.path)?;
    } else {
        map.insert(key, file.path.clone());
        copy_file(file)?;
    }
}
```

**Special files** (devices, sockets, FIFOs):
```rust
enum FileType {
    Regular,
    Directory,
    Symlink,
    Hardlink,
    Device,     // Block/char devices
    Fifo,       // Named pipes
    Socket,     // Unix sockets
}

// Default: Skip special files with warning
// --specials flag: Attempt to recreate (requires root)
```

**CLI flags**:
- `--links` / `-l`: Preserve symlinks (default)
- `--copy-links` / `-L`: Follow and copy symlink targets
- `--hard-links` / `-H`: Preserve hard links
- `--specials`: Copy special files (devices, FIFOs, sockets)

---

### 2. Large-Scale Performance (Millions of Files)

**Problem**: rsync scans entire tree, stores all file metadata in memory
- 1M files ≈ 1-2GB RAM
- stat() calls dominate (IOPS bottleneck)
- rsync --delete scans entire destination

**Solution: Incremental scanning + batching**:
```rust
// Don't load all files into memory
async fn incremental_scan(src: &Path, dst: &Path) -> impl Stream<Item = SyncTask> {
    // Scan in batches
    const BATCH_SIZE: usize = 10_000;

    walkdir(src)
        .chunks(BATCH_SIZE)
        .map(|batch| {
            // Process batch: compare, queue transfers
            process_batch(batch, dst).await
        })
}

// Memory-efficient deletion
async fn delete_extra_files(src: &Path, dst: &Path) {
    // Use bloom filter for existence check (probabilistic, memory-efficient)
    let src_files = BloomFilter::from_dir(src);

    for entry in walkdir(dst) {
        if !src_files.might_contain(&entry.path) {
            // Double-check before delete
            if !src.join(&entry.path).exists() {
                delete(entry.path);
            }
        }
    }
}
```

**Optimizations**:
- Parallel directory traversal (rayon)
- Database/cache for previous sync state (skip unchanged dirs)
- Split large syncs into subdirectory batches
- XFS filesystem for large directories (better than ext4)

**CLI flags**:
- `--incremental`: Stream processing, low memory
- `--cache <file>`: Use state cache from previous sync
- `--max-memory <size>`: Memory budget limit

---

### 3. Atomic Operations & Crash Consistency

**Problem**: Crashes during sync can leave partial/corrupted files

**Strategy: Write to temp, then atomic rename**:
```rust
async fn transfer_file_atomic(src: &File, dst: &Path) -> Result<()> {
    let tmp = dst.with_extension(".sy.tmp");

    // 1. Transfer to temporary file
    transfer(src, &tmp).await?;

    // 2. Verify integrity
    if !verify_checksum(&tmp, src.hash) {
        fs::remove_file(&tmp)?;
        return Err(CorruptionError);
    }

    // 3. fsync data to disk
    let file = fs::OpenOptions::new().write(true).open(&tmp)?;
    file.sync_all()?;

    // 4. Atomic rename (crashes here are safe)
    fs::rename(&tmp, dst)?;

    // 5. fsync directory (ensures rename is persisted)
    let dir = fs::File::open(dst.parent().unwrap())?;
    dir.sync_all()?;

    Ok(())
}
```

**On crash/error**:
```rust
async fn cleanup_on_error() {
    // Find and remove all .sy.tmp files
    for tmp in glob("**/*.sy.tmp") {
        // Check if partial transfer is resumable
        if is_resumable(&tmp) {
            log::info!("Found resumable transfer: {}", tmp);
        } else {
            fs::remove_file(tmp)?;
        }
    }
}
```

**fsync considerations**:
- ext4 with `data=ordered`: fsync() forces delayed allocation
- btrfs: CoW means fsync() is expensive
- Mode: `--fsync always|auto|never`
  - `always`: Maximum durability (slow)
  - `auto`: fsync on remote, skip on local SSD
  - `never`: Speed over durability (risky)

---

### 4. Cross-Platform Filename Issues

**Problem**: Windows/Unix have incompatible filename rules

**Reserved characters**:
```rust
// Windows: \/:*?"<>|
// Unix: / and \0 only
// Cross-platform safe: alphanumeric + ._-

fn sanitize_filename(name: &str, target_os: OS) -> Result<String> {
    match target_os {
        OS::Windows => {
            // Check reserved names (CON, PRN, AUX, NUL, COM1-9, LPT1-9)
            if WINDOWS_RESERVED.contains(name.to_uppercase()) {
                return Err(ReservedNameError);
            }

            // Replace forbidden chars
            let sanitized = name
                .replace(['\\', '/', ':', '*', '?', '"', '<', '>', '|'], "_");

            // Remove trailing space/dot
            let sanitized = sanitized.trim_end_matches([' ', '.']);

            Ok(sanitized)
        }
        OS::Unix => {
            // Only / and \0 forbidden
            let sanitized = name.replace(['/', '\0'], "_");
            Ok(sanitized)
        }
    }
}
```

**Case sensitivity conflicts**:
```rust
// macOS: Case-insensitive by default
// Windows: Case-insensitive
// Linux: Case-sensitive

fn detect_case_conflict(files: &[Path]) -> Vec<Conflict> {
    let mut seen = HashMap::new();
    let mut conflicts = Vec::new();

    for file in files {
        let lower = file.to_lowercase();
        if let Some(existing) = seen.get(&lower) {
            conflicts.push(Conflict {
                file1: existing.clone(),
                file2: file.clone(),
            });
        } else {
            seen.insert(lower, file.clone());
        }
    }

    conflicts
}
```

**CLI flags**:
- `--sanitize-names`: Auto-fix forbidden characters
- `--detect-conflicts`: Error on case conflicts
- `--case-insensitive`: Treat paths as case-insensitive

---

### 5. Deletion Safety

**Problem**: `--delete` can accidentally wipe destination

**Multi-layer safety**:
```rust
enum DeleteMode {
    Never,           // Never delete (default)
    After,           // Delete after successful sync
    During,          // Delete during (faster but riskier)
    Excluded,        // Delete only files matching exclude pattern
}

async fn safe_delete(src: &Path, dst: &Path, mode: DeleteMode) -> Result<()> {
    // 1. Dry-run first (always)
    let to_delete = find_files_to_delete(src, dst);

    // 2. Safety checks
    if to_delete.len() > 1000 {
        eprintln!("WARNING: About to delete {} files", to_delete.len());
        if !confirm("Continue?")? {
            return Err(UserAborted);
        }
    }

    // 3. Percentage check (sanity)
    let dst_file_count = count_files(dst);
    let delete_percentage = to_delete.len() as f64 / dst_file_count as f64;

    if delete_percentage > 0.5 {
        return Err(Error::DeletionThresholdExceeded(
            "Refusing to delete >50% of destination files. Use --force-delete to override."
        ));
    }

    // 4. Trash instead of permanent delete (optional)
    if args.trash {
        for file in to_delete {
            move_to_trash(file)?;
        }
    } else {
        for file in to_delete {
            fs::remove_file(file)?;
        }
    }
}
```

**I/O error protection** (like rsync):
```rust
// If source has read errors, DON'T delete on destination
if source_io_errors.len() > 0 {
    log::warn!("I/O errors detected on source. Disabling --delete for safety.");
    delete_mode = DeleteMode::Never;
}
```

**CLI flags**:
- `--delete`: Delete extraneous files (requires confirmation if >1000)
- `--delete-during`: Delete during transfer (default: delete after)
- `--delete-threshold <percent>`: Max percentage to delete (default: 50%)
- `--trash`: Move to trash instead of permanent delete
- `--force-delete`: Skip safety checks (dangerous!)

---

### 6. Metadata Preservation

**Unix permissions & ownership**:
```rust
struct Metadata {
    mode: u32,           // chmod permissions (0755)
    uid: u32,            // Owner UID
    gid: u32,            // Group GID
    mtime: SystemTime,
    atime: SystemTime,   // Access time
}

async fn preserve_metadata(src: &File, dst: &Path) -> Result<()> {
    let meta = src.metadata;

    // Permissions (always preservable)
    fs::set_permissions(dst, meta.mode)?;

    // Owner/group (requires root or CAP_CHOWN)
    if is_root() {
        fs::chown(dst, meta.uid, meta.gid)?;
    } else {
        log::warn!("Not root, can't preserve owner:group for {}", dst);
    }

    // Times
    filetime::set_file_mtime(dst, meta.mtime)?;
    if args.preserve_atime {
        filetime::set_file_atime(dst, meta.atime)?;
    }
}
```

**Extended attributes (xattrs)**:
```rust
async fn preserve_xattrs(src: &Path, dst: &Path) -> Result<()> {
    let xattrs = xattr::list(src)?;

    for attr in xattrs {
        let value = xattr::get(src, &attr)?;

        // Namespace filtering (non-root can only write user.* namespace)
        if !is_root() && !attr.starts_with("user.") {
            log::warn!("Skipping privileged xattr: {}", attr);
            continue;
        }

        xattr::set(dst, &attr, &value)?;
    }
}
```

**ACLs (POSIX & NFSv4)**:
```rust
async fn preserve_acls(src: &Path, dst: &Path) -> Result<()> {
    // Get ACL from source
    let acl = acl::get_file(src)?;

    // Set ACL on destination
    acl::set_file(dst, acl)?;
}
```

**CLI flags**:
- `-p` / `--perms`: Preserve permissions (default in archive mode)
- `-o` / `--owner`: Preserve owner (requires root)
- `-g` / `--group`: Preserve group (requires root)
- `-t` / `--times`: Preserve modification times (default in archive mode)
- `-X` / `--xattrs`: Preserve extended attributes
- `-A` / `--acls`: Preserve ACLs
- `-a` / `--archive`: Equivalent to `-rlptgoD` (but NOT -X or -A)

---

### 7. Sparse Files

**Problem**: Copying sparse files (VM images) fills holes with zeros, wasting space/bandwidth

**Detection & handling**:
```rust
use std::os::unix::fs::MetadataExt;

fn is_sparse(file: &File) -> bool {
    let meta = file.metadata;
    // Sparse if allocated blocks < file size
    meta.blocks() * 512 < meta.len()
}

async fn transfer_sparse_file(src: &File, dst: &Path) -> Result<()> {
    if !is_sparse(src) {
        // Normal transfer
        return transfer_file(src, dst).await;
    }

    // Sparse-aware transfer
    let mut offset = 0;
    while offset < src.size {
        // Seek to next data region
        match lseek(src.fd, offset, SEEK_DATA) {
            Ok(data_start) => {
                // Find end of data region
                let data_end = lseek(src.fd, data_start, SEEK_HOLE)?;

                // Transfer only data region
                transfer_range(src, dst, data_start, data_end).await?;

                offset = data_end;
            }
            Err(_) => break,  // No more data
        }
    }

    // Punch holes in destination
    fallocate(dst.fd, FALLOC_FL_PUNCH_HOLE, ...)?;
}
```

**Incompatibility**:
- `--sparse` + `--inplace` conflict in old rsync (fixed in 3.2+)
- Not all filesystems support sparse files (FAT32, exFAT)

**CLI flags**:
- `-S` / `--sparse`: Handle sparse files efficiently
- `--no-sparse`: Disable sparse handling (compatibility)

---

### 8. Concurrent Modification & TOCTOU

**Problem**: Files changing during sync (TOCTOU race conditions)

**Detection strategy**:
```rust
async fn transfer_with_toctou_check(src: &File, dst: &Path) -> Result<()> {
    // 1. Check file metadata before transfer
    let meta_before = fs::metadata(src.path)?;

    // 2. Transfer file
    transfer_file(src, dst).await?;

    // 3. Check if file changed during transfer
    let meta_after = fs::metadata(src.path)?;

    if meta_before.mtime != meta_after.mtime ||
       meta_before.size != meta_after.size {
        log::warn!("File modified during transfer: {}", src.path);

        match args.on_change {
            OnChange::Error => return Err(ConcurrentModification),
            OnChange::Retry => return transfer_with_toctou_check(src, dst).await,
            OnChange::Skip => return Ok(()),
            OnChange::Warn => {
                log::warn!("Transfer may be inconsistent");
                return Ok(());
            }
        }
    }

    Ok(())
}
```

**File locking** (optional):
```rust
async fn transfer_with_lock(src: &File, dst: &Path) -> Result<()> {
    // Try to acquire shared lock on source
    let lock = match fs::File::open(src.path)?.try_lock_shared() {
        Ok(lock) => lock,
        Err(_) => {
            log::warn!("File locked, skipping: {}", src.path);
            return Ok(());
        }
    };

    transfer_file(src, dst).await?;

    drop(lock);
    Ok(())
}
```

**CLI flags**:
- `--on-change <action>`: How to handle files modified during transfer
  - `error`: Abort sync (safest)
  - `retry`: Re-transfer if changed (may loop)
  - `warn`: Warn but continue (default)
  - `skip`: Skip changed files

---

## Future Considerations

### Not in v1.0 (but worth considering)
- **Bidirectional sync** (like Syncthing)
- **Filesystem watching** (like Mutagen)
- **S3/cloud backends** (like rclone)
- **Encryption at rest**
- **Deduplication** (like restic/rustic)
- **bdsync integration**: For huge sparse files (VM images)

### Explicitly excluded
- **Daemon mode** - Keep it simple, focus on CLI
- **GUI** - Terminal UX only
- **Windows ACLs** - Start with POSIX, maybe later
- **Custom network protocol** - SSH is good enough, optimize it

---

## Additional Design Decisions

### 9. Filter/Exclude Patterns

**Decision**: Hybrid approach - gitignore syntax with rsync power features

**Gitignore pattern support**:
```rust
// Basic patterns
*.log              // All .log files
!important.log     // Negate: re-include this file
/foo               // Only at root
foo/               // Only directories
**/foo             // In any subdirectory

// Limitation: Can't re-include if parent excluded
dir/               // Excludes entire dir
!dir/file.txt      // WON'T WORK - parent excluded
```

**Rsync-style filters** (more powerful):
```rust
// First-match-wins (like iptables)
+ /important/      // Include this directory
- *.tmp            // Exclude temp files
+ /important/*.tmp // Include temps in important/ (overrides above)

// Merge files
. .syignore        // Load patterns from file

// Per-directory filters
: .syignore        // Scan each directory for .syignore
```

**Implementation**:
```rust
struct FilterRule {
    pattern: glob::Pattern,
    action: Action,      // Include | Exclude
    priority: usize,     // For precedence
    anchor: Anchor,      // Root | Anywhere | Directory
}

struct FilterEngine {
    rules: Vec<FilterRule>,
    gitignore_mode: bool,  // true = gitignore semantics, false = rsync
}

fn matches(path: &Path, rules: &[FilterRule]) -> Action {
    if gitignore_mode {
        // Parent excluded = children excluded (can't negate)
        check_parent_excluded(path, rules)?;
    }

    // First match wins
    for rule in rules {
        if rule.pattern.matches_path(path) {
            return rule.action;
        }
    }

    Action::Include  // Default: include
}
```

**CLI flags**:
- `--exclude <pattern>`: Exclude pattern
- `--include <pattern>`: Include pattern
- `--exclude-from <file>`: Load excludes from file
- `--filter <rule>`: Advanced rsync-style filter
- `--gitignore`: Use .gitignore files (default in git repos)
- `--filter-mode <mode>`: `gitignore` | `rsync` (default: auto-detect)

---

### 10. Bandwidth Limiting

**Decision**: Token bucket algorithm with per-worker budgets

**Algorithm**:
```rust
struct TokenBucket {
    capacity: usize,      // Max burst size (bytes)
    tokens: AtomicUsize,  // Current tokens
    refill_rate: usize,   // Bytes per second
    last_refill: Instant,
}

impl TokenBucket {
    async fn take(&mut self, bytes: usize) -> Result<()> {
        loop {
            // Refill tokens based on elapsed time
            let now = Instant::now();
            let elapsed = now - self.last_refill;
            let new_tokens = (elapsed.as_secs_f64() * self.refill_rate as f64) as usize;

            self.tokens.fetch_add(new_tokens.min(self.capacity), Ordering::Relaxed);
            self.last_refill = now;

            // Try to consume tokens
            let current = self.tokens.load(Ordering::Relaxed);
            if current >= bytes {
                self.tokens.fetch_sub(bytes, Ordering::Relaxed);
                return Ok(());
            }

            // Wait for refill
            let wait_time = Duration::from_secs_f64(
                (bytes - current) as f64 / self.refill_rate as f64
            );
            tokio::time::sleep(wait_time).await;
        }
    }
}
```

**Parallel transfer coordination**:
```rust
// Global bucket shared across workers
static BANDWIDTH_LIMITER: OnceCell<Arc<TokenBucket>> = OnceCell::new();

async fn transfer_chunk(chunk: &[u8]) -> Result<()> {
    // Acquire tokens before transfer
    BANDWIDTH_LIMITER.get().unwrap().take(chunk.len()).await?;

    // Perform transfer
    send_data(chunk).await?;

    Ok(())
}
```

**Configuration**:
- Bucket capacity = 2x bandwidth limit (allows bursts)
- Refill rate = bandwidth limit (bytes/sec)
- Applied after compression (limits network traffic, not file size)

**CLI flags**:
- `--bandwidth <rate>`: Limit in bytes/sec (e.g., `10M`, `1G`, `500K`)
- `--bwlimit <rate>`: Alias for rsync compatibility
- `--no-bandwidth-limit`: Disable (default)

---

### 11. Progress Reporting at Scale

**Decision**: Hierarchical display with aggregation for millions of files

**Design for different scales**:

```rust
// Small scale (< 1K files): Show all files
Files: 234/500 (46%)
├─ config.json ✓
├─ database.db ⣾ (chunk 45/128, 156 MB/s)
└─ video.mp4 ⏸ (queued)

// Medium scale (1K - 100K files): Aggregate by directory
Syncing 45,231 files...
├─ /src: 1,234/2,000 (61%) ████████████░░░░
├─ /docs: 892/1,500 (59%) ███████████░░░░░
└─ /data: 34,521/41,731 (82%) █████████████░░

// Large scale (> 100K files): Summary only
Synced: 1.2M / 3.4M files (35%)
Rate: 15,432 files/sec | 2.3 GB/s
ETA: 2m 34s

Active transfers: 8
├─ large_db.sql: 45% (2.1 GB/s)
├─ video1.mp4: 78% (890 MB/s)
└─ ... (6 more)
```

**Implementation**:
```rust
enum ProgressMode {
    Detailed,    // < 1K files: show individual files
    Directory,   // 1K-100K files: aggregate by directory
    Summary,     // > 100K files: stats only
}

struct ProgressTracker {
    mode: ProgressMode,
    total_files: usize,
    completed_files: AtomicUsize,
    total_bytes: u64,
    transferred_bytes: AtomicU64,
    active_transfers: DashMap<PathBuf, TransferProgress>,
}

// Update every 100ms or every 1000 files (whichever is less frequent)
fn should_update(&self) -> bool {
    let elapsed = self.last_update.elapsed();
    let files_delta = self.completed_files.load() - self.last_file_count;

    elapsed > Duration::from_millis(100) || files_delta > 1000
}
```

**Features**:
- ETA calculation using exponential moving average
- Transfer rate smoothing (avoid jitter)
- Graceful fallback on terminal resize
- JSON output mode for programmatic parsing

**CLI flags**:
- `--progress`: Show progress (auto in TTY)
- `--no-progress`: Disable progress display
- `--progress-mode <mode>`: Force mode: `detailed|directory|summary|json`
- `-q` / `--quiet`: Only show errors

---

### 12. SSH Configuration Integration

**Decision**: Parse `~/.ssh/config` for seamless integration

**Supported directives**:
```rust
struct SshConfig {
    hostname: String,
    port: u16,
    user: String,
    identity_file: Vec<PathBuf>,
    proxy_jump: Option<String>,
    proxy_command: Option<String>,
    // Future: ControlMaster fields (requires OpenSSH, not ssh2)
    control_master: ControlMasterMode,   // Parsed but not used
    control_path: PathBuf,                // Parsed but not used
    control_persist: Duration,            // Parsed but not used
    compression: bool,
}

fn parse_ssh_config(host: &str) -> Result<SshConfig> {
    // 1. Parse ~/.ssh/config
    let config = ssh_config::parse("~/.ssh/config")?;

    // 2. Match host patterns (most specific first)
    let host_config = config.query(host);

    // 3. Apply defaults
    SshConfig {
        hostname: host_config.hostname.unwrap_or(host),
        port: host_config.port.unwrap_or(22),
        user: host_config.user.unwrap_or_else(|| whoami::username()),
        identity_file: host_config.identity_file,
        proxy_jump: host_config.proxy_jump,
        control_master: host_config.control_master.unwrap_or(Auto),
        control_path: host_config.control_path
            .unwrap_or("~/.ssh/sockets/%r@%h-%p".into()),
        control_persist: host_config.control_persist
            .unwrap_or(Duration::from_secs(600)),  // 10 minutes
        ..Default::default()
    }
}
```

**ProxyJump handling**:
```rust
async fn connect_with_proxy(config: &SshConfig) -> Result<SshSession> {
    if let Some(jump_host) = &config.proxy_jump {
        // Recursive: parse config for jump host
        let jump_config = parse_ssh_config(jump_host)?;
        let jump_session = connect(&jump_config).await?;

        // Tunnel through jump host
        let channel = jump_session.channel_direct_tcpip(
            &config.hostname,
            config.port,
            None,
        )?;

        SshSession::new_from_channel(channel).await
    } else {
        // Direct connection
        SshSession::connect(&config.hostname, config.port).await
    }
}
```

**ControlMaster optimization** (future - requires OpenSSH, not ssh2):
```rust
// NOTE: Not currently implemented - ssh2 library doesn't support ControlMaster
// Would require using OpenSSH command-line tool instead of ssh2 library
// Potential 2.5x throughput improvement for future versions

async fn get_or_create_session(config: &SshConfig) -> Result<SshSession> {
    let socket_path = expand_control_path(&config.control_path, config);

    // Try existing socket
    if socket_path.exists() {
        if let Ok(session) = SshSession::from_socket(&socket_path).await {
            return Ok(session);
        }
    }

    // Create new session with ControlMaster
    let session = connect(config).await?;
    session.enable_control_master(&socket_path, config.control_persist)?;

    Ok(session)
}
```

**CLI flags**:
- `--ssh-config <file>`: Use alternate SSH config
- `--no-ssh-config`: Ignore SSH config
- `-i <key>`: Identity file (overrides config)
- `-p <port>`: Port (overrides config)

---

### 13. Error Handling Strategy

**Decision**: Threshold-based with collection and categorization

**Error categories**:
```rust
enum ErrorSeverity {
    Warning,     // Non-fatal, can continue
    Error,       // File failed, continue with others
    Fatal,       // Must abort entire sync
}

enum ErrorCategory {
    Permission,      // Can't read/write
    NotFound,        // File disappeared
    Corruption,      // Checksum mismatch
    Network,         // Connection issues
    DiskFull,        // No space
    Interrupted,     // User canceled or signal
}

struct SyncError {
    path: PathBuf,
    category: ErrorCategory,
    severity: ErrorSeverity,
    message: String,
    retryable: bool,
}
```

**Threshold configuration**:
```rust
struct ErrorPolicy {
    max_errors: Option<usize>,           // Total errors before abort (None = unlimited)
    max_error_rate: Option<f64>,         // Error rate threshold (0.0-1.0)
    fatal_on: HashSet<ErrorCategory>,    // Categories that abort immediately
    retry_transient: bool,               // Auto-retry network/temp errors
    retry_count: usize,                  // Max retries per file
}

impl Default for ErrorPolicy {
    fn default() -> Self {
        Self {
            max_errors: Some(1000),           // Stop after 1K errors
            max_error_rate: Some(0.05),       // Stop if >5% failure rate
            fatal_on: [Corruption, Interrupted].into(),
            retry_transient: true,
            retry_count: 3,
        }
    }
}
```

**Error collection & reporting**:
```rust
struct ErrorCollector {
    errors: Vec<SyncError>,
    warnings: Vec<SyncError>,
    policy: ErrorPolicy,
}

impl ErrorCollector {
    fn should_abort(&self, total_files: usize) -> bool {
        // Check total error count
        if let Some(max) = self.policy.max_errors {
            if self.errors.len() >= max {
                return true;
            }
        }

        // Check error rate
        if let Some(max_rate) = self.policy.max_error_rate {
            let rate = self.errors.len() as f64 / total_files as f64;
            if rate > max_rate {
                return true;
            }
        }

        // Check for fatal errors
        self.errors.iter().any(|e| {
            e.severity == ErrorSeverity::Fatal ||
            self.policy.fatal_on.contains(&e.category)
        })
    }

    fn report(&self) {
        // Group errors by category
        let mut by_category: HashMap<ErrorCategory, Vec<&SyncError>> = HashMap::new();
        for err in &self.errors {
            by_category.entry(err.category).or_default().push(err);
        }

        // Report summary
        eprintln!("\nSync completed with {} errors, {} warnings:",
                  self.errors.len(), self.warnings.len());

        for (category, errs) in by_category {
            eprintln!("  {:?}: {} files", category, errs.len());

            // Show first 5 examples
            for err in errs.iter().take(5) {
                eprintln!("    - {}: {}", err.path.display(), err.message);
            }

            if errs.len() > 5 {
                eprintln!("    ... and {} more", errs.len() - 5);
            }
        }
    }
}
```

**CLI flags**:
- `--max-errors <n>`: Stop after N errors (default: 1000, 0 = unlimited)
- `--max-error-rate <rate>`: Stop if error rate exceeds (default: 0.05 = 5%)
- `--strict`: Abort on first error
- `--continue-on-error`: Never abort due to errors (dangerous!)
- `--retry <n>`: Retry count for transient errors (default: 3)
- `--error-log <file>`: Write errors to file

---

### 14. Logging & Observability

**Decision**: Structured logging with `tracing` crate

**Logging levels**:
```rust
use tracing::{trace, debug, info, warn, error};

// trace: Very detailed (every block transfer)
trace!(path = %file.path, offset = %offset, size = %size, "Transferring block");

// debug: Debugging info (decisions made)
debug!(path = %file.path, strategy = ?strategy, "Selected transfer strategy");

// info: Important events (file completed)
info!(path = %file.path, size = %file.size, duration_ms = %elapsed, "File synced");

// warn: Non-fatal issues (can't preserve metadata)
warn!(path = %file.path, "Not root, can't preserve owner:group");

// error: Failures (transfer failed)
error!(path = %file.path, err = %e, "Transfer failed");
```

**Structured output formats**:
```rust
// Human-readable (default for TTY)
2025-01-15T10:23:45.123Z  INFO sync: File synced path="/data/large.db" size=1073741824 duration_ms=523

// JSON (for log aggregation)
{"timestamp":"2025-01-15T10:23:45.123Z","level":"INFO","target":"sy::sync","path":"/data/large.db","size":1073741824,"duration_ms":523,"message":"File synced"}

// Compact (for CI/scripts)
[INFO] File synced: /data/large.db (1.0 GB in 523ms)
```

**Tracing spans** (for async context):
```rust
#[instrument(skip(src, dst))]
async fn sync_directory(src: &Path, dst: &Path) -> Result<()> {
    let span = tracing::info_span!("sync_dir", path = %src.display());
    let _guard = span.enter();

    // All logs within this scope automatically include path context
    info!("Starting directory sync");

    for entry in read_dir(src)? {
        sync_file(&entry, dst).await?;  // Nested span
    }

    info!("Directory sync complete");
    Ok(())
}
```

**CLI flags**:
- `-v` / `--verbose`: Increase verbosity (can be repeated: `-vv`, `-vvv`)
  - Default: info
  - `-v`: debug
  - `-vv`: trace
- `--log-format <format>`: Output format: `human|json|compact` (default: auto)
- `--log-file <file>`: Write logs to file
- `--log-level <level>`: Set log level: `error|warn|info|debug|trace`

---

### 15. Additional Considerations

**Dry-run implementation**:
```rust
struct DryRunPlanner {
    files_to_create: Vec<PathBuf>,
    files_to_update: Vec<PathBuf>,
    files_to_delete: Vec<PathBuf>,
    bytes_to_transfer: u64,
    estimated_time: Duration,
}

async fn plan_sync(src: &Path, dst: &Path) -> DryRunPlanner {
    // Simulate entire sync without I/O
    let mut planner = DryRunPlanner::default();

    for file in scan_source(src).await {
        match transfer_strategy(&file, dst) {
            Strategy::Full => {
                planner.files_to_create.push(file.path);
                planner.bytes_to_transfer += file.size;
            }
            Strategy::Delta => {
                planner.files_to_update.push(file.path);
                planner.bytes_to_transfer += estimate_delta_size(&file);
            }
            Strategy::Skip => {}
        }
    }

    if args.delete {
        planner.files_to_delete = find_extra_files(src, dst).await;
    }

    planner.estimated_time = estimate_transfer_time(
        planner.bytes_to_transfer,
        detect_network(dst).await.bandwidth
    );

    planner
}
```

**Signal handling**:
```rust
async fn setup_signal_handlers() {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = sigint.recv() => {
            warn!("Received SIGINT, graceful shutdown...");
            SHUTDOWN_FLAG.store(true, Ordering::Relaxed);
        }
        _ = sigterm.recv() => {
            warn!("Received SIGTERM, graceful shutdown...");
            SHUTDOWN_FLAG.store(true, Ordering::Relaxed);
        }
    }
}

// Cleanup on shutdown
async fn graceful_shutdown() {
    info!("Cleaning up temporary files...");
    cleanup_temp_files().await?;

    info!("Closing connections...");
    close_all_connections().await?;

    // Write resume state
    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        save_resume_state().await?;
    }
}
```

**Exit codes**:
```rust
enum ExitCode {
    Success = 0,
    GeneralError = 1,
    SyntaxError = 2,          // Invalid CLI args
    ProtocolError = 5,        // SSH/network protocol issue
    FileNotFound = 23,        // Source file(s) not found
    PartialTransfer = 24,     // Some files transferred, some failed
    Interrupted = 130,        // SIGINT (128 + 2)
    TermReceived = 143,       // SIGTERM (128 + 15)
}
```

**Resource limits checking**:
```rust
async fn check_resources(dst: &Path, bytes_needed: u64) -> Result<()> {
    // Disk space
    let available = fs2::available_space(dst)?;
    if available < bytes_needed * 110 / 100 {  // 10% buffer
        return Err(Error::InsufficientSpace {
            needed: bytes_needed,
            available,
        });
    }

    // File descriptor limit
    let (soft, hard) = getrlimit(Resource::NOFILE)?;
    let needed_fds = args.workers * 10;  // Estimate

    if needed_fds > soft as usize {
        warn!("May hit file descriptor limit ({} < {})", soft, needed_fds);
        info!("Consider: ulimit -n {}", hard);
    }

    Ok(())
}
```

**Deduplication within sync**:
```rust
struct ContentMap {
    // Map hash -> first path with this content
    seen: HashMap<Blake3Hash, PathBuf>,
}

async fn deduplicate_transfers(files: Vec<File>) -> Vec<SyncTask> {
    let mut map = ContentMap::default();
    let mut tasks = Vec::new();

    for file in files {
        let hash = quick_hash(&file, 1_MB).await?;  // Hash first 1MB

        if let Some(original) = map.seen.get(&hash) {
            // Same content, create hardlink instead of transfer
            tasks.push(SyncTask::Hardlink {
                from: original.clone(),
                to: file.path,
            });
        } else {
            map.seen.insert(hash, file.path.clone());
            tasks.push(SyncTask::Transfer(file));
        }
    }

    tasks
}
```

---

## Testing Strategy

### Unit tests
- Hash function correctness (xxHash3, BLAKE3, Adler-32 rolling)
- Compression selection logic
- File skip/resume decisions
- Parallel chunk coordination
- Filter pattern matching (gitignore & rsync styles)
- Token bucket bandwidth limiting
- SSH config parsing

### Integration tests
- Full sync scenarios (new files, updated, deleted)
- Resume interrupted transfers
- Compression with various file types
- Error handling (permissions, network)
- Symlink/hardlink preservation
- Sparse file handling
- Cross-platform filename sanitization
- Concurrent modification detection

### Benchmarks
- Hash speed (xxHash3 vs BLAKE3 vs SHA-256 vs Adler-32)
- Compression (zstd vs lz4 vs none)
- Parallel vs sequential transfers
- Delta sync vs full transfer
- Directory traversal at scale (1M+ files)
- Memory usage under different scenarios

### Property tests
- Sync idempotence (sync twice = same result)
- Compression roundtrip (compress + decompress = original)
- Hash collision resistance
- Filter rule ordering invariants
- Bandwidth limiting maintains rate

### Stress tests
- Millions of small files (1-10MB each)
- Few huge files (>100GB sparse files)
- Deep directory hierarchies (>1000 levels)
- Filenames with unicode, special characters
- Network interruption recovery
- Disk full scenarios
- OOM conditions (limited memory)

---

## Adaptive Performance Modes

**Core innovation**: Auto-detect environment and optimize strategy

```rust
enum SyncMode {
    // Auto-detect (default): Measure network, choose strategy
    Auto,

    // Local: Maximum parallelism, no compression, no delta
    Local {
        workers: usize,           // num_cpus * 2
        use_kernel_copy: true,    // copy_file_range, clonefile
        compression: None,
        delta: false,             // Full copy faster locally
    },

    // LAN: Parallel transfers, minimal compression, selective delta
    Lan {
        workers: usize,           // bandwidth / 100mbps
        compression: Lz4,         // Only if >1Gbps
        delta: true,
        congestion: CUBIC,
    },

    // WAN: Delta sync, adaptive compression, BBR congestion control
    Wan {
        workers: 4,               // Conservative for stability
        compression: ZstdAdaptive,
        delta: true,
        congestion: BBR,
        resume: true,
    },

    // Verify: Cryptographic checksums, paranoid checks
    Verify {
        hash: BLAKE3,
        check_blocks: All,
        comparison_read: true,    // Read back after write
        multiple_passes: 2,
    },

    // Paranoid: Maximum reliability, minimum speed
    Paranoid {
        hash: BLAKE3,
        check_blocks: All,
        comparison_read: true,
        multiple_passes: 3,
        ignore_times: true,       // Always checksum
        verify_metadata: true,
    },
}
```

**Network Detection Logic**:
```rust
async fn detect_mode(src: &Path, dst: &Path) -> SyncMode {
    // Local filesystem check
    if is_same_filesystem(src, dst) || is_local_path(dst) {
        return SyncMode::Local;
    }

    // Network profiling
    let profile = detect_network(dst).await;

    match (profile.bandwidth, profile.latency, profile.packet_loss) {
        // LAN: high bandwidth, low latency, stable
        (>100Mbps, <10ms, <0.1%) => SyncMode::Lan,

        // WAN: anything else
        _ => SyncMode::Wan,
    }
}
```

---

## CLI Reference (Planned)

```
sy - Modern file synchronization tool

USAGE:
    sy [OPTIONS] <SOURCE> <DESTINATION>
    sy [CONFIG_NAME]

MODES (mutually exclusive):
    --mode <MODE>          Strategy: auto|local|lan|wan|verify|paranoid (default: auto)

OPTIONS:
    -n, --dry-run          Preview changes without applying
    -d, --delete           Delete files not in source
    -w, --workers <N>      Parallel workers (default: auto)
    -c, --compress <TYPE>  Compression: zstd|lz4|none|auto (default: auto)
    -v, --verbose          Verbose output
    -q, --quiet            Minimal output

INTEGRITY:
    --verify               Cryptographic verification (BLAKE3 all blocks)
    --paranoid             Maximum integrity checks (slow)
    --checksum             Always compare checksums (ignore mtime)
    --ignore-times         Force checksum comparison
    --size-only            Trust size, ignore time

ADVANCED:
    --bandwidth <RATE>     Limit bandwidth (e.g., 10M, 1G)
    --chunk-size <SIZE>    Block size (default: adaptive)
    --protocol <PROTO>     Force protocol: ssh|sftp|local
    --congestion <ALG>     TCP congestion: bbr|cubic|auto
    --config <PATH>        Use config file

EXAMPLES:
    # Auto-detect and sync
    sy ./src remote:/dest

    # Local copy (maximum speed)
    sy ./src /backup --mode local

    # WAN sync (compression + delta)
    sy ./src remote:/dest --mode wan

    # Verify integrity
    sy ./src remote:/dest --verify

    # Preview changes
    sy ./src remote:/dest --dry-run

    # Use named config
    sy backup-docs
```

---

## Key Takeaways & Design Principles

### 1. **No Single "Best" Strategy**
Different scenarios need different approaches:
- **Local**: Parallelism + kernel optimizations (no compression, no delta)
- **LAN**: Parallel transfers + minimal delta (LZ4 only if >1Gbps)
- **WAN**: Delta sync + compression + BBR (resilience over raw speed)

### 2. **Reliability is Multi-Layer**
Transport layer (TCP) is NOT enough:
- xxHash3 for fast block verification
- BLAKE3 for cryptographic end-to-end integrity
- Multiple verification modes for different trust levels
- Block-level resume on corruption

### 3. **Protocol Surprises**
Research contradicts common assumptions:
- ❌ QUIC is **slower** on fast networks (45% reduction >600 Mbps)
- ✅ TCP with BBR beats CUBIC under packet loss (2-25x faster)
- ⏳ SSH ControlMaster (2.5x boost) requires OpenSSH - not available in ssh2 library
- ❌ Custom protocols add complexity; optimize standard ones first

### 4. **Hash Function Roles**
Different hashes for different purposes:
- **Adler-32**: Rolling hash (delta sync algorithm) - NOT replaceable
- **xxHash3**: Block checksums (fast verification) - can't roll
- **BLAKE3**: End-to-end integrity (cryptographic) - slow but secure

### 5. **mtime is Unreliable**
Size + mtime is a heuristic, not truth:
- FAT32/exFAT: 2-second granularity
- NFS: Timezone issues, caching delays
- Always need tolerance windows
- Paranoid mode: checksum even if metadata matches

### 6. **Compression Thresholds Benchmarked**
Revised based on actual Rust benchmarks (2024+ hardware):
- **LZ4**: 23 GB/s throughput (184 Gbps) - 50x faster than originally assumed
- **Zstd level 3**: 8 GB/s throughput (64 Gbps) - 16x faster than assumed
- **Network**: Always compress (CPU never bottleneck, even on 100 Gbps networks)
- **Local**: Never compress (disk I/O bottleneck, not CPU/network)

### 7. **Block Size is Adaptive**
One size doesn't fit all:
- Small files (<1MB): 64KB blocks
- Medium files (1-100MB): 256KB blocks
- Large files (100MB-1GB): 1MB blocks
- Huge files (>1GB): 4MB blocks

### 8. **UX is Critical**
Learn from `eza`, `fd`, `ripgrep`:
- Auto-detection by default (--mode auto)
- Clear, simple flags (--verify, --paranoid)
- Beautiful progress output
- Helpful error messages with fixes
- Named configs for common tasks

---

This design balances performance, usability, and maintainability. Focus is on doing fewer things really well rather than feature bloat.

The goal isn't to be "100% reliable and as fast as possible" - that's physically impossible. Instead: **Verifiable integrity, adaptive performance, and transparent tradeoffs**.

---

## Configuration File Format

**Decision**: TOML for human-friendliness

```toml
# ~/.config/sy/config.toml

# Global defaults
[defaults]
mode = "auto"              # auto|local|lan|wan|verify|paranoid
workers = 0                # 0 = auto-detect
compress = "auto"          # auto|zstd|lz4|none
verify = false
delete = false
gitignore = true

# Named sync profiles
[[sync]]
name = "backup-home"
source = "~/"
destination = "backup-server:/mnt/backups/home"
exclude = [
    ".cache/**",
    "*.tmp",
    "node_modules/**",
]
delete = true
mode = "wan"

[[sync]]
name = "media-sync"
source = "~/Pictures"
destination = "nas:/media/photos"
compress = false           # Already compressed
bandwidth = "50M"          # Limit to 50 MB/s
preserve = ["permissions", "times"]  # Not owner/group

[[sync]]
name = "code-deploy"
source = "./dist"
destination = "prod-server:/var/www/app"
mode = "verify"            # Cryptographic checksums
delete = true
exclude-from = ".deployignore"

# SSH connection profiles
[ssh]
config = "~/.ssh/config"   # Parse SSH config
# control-master = true    # Future: ControlMaster (requires OpenSSH, not ssh2)
# control-persist = "10m"  # Future: Keep connections alive

# Logging configuration
[logging]
level = "info"             # error|warn|info|debug|trace
format = "auto"            # auto|human|json|compact
file = "~/.local/share/sy/sy.log"

# Error handling
[errors]
max-errors = 1000
max-error-rate = 0.05      # 5%
retry-transient = true
retry-count = 3
```

**Usage**:
```bash
# Use named profile
sy backup-home

# Override config settings
sy backup-home --mode verify --workers 16

# Specify alternate config
sy --config ~/work-sync.toml code-deploy
```

---

## Project Structure & Dependencies

### Module organization
```
sy/
├── src/
│   ├── main.rs                 # CLI entry point
│   ├── cli.rs                  # Argument parsing (clap)
│   ├── config.rs               # Config file parsing
│   │
│   ├── sync/
│   │   ├── mod.rs              # Sync orchestration
│   │   ├── scanner.rs          # Directory traversal
│   │   ├── strategy.rs         # Transfer strategy selection
│   │   ├── transfer.rs         # File transfer logic
│   │   ├── delta.rs            # Rsync algorithm
│   │   └── resume.rs           # Resume logic
│   │
│   ├── integrity/
│   │   ├── mod.rs
│   │   ├── hash.rs             # xxHash3, BLAKE3, Adler-32
│   │   ├── checksum.rs         # Block-level checksums
│   │   └── verify.rs           # Verification modes
│   │
│   ├── transport/
│   │   ├── mod.rs
│   │   ├── local.rs            # Local filesystem
│   │   ├── ssh.rs              # SSH custom protocol
│   │   ├── sftp.rs             # SFTP fallback
│   │   └── network.rs          # Network detection
│   │
│   ├── compress/
│   │   ├── mod.rs
│   │   ├── zstd.rs             # Zstandard
│   │   ├── lz4.rs              # LZ4
│   │   └── adaptive.rs         # Compression selection
│   │
│   ├── filter/
│   │   ├── mod.rs
│   │   ├── gitignore.rs        # Gitignore parser
│   │   ├── rsync.rs            # Rsync filter rules
│   │   └── engine.rs           # Filter matching engine
│   │
│   ├── progress/
│   │   ├── mod.rs
│   │   ├── tracker.rs          # Progress tracking
│   │   ├── display.rs          # Terminal UI
│   │   └── eta.rs              # ETA calculation
│   │
│   ├── metadata/
│   │   ├── mod.rs
│   │   ├── permissions.rs      # Unix permissions
│   │   ├── xattr.rs            # Extended attributes
│   │   └── acl.rs              # Access control lists
│   │
│   ├── error.rs                # Error types
│   ├── bandwidth.rs            # Token bucket rate limiting
│   └── ssh_config.rs           # SSH config parsing
│
├── tests/
│   ├── integration/            # Integration tests
│   ├── property/               # Property tests (proptest)
│   └── stress/                 # Stress tests
│
├── benches/                    # Criterion benchmarks
├── docs/                       # User documentation
├── Cargo.toml
└── DESIGN.md                   # This file
```

### Core dependencies
```toml
[dependencies]
# CLI & Config
clap = { version = "4", features = ["derive", "env"] }
toml = "0.8"
serde = { version = "1", features = ["derive"] }

# Async runtime
tokio = { version = "1", features = ["full"] }
futures = "0.3"

# Hashing
xxhash-rust = "0.8"
blake3 = { version = "1", features = ["rayon"] }

# Compression
zstd = "0.13"
lz4-flex = "0.11"

# SSH/Network
russh = "0.44"                  # SSH protocol
russh-sftp = "2"                # SFTP
ssh-config = "0.1"              # SSH config parsing

# Filesystem
walkdir = "2"
ignore = "0.4"                  # Gitignore support
filetime = "0.2"
xattr = "1"                     # Extended attributes

# Progress & Logging
indicatif = "0.17"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }

# Error handling
anyhow = "1"
thiserror = "1"

# Utilities
dashmap = "6"                   # Concurrent HashMap
rayon = "1.10"                  # Parallel iterators
once_cell = "1"

[dev-dependencies]
criterion = "0.5"               # Benchmarking
tempfile = "3"                  # Test fixtures
proptest = "1"                  # Property testing
mockall = "0.13"                # Mocking
```

---

## Security Considerations

### Dependency auditing
```bash
# Regular security audits
cargo audit

# Check for outdated dependencies
cargo outdated

# Supply chain security
cargo-deny check advisories
```

### Threat model

**In scope:**
- ✅ Data integrity (checksums prevent corruption)
- ✅ Man-in-the-middle (SSH encryption)
- ✅ Denial of service (bandwidth limiting, resource checks)
- ✅ Path traversal (sanitize paths, check bounds)
- ✅ Symlink attacks (configurable symlink handling)

**Out of scope (v1.0):**
- ❌ Encryption at rest (use LUKS/dm-crypt on destination)
- ❌ Authentication (delegated to SSH)
- ❌ Authorization (filesystem permissions)
- ❌ Side-channel attacks (timing attacks on checksums)

### Safe defaults
```rust
// Never follow symlinks outside tree by default
symlink_mode: SymlinkMode::IgnoreUnsafe,

// Never delete by default
delete: false,

// Always verify checksums on cryptographic modes
verify_checksums: mode != Mode::Fast,

// Reject operations on suspicious paths
fn validate_path(path: &Path) -> Result<()> {
    // No absolute symlinks
    // No parent directory references outside tree
    // No special files unless --specials
}
```

---

## Implementation Roadmap

### Phase 1: MVP (v0.1.0)
**Goal**: Basic local sync working

- [ ] CLI argument parsing
- [ ] Local filesystem traversal
- [ ] File comparison (size + mtime)
- [ ] Full file copy (no delta)
- [ ] Basic progress display
- [ ] Unit tests

**Deliverable**: `sy /src /dst` works locally

---

### Phase 2: Network Sync (v0.2.0)
**Goal**: Remote sync via SSH

- [ ] SSH transport layer
- [ ] SFTP fallback
- [ ] Network bandwidth detection
- [ ] SSH config parsing
- [ ] Basic error handling

**Deliverable**: `sy /src remote:/dst` works

---

### Phase 3: Performance (v0.3.0)
**Goal**: Parallel transfers

- [ ] Parallel file transfers
- [ ] Parallel chunk transfers
- [ ] Adaptive compression
- [ ] Network detection (LAN vs WAN)
- [ ] Progress UI at scale

**Implementation techniques**:
- [ ] Parallel scanning with rayon (scan source and destination concurrently)
- [ ] Parallel file operations with rayon (concurrent file copies)
- [ ] Memory-mapped I/O for very large files (>100MB)
- [ ] Async I/O with tokio for network operations

**Deliverable**: Fast sync for various scenarios

---

### Phase 4: Delta Sync (v0.4.0)
**Goal**: Rsync algorithm

- [ ] Adler-32 rolling hash
- [ ] Block signature generation
- [ ] Delta computation
- [ ] Resume support

**Deliverable**: Efficient updates of changed files

---

### Phase 5: Reliability (v0.5.0)
**Goal**: Multi-layer integrity

- [ ] Block-level checksums (xxHash3)
- [ ] End-to-end verification (BLAKE3)
- [ ] Verification modes
- [ ] Atomic operations
- [ ] Crash recovery

**Deliverable**: Verifiable integrity

---

### Phase 6: Advanced Features (v0.6.0)
**Goal**: Edge cases & polish

- [ ] Symlink/hardlink handling
- [ ] Sparse file support
- [ ] Extended attributes
- [ ] ACLs
- [ ] Cross-platform filenames
- [ ] Filter patterns (gitignore + rsync)

**Deliverable**: Production-ready edge case handling

---

### Phase 7: Scale (v0.7.0)
**Goal**: Millions of files

- [ ] Incremental scanning
- [ ] Memory-efficient deletion
- [ ] Bloom filters
- [ ] State caching
- [ ] Deduplication

**Deliverable**: Handle extreme scale

---

### Phase 8: Polish (v0.8.0)
**Goal**: UX refinement

- [ ] Bandwidth limiting
- [ ] Error thresholds
- [ ] Dry-run mode
- [ ] Config file support
- [ ] Structured logging
- [ ] Beautiful error messages

**Deliverable**: Great UX

---

### Phase 9: Testing & Docs (v0.9.0)
**Goal**: Production readiness

- [ ] Integration test suite
- [ ] Property tests
- [ ] Stress tests
- [ ] Benchmarks
- [ ] User documentation
- [ ] Man pages

**Deliverable**: Well-tested, documented

---

### Phase 10: v1.0 Release
**Goal**: Stable release

- [ ] Security audit
- [ ] Performance profiling
- [ ] CI/CD pipeline
- [ ] Release automation
- [ ] Homebrew formula
- [ ] Arch AUR package

**Deliverable**: `sy` v1.0.0 🚀

---

## Design Completeness Checklist

### Core Functionality
- ✅ Sync algorithm (rsync-based delta sync)
- ✅ Transport protocols (SSH custom, SFTP, local)
- ✅ Integrity verification (multi-layer checksums)
- ✅ Compression (adaptive zstd/lz4)
- ✅ Parallelism (files + chunks)

### Edge Cases
- ✅ Symlinks & hardlinks
- ✅ Sparse files
- ✅ Special files (devices, FIFOs, sockets)
- ✅ Metadata (permissions, xattrs, ACLs)
- ✅ Cross-platform filenames
- ✅ Large-scale performance (millions of files)
- ✅ Atomic operations & crash consistency
- ✅ Concurrent modification (TOCTOU)

### UX & Usability
- ✅ Adaptive performance modes (auto/local/lan/wan/verify/paranoid)
- ✅ Filter patterns (gitignore + rsync styles)
- ✅ Progress reporting (scales to millions of files)
- ✅ Error handling (threshold-based, categorized)
- ✅ Bandwidth limiting (token bucket)
- ⏳ SSH config integration (ProxyJump supported, ControlMaster requires OpenSSH)
- ✅ Deletion safety (confirmation, thresholds, trash)
- ✅ Dry-run mode
- ✅ Structured logging (tracing crate)
- ✅ Configuration files (TOML)

### Implementation Details
- ✅ Project structure & modules
- ✅ Dependencies selection
- ✅ Security considerations
- ✅ Testing strategy
- ✅ Implementation roadmap
- ✅ Exit codes & signal handling
- ✅ Resource limit checking

---

## Design is Complete! 🎉

This design document now contains:

1. **Vision & Philosophy** - What sy is and isn't
2. **Core Decisions** - Parallelism, delta sync, integrity, compression, transport
3. **Edge Cases** - 8 major categories fully designed
4. **Advanced Features** - Filters, bandwidth limiting, progress, SSH, errors, logging
5. **Additional Considerations** - Dry-run, signals, exit codes, resources, deduplication
6. **Testing Strategy** - Unit, integration, property, stress tests
7. **Configuration** - TOML format and usage
8. **Project Structure** - Modules and dependencies
9. **Security** - Threat model and safe defaults
10. **Roadmap** - 10-phase implementation plan

**Status**: Implementation in progress (v0.0.13 - Phase 4 complete)

**Note**: This document serves as a technical reference. For current implementation status and detailed feature roadmap, see `docs/MODERNIZATION_ROADMAP.md`.
