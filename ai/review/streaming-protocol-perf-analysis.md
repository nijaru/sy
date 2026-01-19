# Streaming Protocol v2 Performance Analysis

**Date**: 2026-01-19
**Branch**: `feature/streaming-protocol-v2`
**Analyst**: profiler agent
**Focus**: Hot paths in streaming protocol implementation

## Executive Summary

The streaming protocol v2 implementation has several performance opportunities. The most impactful issues are:

1. **String allocations in hot loops** - `to_string_lossy().to_string()` patterns (HIGH)
2. **Path cloning per entry** - `path.to_path_buf()` and `Arc::new(rel_path)` (HIGH)
3. **Synchronous scan blocking async pipeline** - `spawn_blocking` overhead (MEDIUM)
4. **Vec allocation in delta ops** - `Vec<u8>` per operation (MEDIUM)
5. **Protocol decode allocations** - `copy_to_bytes().to_vec()` pattern (MEDIUM)

**Target metrics from STATUS.md**:

- SSH small_files: parity with rsync
- Time to first byte: <0.5s
- Memory (1M files): <500MB

Current estimate with 1M files: ~800MB (160 bytes/entry x 1M + overhead)

---

## Critical Path Analysis

### 1. Generator Hot Loop

**Files**: `/Users/nick/github/nijaru/sy/src/streaming/generator.rs:88-155`

#### [HIGH] generator.rs:89-90 - String allocation per entry

```rust
let rel_path = entry.relative_path.as_ref().to_path_buf();
let rel_path_str = rel_path.to_string_lossy().to_string();
```

**Issue**: Two allocations per file entry:

1. `to_path_buf()` clones the PathBuf from Arc
2. `to_string_lossy().to_string()` creates owned String

With 1M files at ~50 bytes average path, this is ~100MB of redundant allocations.

**Suggested optimization**:

```rust
// Borrow directly from Arc, avoid String allocation until needed
let rel_path = entry.relative_path.as_ref();
let rel_path_str = rel_path.to_string_lossy();  // Cow<str>, no alloc if ASCII

// Only allocate when sending to channel
GeneratorMessage::File(FileJob {
    path: entry.relative_path.clone(),  // Arc clone, not PathBuf clone
    // ...
})
```

**Estimated benefit**: -50% generator memory, -10% CPU time
**Effort**: Low (2-3 hours)

---

#### [HIGH] generator.rs:143-144 - Arc wrapping in loop

```rust
GeneratorMessage::File(FileJob {
    path: Arc::new(rel_path),  // New Arc allocation per file
    // ...
})
```

**Issue**: Creates new Arc for path that already exists as `Arc<PathBuf>` in scanner entry.

**Suggested optimization**:

```rust
// Reuse Arc from scanner entry directly
path: entry.relative_path.clone(),  // Arc::clone is O(1)
```

**Estimated benefit**: -50% path-related allocations
**Effort**: Low (1 hour, signature change)

---

#### [MEDIUM] generator.rs:167-171 - Delete path collection

```rust
let remaining: Vec<_> = self
    .dest_index
    .remaining_paths()
    .map(|(path, state)| (path.clone(), state.is_dir))
    .collect();
```

**Issue**: Collects all remaining paths into Vec, then iterates again. Clones String paths.

**Suggested optimization**:

```rust
// Iterate directly, avoid intermediate Vec
for (path, state) in self.dest_index.remaining_paths() {
    tx.send(GeneratorMessage::Delete {
        path: Arc::new(PathBuf::from(path)),  // Allocate once, not clone then Arc
        is_dir: state.is_dir,
    })
    .await?;
    delete_count += 1;
}
```

**Estimated benefit**: -5% delete phase memory
**Effort**: Low (30 minutes)

---

### 2. Sender Hot Loop

**Files**: `/Users/nick/github/nijaru/sy/src/streaming/sender.rs:40-86`

#### [HIGH] sender.rs:50-51, 55-56, 62-64 - to_string_lossy per message

