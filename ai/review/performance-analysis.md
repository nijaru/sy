# Performance Analysis: sy File Synchronization

**Date**: 2026-01-18
**Scope**: Hot paths, memory patterns, async efficiency, I/O patterns
**Baseline**: Benchmarks from ai/STATUS.md

---

## Executive Summary

| Metric                 | Local Sync   | SSH Sync                      |
| ---------------------- | ------------ | ----------------------------- |
| vs rsync (small files) | 2-44x faster | 1.3-1.4x slower (incremental) |
| vs rsync (large files) | 2-11x faster | Near parity                   |
| vs rsync (delta sync)  | 2x faster    | Needs measurement             |

**Key Finding**: Local performance is excellent. SSH incremental sync is the primary performance gap requiring attention.

---

## Hot Paths Identified

### 1. Directory Scanner (`src/sync/scanner.rs`)

**Location**: `process_dir_entry()` lines 180-280

**Current behavior**:

- Adaptive parallel/sequential based on subdirectory count (threshold: 30)
- Bounded channel with 1024 capacity for backpressure
- Multiple allocations per entry: 2x `Arc<PathBuf>`, HashMap for xattrs

**Bottleneck**: Allocation overhead for large directories with many small files.

```rust
// Lines 195-197: Two Arc<PathBuf> allocations per entry
let path = Arc::new(entry.path());
let rel_path = Arc::new(rel_path);
```

### 2. Delta Generator (`src/delta/generator.rs`)

**Location**: `generate_delta_streaming()` lines 45-180

**Current behavior**:

- Streaming with 256KB chunks keeps memory at ~512KB
- Rolling hash (Adler-32) for block matching
- Strong hash (xxHash3) for verification

**Bottleneck**: `literal_buffer` grows unbounded when no matches found; `window.drain(0..window_pos)` shifts entire Vec.

```rust
// Line 89: Vec shift on every match
window.drain(0..window_pos);

// Line 95: Unbounded growth
literal_buffer.push(byte);
```

### 3. Protocol Serialization (`src/server/protocol.rs`)

**Location**: `write_to()` methods throughout

**Current behavior**:

- Length-type-payload pattern
- Each write creates intermediate `Vec<u8>` allocation

**Bottleneck**: Allocation per message serialization.

### 4. Server Mode Transfer (`src/sync/server_mode.rs`)

**Location**: `transfer_files()` lines 150-250, `process_delta_batch()` lines 680-801

**Current behavior**:

- Fixed pipeline depth of 8 concurrent requests
- Data cloning at line 185 for send operations

**Bottleneck**: `data.clone()` copies entire file content for protocol transmission.

```rust
// Line 185: Clones potentially large data
session.send_file_data_with_flags(*idx, 0, *flags, data.clone())
```

### 5. Local Copy (`src/transport/local.rs`)

**Location**: `copy_file()` lines 80-150

**Current behavior**:

- Uses `fs::copy()` for OS-optimized COW (clonefile/copy_file_range)
- 256KB BufReader for non-COW paths
- xattr stripping adds syscalls after copy

**Bottleneck**: Post-copy xattr operations add syscall overhead.

---

## Memory Allocation Patterns

| Component | Allocation                 | Size            | Frequency             | Impact              |
| --------- | -------------------------- | --------------- | --------------------- | ------------------- |
| Scanner   | `Arc<PathBuf>`             | 16 + path len   | 2x per entry          | Medium              |
| Scanner   | `HashMap<String, Vec<u8>>` | Variable        | Per entry with xattrs | Low                 |
| Delta     | `literal_buffer: Vec<u8>`  | Up to file size | Per delta generation  | High (pathological) |
| Delta     | `window: Vec<u8>`          | block_size      | Per delta generation  | Low                 |
| Protocol  | `Vec<u8>`                  | message size    | Per message           | Medium              |
| Server    | `data.clone()`             | file size       | Per file transfer     | High                |

### Recommendations

1. **Scanner**: Consider `Cow<Path>` or path interning for repeated directory prefixes
2. **Delta**: Use ring buffer instead of Vec for window; cap literal_buffer and flush
3. **Protocol**: Pre-allocate message buffer, reuse across serializations
4. **Server**: Use `Bytes` or `Arc<[u8]>` to avoid cloning file data

---

