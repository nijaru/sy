# Design Document

This document captures key design decisions and technical rationale for `sy`.

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

**Decision**: Use rsync's rolling hash algorithm, not reinvent.

**Rationale**:
- Research shows rsync is still state-of-the-art for single-round protocols
- Multi-round protocols save bandwidth but add latency (not worth it)
- rsync algorithm is battle-tested, well-understood
- Focus innovation on parallel transfers, not delta algorithm

**Enhancements over classic rsync**:
- Parallel chunk processing where possible
- Modern hash functions (xxHash3 instead of MD5)
- Better progress reporting during block comparison

---

### 3. Hashing Strategy

**Decision**: xxHash3 for default integrity, BLAKE3 for cryptographic verification.

**Performance data**:
- xxHash3: ~10x faster than MD5, non-cryptographic
- BLAKE3: 10x faster than SHA-2, cryptographic, parallelizable
- SHA-256: Slower, but universally recognized

**Implementation**:
```rust
// Default mode: fast integrity checks
let hash = xxh3::hash64(data);

// Verify mode: cryptographic checksums
if args.verify {
    let hash = blake3::hash(data);
}
```

**CLI flags**:
- Default: xxHash3 (fast)
- `--verify`: BLAKE3 (cryptographic)
- `--checksum sha256`: SHA-256 (compatibility)

---

### 4. Compression Strategy

**Decision**: Adaptive compression with smart defaults and file-type awareness.

**Rationale**:
- Compression helps on slow networks, wastes CPU on fast LANs
- Many files are already compressed (.jpg, .mp4, .zip)
- Testing every file has overhead; cache decisions

**Algorithm**:
```rust
fn should_compress(file: &File, connection: &Connection) -> Compression {
    // Skip if LAN speed (network not bottleneck)
    if connection.speed > 100_MB_PER_SEC {
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

    // Use cached decision if available
    if let Some(cached) = connection.compression_cache.get(&file.type) {
        return *cached;
    }

    // Test first 64KB, cache decision
    let sample = file.read(0..64 * 1024);
    let decision = benchmark_compression(sample, connection.speed);
    connection.compression_cache.insert(file.type, decision);
    decision
}
```

**Compression options**:
- **zstd level 3** (default): Balanced speed/ratio
- **lz4** (fast mode): Minimal CPU overhead
- **zstd level 11+** (max mode): Slow networks
- **none**: Disable entirely

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

### 6. Skip & Resume Logic

**Decision**: Size + mtime check first, then delta sync only if needed.

**Rationale**:
- Most files unchanged between syncs
- Quick metadata check avoids expensive delta computation
- Resume support for interrupted transfers

**Implementation**:
```rust
fn transfer_strategy(local: &File, remote: &File) -> Strategy {
    // Quick skip: identical size and mtime
    if local.size == remote.size && local.mtime == remote.mtime {
        return Strategy::Skip;
    }

    // Resume: partial transfer exists
    if let Some(partial) = remote.partial_file() {
        return Strategy::Resume(partial.offset);
    }

    // Delta: file exists but differs
    if remote.exists() {
        return Strategy::Delta;
    }

    // Full: new file
    Strategy::Full
}
```

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

### Performance Benchmarks
- **rclone vs rsync**: 4x speedup with parallel transfers (Jeff Geerling, 2025)
- **xxHash vs MD5**: 10x faster on 6GB files
- **BLAKE3 vs SHA-2**: 10x faster, parallelizable
- **zstd vs lz4**: zstd better ratio, lz4 faster (~500MB/s)

### Tools Analyzed
- **rclone**: Multi-thread streams, parallel file transfers
- **rusync**: Minimalist Rust implementation
- **Mutagen**: Low-latency with filesystem watching
- **LuminS**: Fast local file sync in Rust

---

## Future Considerations

### Not in v1.0 (but worth considering)
- **Bidirectional sync** (like Syncthing)
- **Filesystem watching** (like Mutagen)
- **S3/cloud backends** (like rclone)
- **Encryption at rest**
- **Deduplication** (like restic/rustic)

### Explicitly excluded
- **Daemon mode** - Keep it simple, focus on CLI
- **GUI** - Terminal UX only
- **Windows ACLs** - Start with Unix permissions
- **Custom protocol** - Use SSH, focus on algorithm

---

## Testing Strategy

### Unit tests
- Hash function correctness (xxHash3, BLAKE3)
- Compression selection logic
- File skip/resume decisions
- Parallel chunk coordination

### Integration tests
- Full sync scenarios (new files, updated, deleted)
- Resume interrupted transfers
- Compression with various file types
- Error handling (permissions, network)

### Benchmarks
- Hash speed (xxHash3 vs BLAKE3 vs SHA-256)
- Compression (zstd vs lz4 vs none)
- Parallel vs sequential transfers
- Delta sync vs full transfer

### Property tests
- Sync idempotence (sync twice = same result)
- Compression roundtrip (compress + decompress = original)
- Hash collision resistance

---

## CLI Reference (Planned)

```
sy - Modern rsync alternative

USAGE:
    sy [OPTIONS] <SOURCE> <DESTINATION>

OPTIONS:
    -p, --preview           Show changes without applying
    -d, --delete            Delete files not in source
    -v, --verify            Cryptographic verification (BLAKE3)
    -w, --workers <N>       Parallel workers (default: CPU cores)
    -c, --compress <TYPE>   Compression: zstd|lz4|none|auto (default: auto)
        --fast              Fast mode (lz4 compression)
        --max-compression   Maximum compression (zstd level 11)
        --no-parallel       Disable parallel transfers
        --bandwidth <RATE>  Limit bandwidth (e.g., 10M, 1G)
        --config <PATH>     Use config file
        --rsync-compat      Accept rsync-style flags

EXAMPLES:
    sy ./src remote:/dest                 # Basic sync
    sy ./src remote:/dest --preview       # Preview changes
    sy ./src remote:/dest --delete        # Mirror (delete extras)
    sy docs                               # Use named config
```

---

This design balances performance, usability, and maintainability. Focus is on doing fewer things really well rather than feature bloat.