```rust
let msg = Mkdir {
    path: path.to_string_lossy().to_string(),
    mode,
};
```

Pattern repeats for Mkdir, Symlink, Delete messages.

**Issue**: Converts Arc<PathBuf> to String for each message. For 100k directories, this is 100k allocations.

**Suggested optimization**:

```rust
// Store paths as Bytes in protocol messages, decode lazily
let msg = Mkdir {
    path: Bytes::copy_from_slice(path.to_string_lossy().as_bytes()),
    mode,
};

// Or: Make protocol messages generic over path type
pub struct Mkdir<P> {
    pub path: P,
    pub mode: u32,
}
```

**Estimated benefit**: -30% sender string allocations
**Effort**: Medium (6-8 hours, protocol changes)

---

#### [MEDIUM] sender.rs:137 - Buffer allocation per chunk

```rust
let mut buf = vec![0u8; DATA_CHUNK_SIZE];  // 256KB per file
```

**Issue**: Allocates 256KB buffer for each file, even small files.

**Suggested optimization**:

```rust
// Reuse buffer across files (pass as parameter or store in Sender)
pub struct Sender {
    config: SenderConfig,
    read_buf: Vec<u8>,  // Reused across files
}

impl Sender {
    pub fn new(config: SenderConfig) -> Self {
        Self {
            config,
            read_buf: vec![0u8; DATA_CHUNK_SIZE],
        }
    }
}
```

**Estimated benefit**: -95% buffer allocations
**Effort**: Low (1-2 hours)

---

#### [MEDIUM] sender.rs:154 - Bytes::copy_from_slice per chunk

```rust
data: Bytes::copy_from_slice(&buf[..n]),
```

**Issue**: Copies read buffer into new Bytes allocation.

**Suggested optimization**:

```rust
// Use BytesMut with split_to for zero-copy chunks
let mut buf = BytesMut::with_capacity(DATA_CHUNK_SIZE);
// ... read into buf ...
data: buf.split_to(n).freeze(),  // Zero-copy, just adjusts pointers
```

**Estimated benefit**: -50% data chunk allocations
**Effort**: Medium (2-4 hours)

---

#### [MEDIUM] sender.rs:210-224 - Delta ops serialization

```rust
let mut delta_bytes = Vec::new();
for op in delta.ops {
    match op {
        DeltaOp::Copy { offset, size } => {
            delta_bytes.push(0x00);
            delta_bytes.extend_from_slice(&offset.to_be_bytes());
            delta_bytes.extend_from_slice(&(size as u32).to_be_bytes());
        }
        DeltaOp::Data(data) => {
            delta_bytes.push(0x01);
            delta_bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
            delta_bytes.extend_from_slice(&data);
        }
    }
}
```

**Issue**: Creates Vec for delta ops, then converts to Bytes. Multiple `extend_from_slice` calls cause reallocations.

**Suggested optimization**:

```rust
// Pre-calculate size
let size_estimate = delta.ops.iter().map(|op| match op {
    DeltaOp::Copy { .. } => 13,  // 1 + 8 + 4
    DeltaOp::Data(d) => 5 + d.len(),  // 1 + 4 + len
}).sum();

let mut delta_bytes = BytesMut::with_capacity(size_estimate);
// ... serialize ...
```

**Estimated benefit**: -50% delta serialization allocations
**Effort**: Low (1-2 hours)

---

### 3. Receiver Hot Loop

**Files**: `/Users/nick/github/nijaru/sy/src/streaming/receiver.rs:53-119`

#### [HIGH] receiver.rs:64-66 - Path string conversion in scan

```rust
let rel_path = entry.relative_path.as_ref();
let path_str = rel_path.to_string_lossy().to_string();
```

Same pattern as generator - allocates String per entry during initial exchange.

**Suggested optimization**: Same as generator - use Cow<str> or delay allocation.