## Async/Concurrency Efficiency

### Current Model

```
Scanner (spawn_blocking) --> Channel (1024) --> Processor (async)
                                                    |
                                                    v
                                            Local: spawn_blocking for I/O
                                            Remote: async network I/O
```

### Analysis

| Aspect               | Implementation            | Assessment        |
| -------------------- | ------------------------- | ----------------- |
| Scanner parallelism  | Adaptive (>30 subdirs)    | Good              |
| Channel backpressure | Bounded (1024)            | Adequate          |
| I/O dispatch         | spawn_blocking for fs ops | Correct           |
| Pipeline depth       | Fixed 8                   | May be suboptimal |
| Batch processing     | Delta batches of ~100     | Good              |

### Issues

1. **Pipeline depth hardcoded**: `const PIPELINE_DEPTH: usize = 8` does not adapt to network latency or bandwidth
2. **No work stealing**: Parallel directory scanning doesn't balance across uneven directory trees
3. **Semaphore contention**: High-throughput scenarios may bottleneck on semaphore acquisition

---

## I/O Patterns

### Buffer Sizes

| Operation          | Buffer Size  | Rationale                  |
| ------------------ | ------------ | -------------------------- |
| File read (delta)  | 256 KB       | Balance memory vs syscalls |
| Block size (delta) | 64 KB        | Match rsync default        |
| Stream chunk       | 256 KB       | Streaming delta generation |
| Protocol read      | 8 KB default | Tokio BufReader            |

### Syscall Efficiency

| Path              | Syscalls per File   | Notes                         |
| ----------------- | ------------------- | ----------------------------- |
| Local COW         | 1-2                 | clonefile/copy_file_range     |
| Local non-COW     | N (file_size/256KB) | read/write loop               |
| Local with xattrs | +2-4 per xattr      | listxattr, getxattr, setxattr |
| Remote            | 2 + N chunks        | open, read chunks, close      |

---

## Benchmark Gaps

| Benchmark      | Coverage                              | Gap                   |
| -------------- | ------------------------------------- | --------------------- |
| sync_bench.rs  | Small files, nested dirs, large files | No network simulation |
| delta_bench.rs | Delta generation/application          | Good coverage         |
| scale.rs       | Scaling tests                         | Limited               |

**Missing**: Network latency simulation, memory profiling, concurrent transfer benchmarks, mixed workloads.

---

## Optimization Recommendations

### High Impact, Low Effort

| ID  | Change                                  | Location           | Expected Impact             |
| --- | --------------------------------------- | ------------------ | --------------------------- |
| H1  | Replace `data.clone()` with `Arc<[u8]>` | server_mode.rs:185 | -50% memory for large files |
| H2  | Use ring buffer for delta window        | generator.rs:89    | -30% CPU for delta gen      |
| H3  | Pre-allocate protocol buffer            | protocol.rs        | -20% allocations            |

### High Impact, Medium Effort

| ID  | Change                     | Location       | Expected Impact        |
| --- | -------------------------- | -------------- | ---------------------- |
| M1  | Adaptive pipeline depth    | server_mode.rs | +20-50% SSH throughput |
| M2  | Batch xattr operations     | local.rs       | -10% syscalls          |
| M3  | Path interning for scanner | scanner.rs     | -30% allocations       |

### Medium Impact, Higher Effort

| ID  | Change                    | Location       | Expected Impact             |
| --- | ------------------------- | -------------- | --------------------------- |
| L1  | Vectored I/O for protocol | protocol.rs    | -15% syscalls               |
| L2  | Work-stealing for scanner | scanner.rs     | +10-20% on unbalanced trees |
| L3  | Zero-copy file transfer   | server_mode.rs | Significant for large files |

---

## Conclusion

sy achieves excellent local performance through OS-optimized COW operations, efficient streaming delta algorithm, and adaptive parallel scanning.

The SSH incremental sync regression (1.3-1.4x slower than rsync) likely stems from:

1. Fixed pipeline depth not adapting to network conditions
2. Data cloning overhead in server mode
3. Protocol serialization allocations

**Priority**: Focus on H1 (`Arc<[u8]>`) and M1 (adaptive pipeline) for maximum SSH performance improvement.

---

_Files analyzed_: 15 source files, 3 benchmark files
_Methodology_: Static analysis + benchmark review
