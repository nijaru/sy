# SSH Performance Optimization

## Current State

sy uses a custom binary protocol over SSH stdin/stdout (like rsync). Server mode implemented in v0.1.2 but not benchmarked.

## Bottlenecks Identified

### 1. Sequential Delta Sync (High Impact)

**Location**: `src/sync/server_mode.rs:203-306`

**Current flow** (per file):

```
CHECKSUM_REQ → wait → CHECKSUM_RESP → compute delta → DELTA_DATA → wait → FILE_DONE
```

4 round-trips per file. For 100 modified files over 50ms latency = 20 seconds overhead.

**rsync approach**: Requests checksums while still sending previous file's delta.

**Fix**: Pipeline checksum requests

```
CHECKSUM_REQ(file1)
CHECKSUM_REQ(file2)  ← don't wait
CHECKSUM_RESP(file1) → compute → DELTA_DATA(file1)
CHECKSUM_REQ(file3)  ← request next
CHECKSUM_RESP(file2) → compute → DELTA_DATA(file2)
...
```

**Estimated gain**: 3-4x faster delta sync over high-latency links

### 2. No Stream Compression (Medium Impact)

**Location**: `src/server/protocol.rs`

**Current**: Compression only for files >1MB, per-message basis.

**rsync approach**: Wraps entire SSH channel in zstd stream after HELLO.

**Fix**: Add `compression: bool` to HELLO negotiation, wrap stdin/stdout in zstd encoder/decoder.

```rust
// After HELLO exchange
let stdin = if compression {
    Box::new(zstd::stream::write::Encoder::new(stdin, 3)?)
} else {
    Box::new(stdin)
};
```

**Estimated gain**: 2-5x less bandwidth for compressible data

### 3. Full File in Memory (Medium Impact)

**Location**: `src/sync/server_mode.rs:153-175`

**Current**: `std::fs::read(&*path)` loads entire file before sending.

**Fix**: Stream chunks while reading

```rust
let file = File::open(&path)?;
let mut reader = BufReader::with_capacity(64 * 1024, file);
loop {
    let chunk = reader.fill_buf()?;
    if chunk.is_empty() { break; }
    session.send_file_chunk(idx, offset, chunk).await?;
    offset += chunk.len();
    reader.consume(chunk.len());
}
```

**Estimated gain**: Constant memory, faster time-to-first-byte

### 4. Compression Threshold Too High (Low Impact)

**Current**: 1MB minimum for compression

**Issue**: Small files sent uncompressed even over slow links where CPU is faster than network.

**Fix**: Adaptive based on network speed (already designed, not implemented)

- > 500 MB/s: No compression
- 100-500 MB/s: LZ4
- <100 MB/s: zstd (include small files)

## Implementation Priority

| Optimization         | Impact | Effort | Priority |
| -------------------- | ------ | ------ | -------- |
| Pipeline delta       | High   | Medium | P0       |
| Stream compression   | Medium | Low    | P1       |
| Streaming file I/O   | Medium | Medium | P2       |
| Adaptive compression | Low    | Low    | P3       |

## Benchmark First

Before implementing, run `scripts/benchmark.py --ssh user@host` to establish baseline.

Compare against rsync in these scenarios:

1. Initial sync (many small files) - tests batching
2. Initial sync (few large files) - tests throughput
3. Incremental (no changes) - tests scan speed
4. Delta (modified files) - tests delta algorithm

## Benchmark Results (2025-12-18)

### Local (Mac M3 Max → Local)

| Scenario           | Operation   | sy     | rsync  | Winner      |
| ------------------ | ----------- | ------ | ------ | ----------- |
| small_files (1000) | initial     | 319ms  | 197ms  | rsync 1.6x  |
| small_files (1000) | incremental | 21ms   | 63ms   | **sy 3x**   |
| small_files (1000) | delta       | 20ms   | 63ms   | **sy 3x**   |
| large_file (100MB) | initial     | 46ms   | 327ms  | **sy 7x**   |
| large_file (100MB) | incremental | 7ms    | 10ms   | **sy 1.5x** |
| source_code (5000) | initial     | 2164ms | 1026ms | rsync 2.1x  |
| source_code (5000) | incremental | 79ms   | 273ms  | **sy 3.5x** |

### SSH (Mac → Fedora via Tailscale)

| Scenario           | Operation   | sy    | rsync  | Winner      |
| ------------------ | ----------- | ----- | ------ | ----------- |
| small_files (1000) | initial     | 428ms | 285ms  | rsync 1.5x  |
| small_files (1000) | incremental | 333ms | 228ms  | rsync 1.5x  |
| small_files (1000) | delta       | 350ms | 226ms  | rsync 1.5x  |
| large_file (100MB) | initial     | 396ms | 1644ms | **sy 4.2x** |
| large_file (100MB) | incremental | 292ms | 214ms  | rsync 1.4x  |
| source_code (5000) | initial     | 965ms | 1682ms | **sy 1.7x** |
| source_code (5000) | incremental | 512ms | 336ms  | rsync 1.5x  |

### Key Findings

1. **Local**: sy wins incremental/delta massively (3x), loses initial for many small files
2. **SSH**: sy wins initial sync (bulk transfers), but **loses** incremental/delta by ~1.5x
3. **Constant overhead**: sy's SSH incremental has ~300ms overhead regardless of data size

### Root Cause Analysis

The SSH incremental slowness points to:

- Protocol handshake overhead (HELLO exchange)
- File list comparison happening locally instead of streaming
- Sequential checksum requests for delta

### Updated Priority

| Optimization                       | Impact | Priority |
| ---------------------------------- | ------ | -------- |
| Optimize incremental protocol flow | High   | P0       |
| Pipeline delta (checksum requests) | High   | P0       |
| Stream compression                 | Medium | P1       |