**Estimated benefit**: -50% receiver scan memory
**Effort**: Low (2 hours)

---

#### [HIGH] receiver.rs:78-85 - Checksum computation blocking

```rust
let (block_size, checksums) = if !entry.is_dir && entry.size >= DELTA_MIN_SIZE {
    flags |= DestFileFlags::HAS_CHECKSUMS;
    let cs = self.compute_checksums(&entry.path).await?;
    (self.config.block_size, cs)
} else {
    (0, vec![])
};
```

**Issue**: Computes checksums sequentially for each file, even though the underlying implementation uses rayon. The `await?` blocks the receiver loop.

Looking at `compute_checksums` in receiver.rs:121-139:

```rust
let checksums =
    tokio::task::spawn_blocking(move || crate::delta::checksum::compute_checksums(&p, bs))
        .await??;
```

**Issue**: spawn_blocking has ~5us overhead per call. With 10k delta candidates, that's 50ms of pure overhead.

**Suggested optimization**:

```rust
// Batch checksum computation
// Collect all delta candidates first, then compute in parallel
let delta_candidates: Vec<_> = entries
    .iter()
    .filter(|e| !e.is_dir && e.size >= DELTA_MIN_SIZE)
    .collect();

let checksums: Vec<_> = tokio::task::spawn_blocking(move || {
    delta_candidates
        .par_iter()
        .map(|e| (e.path.clone(), compute_checksums(&e.path, block_size)))
        .collect()
}).await??;
```

**Estimated benefit**: -80% spawn_blocking overhead, better parallelism
**Effort**: Medium (4-6 hours)

---

#### [MEDIUM] receiver.rs:339-348 - Delta apply buffer allocation

```rust
0x00 => {
    // Copy
    let offset = reader.get_u64();
    let size = reader.get_u32() as usize;

    let mut buf = vec![0u8; size];  // New allocation per Copy op
    original.seek(SeekFrom::Start(offset)).await?;
    original.read_exact(&mut buf).await?;
    file.write_all(&buf).await?;
}
```

**Issue**: Allocates buffer for each Copy operation. A delta with 1000 Copy ops allocates 1000 buffers.

**Suggested optimization**:

```rust
// Reuse buffer, grow only if needed
let mut copy_buf = Vec::with_capacity(block_size);
// ...
0x00 => {
    let size = reader.get_u32() as usize;
    if copy_buf.len() < size {
        copy_buf.resize(size, 0);
    }
    original.read_exact(&mut copy_buf[..size]).await?;
    file.write_all(&copy_buf[..size]).await?;
}
```

**Estimated benefit**: -95% delta apply allocations
**Effort**: Low (1 hour)

---

### 4. Protocol Serialization/Deserialization

**Files**: `/Users/nick/github/nijaru/sy/src/streaming/protocol.rs`

#### [MEDIUM] protocol.rs:207, 291, 438, 546, etc. - copy_to_bytes().to_vec() pattern

```rust
let path = String::from_utf8(payload.copy_to_bytes(path_len).to_vec())
    .context("Invalid UTF-8 in FileEntry path")?;
```

**Issue**: `copy_to_bytes()` creates Bytes, `.to_vec()` copies again, `String::from_utf8` takes ownership. Two copies.

**Suggested optimization**:

```rust
// Single copy with Bytes directly
let path_bytes = payload.copy_to_bytes(path_len);
let path = String::from_utf8(path_bytes.to_vec())  // Still one copy
    .context("Invalid UTF-8")?;

// Or for true zero-copy (if path lifetime allows):
// Use Bytes directly as path representation
```

**Estimated benefit**: -30% protocol decode memory
**Effort**: Low (2-3 hours)

---

#### [MEDIUM] protocol.rs:1021-1032 - read_frame allocation

```rust
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<(MessageType, Bytes)> {
    // ...
    let payload_len = len.saturating_sub(0) as usize;
    let mut payload = vec![0u8; payload_len];  // Allocation per frame
    r.read_exact(&mut payload).await?;
    Ok((msg_type, Bytes::from(payload)))
}
```

