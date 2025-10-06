# Optimization Roadmap - sy

> **Note**: This document tracks optimization history. For current status, see:
> - [README.md](../README.md) - Current features (v0.0.10)
> - [BENCHMARK_RESULTS.md](BENCHMARK_RESULTS.md) - Performance data
>
> **Current version**: v0.0.10 (Phase 3.5 complete)

## Optimization History

### Completed Optimizations ✅

#### 1. Delta Sync Implementation (v0.0.3)
- **Algorithm**: Rsync with Adler-32 + xxHash3
- **Status**: Implemented for SSH/remote operations
- **Decision**: Disabled for local operations (overhead > benefit)
- **Impact**: Dramatic bandwidth savings for remote updates

#### 2. Local Sync Performance (v0.0.3 → v0.0.8)
- **Problem**: Delta sync was 191x slower than direct copy locally
- **Root Cause**: Rolling hash O(n*block_size) overhead
- **Solution (v0.0.3)**: Disabled delta sync for LocalTransport
- **Result**: 26.93s → 0.14s (191x faster)

**Update (v0.0.8)**: Size-based heuristic re-enabled delta for large files
- **After O(1) fix**: True constant-time rolling hash (2ns/op)
- **After streaming**: Constant memory regardless of file size
- **Decision**: Enable delta for files >1GB only
- **Rationale**: For large files with small changes, I/O savings > overhead
- **Files <1GB**: Still use full copy (overhead > benefit)
- **Files >1GB**: Use delta sync (beneficial for partial updates)

#### 3. Progress Bar Improvements (v0.0.3)
- **Added**: ETA calculation
- **Added**: Steady tick animation
- **Format**: `[####>---] 42/100 (2m 15s) Updating file.txt`

### Performance Baseline

**Current (Sequential)**:
```
Local sync (100MB file update):  0.14s
Small files (100 files):         ~3.4s
Network: Single connection bandwidth
```

## High-Impact Optimizations (Phase 3)

### 1. Parallel File Transfers 🚀 **COMPLETED**

**Impact**: 5-10x speedup for multiple files
**Effort**: Medium (2-3 hours)
**Status**: ✅ Implemented (v0.0.4)

#### Design

```rust
// Concurrency control
const MAX_CONCURRENT: usize = 10;
let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT));

// Spawn tasks
let mut handles = vec![];
for task in tasks {
    let permit = semaphore.clone().acquire_owned().await?;
    let handle = tokio::spawn(async move {
        let result = execute_task(task).await;
        drop(permit);
        result
    });
    handles.push(handle);
}

// Collect results
let results = futures::future::join_all(handles).await;
```

#### Implementation Steps

1. **Stats tracking**: Use `Arc<Mutex<SyncStats>>` for thread-safe updates
2. **Progress bar**: Already thread-safe (indicatif design)
3. **Transport**: Wrap in `Arc` for sharing across tasks
4. **Error handling**: Collect all errors, report at end
5. **Semaphore**: Limit concurrent operations to avoid overwhelming system

#### Testing Strategy