**Issue**: Every frame read allocates a new Vec, then converts to Bytes.

**Suggested optimization**:

```rust
// Use BytesMut with read_buf for zero-copy
pub async fn read_frame_buffered<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut BytesMut,
) -> Result<(MessageType, Bytes)> {
    let len = r.read_u32().await?;
    let msg_type = r.read_u8().await?;

    buf.reserve(len as usize);
    unsafe { buf.set_len(len as usize); }
    r.read_exact(buf).await?;

    Ok((msg_type, buf.split().freeze()))
}
```

**Estimated benefit**: -50% frame read allocations (with buffer reuse)
**Effort**: Medium (4-6 hours, API change)

---

### 5. Channel Configuration

**Files**: `/Users/nick/github/nijaru/sy/src/streaming/channel.rs`

#### [LOW] channel.rs:13-14 - Channel buffer sizes

```rust
pub const GENERATOR_CHANNEL_SIZE: usize = 1024;
pub const SENDER_CHANNEL_SIZE: usize = 64;
```

**Issue**: GENERATOR_CHANNEL_SIZE of 1024 may cause memory pressure with large FileJob structs. Each FileJob is ~120 bytes, so 1024 \* 120 = 122KB just for buffered jobs.

For 1M files, if generator outpaces sender, memory will spike.

**Suggested optimization**:

```rust
// Reduce generator channel size, increase sender channel
pub const GENERATOR_CHANNEL_SIZE: usize = 256;  // 30KB buffer
pub const SENDER_CHANNEL_SIZE: usize = 128;     // More data pipelining
```

**Estimated benefit**: -75% channel memory, better backpressure
**Effort**: Low (30 minutes, benchmark to tune)

---

### 6. Pipeline Orchestration

**Files**: `/Users/nick/github/nijaru/sy/src/streaming/pipeline.rs`

#### [HIGH] pipeline.rs:99-110 - blocking_send in async context

```rust
let sender_handle = tokio::spawn(async move {
    sender
        .run(rx, |bytes| {
            if data_tx.blocking_send(bytes).is_err() {  // BLOCKING in async!
                return Err(anyhow::anyhow!("Data channel closed"));
            }
            Ok(())
        })
        .await
});
```

**Issue**: `blocking_send` in async context can block the tokio runtime thread. If channel is full, this starves other tasks.

**Suggested optimization**:

```rust
// Use async channel directly
let sender_handle = tokio::spawn(async move {
    sender.run_async(rx, data_tx).await
});

// In Sender:
pub async fn run_async<F>(self, mut rx: FileJobReceiver, tx: mpsc::Sender<Bytes>) -> Result<()> {
    while let Some(msg) = rx.recv().await {
        // ... process ...
        tx.send(bytes).await?;  // Proper async send
    }
}
```

**Estimated benefit**: -100% runtime blocking, better concurrency
**Effort**: Medium (3-4 hours)

---

#### [MEDIUM] pipeline.rs:188-200 - spawn_blocking for scan_dest

```rust
tokio::task::spawn_blocking(move || {
    let rt = tokio::runtime::Handle::current();
    let receiver = Receiver::new(ReceiverConfig { ... });
    rt.block_on(receiver.scan_dest(|bytes| {
        data_tx
            .blocking_send(bytes)
            .map_err(|_| anyhow::anyhow!("Data channel closed"))
    }))
}).await??;
```

**Issue**: Mixing spawn_blocking with block_on is anti-pattern. Also uses blocking_send.

**Suggested optimization**:

```rust
// Keep scan_dest fully async
let receiver = Receiver::new(ReceiverConfig { ... });
let (count_tx, count_rx) = tokio::sync::oneshot::channel();

tokio::spawn(async move {
    let result = receiver.scan_dest_async(|bytes| {
        data_tx.send(bytes)
    }).await;
    count_tx.send(result);
});

// Process in parallel
while let Some(bytes) = data_rx.recv().await {
    writer.write_all(&bytes).await?;
}
```

**Estimated benefit**: Cleaner async flow, -50% spawn overhead
**Effort**: Medium (4-6 hours)

---

## Memory Usage Estimate (1M Files)

| Component              | Per-Entry | 1M Files   | After Optimization |
| ---------------------- | --------- | ---------- | ------------------ |
| Scanner FileEntry      | 160 bytes | 160 MB     | 80 MB (path dedup) |
| Generator String alloc | 50 bytes  | 50 MB      | 0 MB (Cow)         |
| DestIndex              | 80 bytes  | 80 MB      | 80 MB              |
| Channel buffers        | -         | 30 MB      | 10 MB              |
| Protocol buffers       | -         | 50 MB      | 25 MB              |
| **Total**              |           | **370 MB** | **195 MB**         |

Plus working set for concurrent operations: ~100-150 MB

**Current estimate**: ~500-600 MB for 1M files
**After optimizations**: ~300-350 MB

---

## Recommendations by Priority

### Immediate (High Impact, Low Effort)

| Issue                       | File:Line        | Estimated Impact         |
| --------------------------- | ---------------- | ------------------------ |
| Reuse read buffer           | sender.rs:137    | -95% buffer allocs       |
| Reuse delta apply buffer    | receiver.rs:339  | -95% delta allocs        |
| Arc clone not PathBuf clone | generator.rs:143 | -50% path allocs         |
| Pre-size delta bytes        | sender.rs:210    | -50% delta serial allocs |

### Short-term (High Impact, Medium Effort)

| Issue                      | File:Line       | Estimated Impact    |
| -------------------------- | --------------- | ------------------- |
| Async callback in pipeline | pipeline.rs:104 | No runtime blocking |
| Batch checksum computation | receiver.rs:78  | -80% spawn overhead |
| BytesMut for data chunks   | sender.rs:154   | -50% data allocs    |

### Medium-term (Medium Impact, Medium Effort)

| Issue                  | File:Line        | Estimated Impact   |
| ---------------------- | ---------------- | ------------------ |
| Buffered frame reading | protocol.rs:1020 | -50% frame allocs  |
| Path type in protocol  | sender.rs:50     | -30% string allocs |
| Fully async scan_dest  | pipeline.rs:188  | Cleaner flow       |

---

## Validation Plan

1. **Baseline measurement**:

   ```bash
   # Memory profile with 100k files
   RUST_BACKTRACE=1 cargo run --release -- /tmp/test-100k /tmp/dest-100k -v

   # Use heaptrack or valgrind for allocation analysis
   heaptrack ./target/release/sy /tmp/test-100k /tmp/dest-100k
   ```

2. **After each optimization**:

   ```bash
   # Run streaming protocol benchmark
   cargo bench --bench scale_bench -- streaming

   # Memory check
   /usr/bin/time -l ./target/release/sy /tmp/test-100k /tmp/dest-100k
   ```

3. **Target metrics**:
   - Time to first byte: <0.5s (measure with strace/dtruss)
   - Memory (1M files): <500MB (measure with /usr/bin/time -l)
   - SSH small files: benchmark against rsync

---

## Architectural Observations

The streaming protocol v2 is fundamentally sound - it follows rsync's Generator->Sender->Receiver pattern. The performance issues are implementation details, not architectural problems.

Key insights:

1. The `spawn_blocking` pattern is overused - should be reserved for truly blocking operations
2. String/path allocations dominate memory for large file counts
3. The channel-based pipeline could benefit from `FuturesUnordered` for better concurrency
4. Protocol message types use owned Strings - could use borrowed or Bytes

The biggest wins will come from:

1. Buffer reuse (read buffer, delta buffer, protocol buffer)
2. Path allocation reduction (Arc clone, Cow strings)
3. Removing blocking calls from async context