- Benchmark with 100 small files (should be 5-10x faster)
- Test error handling (one failure doesn't stop others)
- Verify progress bar correctness with concurrent updates
- Test with --dry-run mode
- Verify --delete works correctly with parallelism

#### Expected Results

```
Before (sequential):
- 100 files (1MB each): ~3.4s
- Network: Limited by latency * file_count

After (parallel, 10 workers):
- 100 files (1MB each): ~0.4-0.8s (5-10x faster)
- Network: Saturate bandwidth, amortize latency
```

### 2. Bytes Transferred Accounting 📊 **COMPLETED**

**Impact**: Correctness (users see accurate statistics)
**Effort**: Medium (requires Transport trait changes)
**Status**: ✅ Implemented (v0.0.4)

#### Current Issue

```rust
// Reports source file size (incorrect for delta sync)
stats.bytes_transferred += source.size;

// Should report actual bytes (network transfer)
stats.bytes_transferred += result.bytes_written;
```

#### Design Options

**Option A**: Modify Transport trait to return TransferResult
```rust
pub struct TransferResult {
    pub bytes_written: u64,
    pub compression_ratio: f64,
}

async fn copy_file(...) -> Result<TransferResult>;
async fn sync_file_with_delta(...) -> Result<TransferResult>;
```

**Option B**: Use thread-local state (hacky, not recommended)

**Option C**: Accept current behavior, document clearly

**Recommendation**: Option A (clean, correct)

### 3. Streaming Delta Generation **COMPLETED**

**Impact**: Reduced memory usage for large files
**Effort**: High (requires algorithm refactor)
**Status**: ✅ Implemented (v0.0.6)

#### Problem

```rust
// Original: loads entire source file into memory
let mut source_data = Vec::new();
source_file.read_to_end(&mut source_data)?;
// For 10GB file: 10GB RAM usage
```

#### Solution

`generate_delta_streaming()` with constant memory usage:
- Read source in 128KB chunks
- Maintain sliding window buffer (capacity: block_size + 128KB)
- Sliding window refilling when processed data exceeds block_size
- Emit delta operations incrementally
- Never load full file into memory

#### Implementation

```rust
pub fn generate_delta_streaming(
    source_path: &Path,
    dest_checksums: &[BlockChecksum],
    block_size: usize,
) -> io::Result<Delta> {
    const CHUNK_SIZE: usize = 128 * 1024; // 128KB chunks

    // Sliding window: block_size + CHUNK_SIZE
    let mut window = Vec::with_capacity(block_size + CHUNK_SIZE);
    let mut chunk_buf = vec![0u8; CHUNK_SIZE];

    // Process incrementally, refill window as needed
    while window_pos < window.len() {
        // Match blocks and emit operations...

        // Refill when needed
        if window_pos >= block_size && window.len() - window_pos < block_size {
            window.drain(0..window_pos); // Remove processed bytes
            window_pos = 0;
            bytes_read = source_file.read(&mut chunk_buf)?;
            if bytes_read > 0 {
                window.extend_from_slice(&chunk_buf[..bytes_read]);
            }
        }
    }
}
```

#### Results

**Memory Usage**:
- Original: O(file_size) - loads entire file
- Streaming: O(1) - constant ~256KB regardless of file size
- For 10GB file: 10GB → 256KB (39,000x reduction)

**Performance**: Identical (same algorithm, different I/O pattern)

**Tests** (5 comprehensive tests):
- `test_streaming_identical_files` - Basic functionality
- `test_streaming_large_file` - 256KB file (2x chunk size)
- `test_streaming_vs_nonstreaming_identical` - Correctness verification
- `test_streaming_window_refill` - 512KB file (tests refilling logic)
- `test_streaming_empty_file` - Edge case handling

### 4. True O(1) Rolling Hash **COMPLETED**

**Impact**: Enable delta sync for local operations
**Effort**: Medium (algorithm correctness is tricky)
**Status**: ✅ Implemented (v0.0.5)

#### Implementation

Adler-32 rolling formula with O(1) incremental update:
- A_new = (A_old - old_byte + new_byte) mod M
- B_new = (B_old - n*old_byte + A_new - 1) mod M

```rust
// O(1) incremental update
self.a = (self.a + MOD_ADLER * 2 - old + new) % MOD_ADLER;
let n_old = (n * old) % MOD_ADLER;
self.b = (self.b + MOD_ADLER * 3 - n_old + self.a - 1) % MOD_ADLER;
```

#### Performance Results

**Benchmark** (8KB blocks, 100K iterations):
- Old O(n): 1.79s
- New O(1): 293µs
- **Speedup: 6,124x faster**

**Critical Bug Fixed** (v0.0.5):
- Initial implementation maintained unused `window: Vec<u8>` field
- `Vec::remove(0)` in roll() was O(n), defeating optimization
- Fixed by removing window entirely - hash state (a, b) is sufficient
- **Verified true O(1)**: 2ns per operation regardless of block size

**Tests**: 11 comprehensive tests including edge cases:
- Large blocks (128KB)
- All zeros / all 0xFF
- Repeating patterns
- Modulo boundary conditions
- Constant-time verification across all block sizes

## Long-Term Optimizations

### 5. Compression Module **COMPLETED (Foundation)**

**Impact**: Network bandwidth savings for WAN transfers
**Effort**: Medium (module complete, transport integration deferred)
**Status**: ✅ Module implemented (v0.0.7), transport integration pending

#### Implementation

Compression module with LZ4 and Zstd support:
- `compress()` and `decompress()` functions
- Smart heuristics (file size, extension-based)
- List of pre-compressed extensions (jpg, mp4, zip, etc.)
- 11 comprehensive tests

```rust
pub enum Compression {
    None,
    Lz4,    // Fast: ~400-500 MB/s compression speed
    Zstd,   // Better ratio: level 3 (balanced)
}

// Automatic decision logic
pub fn should_compress(filename: &str, file_size: u64) -> Compression {
    if file_size < 1MB { return Compression::None; }
    if is_compressed_extension(filename) { return Compression::None; }
    Compression::Lz4  // Default for >1MB uncompressed files
}
```

#### Test Results (11 tests passing)

- ✅ LZ4 roundtrip correctness
- ✅ Zstd roundtrip correctness
- ✅ Compression ratio verification (repetitive data <10%)
- ✅ Zstd better compression than LZ4
- ✅ Extension detection (jpg, mp4, pdf, etc.)
- ✅ Size-based heuristics
- ✅ Empty data and large data (1MB) handling

#### Transport Integration (Deferred)

**Reason for deferral**: SSH transport integration requires:
1. Remote helper binary to decompress
2. Bidirectional delta sync optimization
3. Network speed detection for adaptive compression

**Current state**: Module complete and ready for use. Integration will come in Phase 6 when implementing proper remote delta sync (send delta ops, not full file).

**Planned approach**:
- Local: No compression (disk I/O bottleneck)
- LAN (>500 MB/s): No compression (CPU bottleneck)
- LAN (100-500 MB/s): LZ4 only
- WAN (<100 MB/s): Adaptive zstd levels

### Network Detection (Phase 5)

**Auto-detect connection type**:
```bash
sy ~/src remote:/dst          # Auto-detects: WAN, uses compression
sy ~/src nas:/dst             # Auto-detects: LAN, no compression
sy ~/src /backup              # Auto-detects: Local, max parallelism
```

Implementation:
- Ping latency: <1ms = local, <10ms = LAN, >10ms = WAN
- Bandwidth test: Small file transfer timing
- mDNS for local network detection

### Resume Support (Phase 6)

**Checkpoint progress for large transfers**:
- Save state every N files
- Resume from checkpoint on failure
- Verify partial files with checksums

### Parallel Chunks (Phase 7)

**Split large files across multiple connections**:
- SSH connection pooling
- Range requests for HTTP
- Combine with delta sync
- Requires careful coordination

## Benchmarking

### Test Suite

```bash
# Small files (latency bound)
create_files 1000 1KB
benchmark sy vs rsync vs rclone

# Large files (bandwidth bound)
create_files 10 100MB
benchmark sy vs rsync vs rclone

# Mixed workload
create_files 100 1KB-10MB
benchmark sy vs rsync vs rclone

# Delta sync
modify_files 10% of 100MB
benchmark delta vs full copy

# Network simulation
use tc to add latency/jitter
benchmark local vs LAN vs WAN profiles
```

### Performance Targets

| Scenario | Current | Target | Improvement |
|----------|---------|--------|-------------|
| 100 small files | 3.4s | 0.4s | 8x |
| 10 large files | 33s | 3.3s | 10x |
| Delta (network) | 26s | 0.5s | 50x+ |
| Delta (local) | 0.14s | 0.14s | - |

## Priority Order

1. ⚡ **Parallel file transfers** (Week 1) - Biggest immediate win
2. 📊 **Bytes transferred accounting** (Week 1) - Correctness
3. 🔄 **O(1) rolling hash** (Week 2) - Enables local delta
4. 💾 **Streaming delta** (Week 3) - Memory efficiency
5. 🗜️ **Compression** (Week 4) - Network optimization
6. 🌐 **Network detection** (Week 4) - Auto-tuning
7. ⏸️ **Resume support** (Week 5) - Reliability
8. 🚀 **Parallel chunks** (Week 6) - Max performance

## Notes

- Focus on correctness before performance
- Benchmark before and after each optimization
- Document tradeoffs clearly
- Test edge cases thoroughly
- Keep code maintainable

---

**Last Updated**: 2025-10-02
**Current Version**: v0.0.8 (size-based local delta!)
**Completed**:
- ✅ Parallel transfers (5-10x faster)
- ✅ Bytes transferred accounting (correctness)
- ✅ O(1) rolling hash (TRUE O(1): 2ns constant time)
- ✅ Streaming delta generation (O(1) memory, 39,000x reduction for 10GB files)
- ✅ Compression module (LZ4 + Zstd, transport integration deferred)
- ✅ Size-based local delta (>1GB files use delta sync)

**Recent Achievements**:
- Local delta sync re-enabled for large files (>1GB)
- Size-based heuristic leverages O(1) hash + streaming
- Atomic file updates (temp + rename)
- All 85 tests passing

**Next Target**: Benchmarking and performance validation
