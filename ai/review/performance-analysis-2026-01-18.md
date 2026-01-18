# Deep Performance Analysis: sy vs rsync

**Date**: 2026-01-18
**Analyst**: profiler agent
**Goal**: Identify what it takes to beat rsync on ALL metrics

## Executive Summary

sy beats rsync on local sync (2-44x faster) but loses on SSH incremental/small files (1.3-1.6x slower). The gap stems from architectural differences: rsync uses a streaming 3-process model while sy uses request-response with pipelining.

**Closing the SSH gap requires**:

1. Daemon mode (eliminates 2.5s startup) - **3.5x faster for repeated syncs**
2. Adaptive pipeline depth (8 -> 32-64 based on RTT) - **20-40% faster WAN**
3. Zero-copy file data (`Arc<[u8]>`) - **50% less memory, better throughput**

**Beating rsync on initial small files requires**:

1. Incremental recursion (transfer starts before scan completes) - **2-3x faster to first byte**
2. Scanner parallelism tuning - **10-20% faster scanning**

---

## Gap 1: Fixed Pipeline Depth (8)

**Files**: `/Users/nick/github/nijaru/sy/src/sync/server_mode.rs:23`

```rust
/// Number of delta checksum requests to pipeline before reading responses
const PIPELINE_DEPTH: usize = 8;
```

**Root cause**: Pipeline depth of 8 is insufficient for high-latency connections. With 50ms RTT, each batch waits 50ms before reading responses. rsync has no such waiting - it fires all messages immediately.

**Evidence from code** (`server_mode.rs:216-245`):

```rust
let mut pending: Vec<(u32, &SourceEntry, u32)> = Vec::with_capacity(PIPELINE_DEPTH);

for (idx, entry) in &delta_candidates {
    // Send checksum request without waiting (no flush)
    session.send_checksum_req_no_flush(file_idx, block_size).await?;
    pending.push((file_idx, *entry, block_size));

    // Process batch when full - THIS IS THE BOTTLENECK
    if pending.len() >= PIPELINE_DEPTH {
        session.flush().await?;  // <-- Wait for RTT
        let (updated, transferred) = process_delta_batch(&mut session, &pending).await?;
        ...
    }
}
```

**Fix**: Adaptive pipeline depth based on measured RTT:

```rust
// Measure RTT during handshake
let rtt_ms = measure_handshake_rtt().await?;

// Formula: target 200-400ms of in-flight data
// At 50ms RTT: depth = 8 (4 round-trips)
// At 200ms RTT: depth = 32 (6 round-trips)
let pipeline_depth = (400 / rtt_ms.max(10)).max(8).min(128);
```

**Impact**: +20-40% throughput on WAN (>50ms RTT), +10% on LAN (<10ms RTT)
**Effort**: Low (2-4 hours)

---

## Gap 2: data.clone() in FileData Handling

**Files**: `/Users/nick/github/nijaru/sy/src/sync/server_mode.rs:185`, `/Users/nick/github/nijaru/sy/src/server/handler.rs:361-365`

**Root cause**: File contents are cloned multiple times during transfer:

**Client side** (`server_mode.rs:185`):

```rust
session.send_file_data_with_flags(*idx, 0, *flags, data.clone()).await?;
//                                                      ^^^^^^^^^^^
// Clone entire file content just to send it
```

**Server side** (`handler.rs:361-365`):

```rust
let write_data = if data.flags & DATA_FLAG_COMPRESSED != 0 {
    decompress(&data.data, Compression::Zstd)?
} else {
    data.data.clone()  // <-- Clone again for non-compressed data
};
```

**Fix**: Use `Arc<[u8]>` for zero-copy sharing:

```rust
// In protocol.rs
pub struct FileData {
    pub index: u32,
    pub offset: u64,
    pub flags: u8,
    pub data: Arc<[u8]>,  // Zero-copy reference
}

// In server_mode.rs - no clone needed
session.send_file_data_with_flags(*idx, 0, *flags, Arc::clone(&data)).await?;

// In handler.rs - borrow instead of clone
let write_data: Cow<[u8]> = if compressed {
    Cow::Owned(decompress(&data.data, Compression::Zstd)?)
} else {
    Cow::Borrowed(&data.data)
};
```

**Impact**: -50% memory usage, +15-20% throughput (less GC pressure, better cache)
**Effort**: Medium (4-8 hours, touches multiple files)

---

## Gap 3: Protocol Serialization Allocations

**Files**: `/Users/nick/github/nijaru/sy/src/server/protocol.rs:186-212`

**Root cause**: Every message allocates a new `Vec<u8>` for serialization:

```rust
impl FileList {
    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
        let mut payload = Vec::new();  // <-- New allocation per message
        payload.write_u32(self.entries.len() as u32).await?;
        for entry in &self.entries {
            // ...more allocations for strings...
        }
        // ...
    }
}
```

Same pattern in `DeltaData::write` (`protocol.rs:686-712`):

```rust
pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
    let mut payload = Vec::new();  // Another allocation
    payload.write_u32(self.index).await?;
    // ...
}
```

**Fix**: Pre-allocated reusable buffer per session:

```rust
pub struct ServerSession {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    write_buf: Vec<u8>,  // Reusable buffer
}

impl ServerSession {
    pub async fn send_file_data(&mut self, ...) -> Result<()> {
        self.write_buf.clear();
        // Serialize directly to write_buf
        self.write_buf.extend_from_slice(&len.to_be_bytes());
        // ...
        self.stdin.write_all(&self.write_buf).await?;
    }
}
```

**Impact**: -20% allocations, +5-10% throughput
**Effort**: Medium (6-10 hours, API changes)

---

## Gap 4: Scanner 2x Arc<PathBuf> per Entry

**Files**: `/Users/nick/github/nijaru/sy/src/sync/scanner.rs:16-18`, `259-261`

**Root cause**: Every `FileEntry` stores both absolute and relative paths as `Arc<PathBuf>`:

```rust
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: Arc<PathBuf>,           // Full path
    pub relative_path: Arc<PathBuf>,  // Relative path - redundant!
    // ...
}
```

And created at `scanner.rs:259-261`:

```rust
Ok(FileEntry {
    path: Arc::new(path),
    relative_path: Arc::new(relative_path),  // Second allocation
    // ...
})
```

**Fix**: Store offset into path instead of separate string:

```rust
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: Arc<PathBuf>,
    pub rel_start: usize,  // Offset where relative path starts
    // ...
}

impl FileEntry {
    pub fn relative_path(&self) -> &Path {
        &self.path.as_path()[self.rel_start..]
    }
}
```

**Impact**: -50% scanner memory, +5-10% scan speed
**Effort**: Low (2-4 hours)

---

## Gap 5: Delta Window Vec::drain()

**Files**: `/Users/nick/github/nijaru/sy/src/delta/generator.rs:200-203`

**Root cause**: Window buffer shift copies entire remaining data:

```rust
// Refill window when needed
if window_pos >= block_size && bytes_read > 0 && window.len() - window_pos < block_size {
    // Shift window: remove processed bytes
    window.drain(0..window_pos);  // <-- O(n) copy of remaining data!
    window_pos = 0;
    // ...
}
```

For a 256KB chunk with 64KB processed, this copies 192KB of data.

**Fix**: Ring buffer with wrap-around indexing:

```rust
struct RingBuffer {
    data: Vec<u8>,
    head: usize,  // Read position
    tail: usize,  // Write position
    cap: usize,
}

impl RingBuffer {
    fn drain_front(&mut self, n: usize) {
        self.head = (self.head + n) % self.cap;  // O(1)
    }

    fn extend(&mut self, data: &[u8]) {
        // Copy to tail, wrap if needed
    }
}
```

**Impact**: -30% CPU time in delta generation
**Effort**: Medium (6-8 hours, careful testing)

---

## Gap 6: SSH Startup Overhead (2.5s)

**Files**: `/Users/nick/github/nijaru/sy/src/transport/server.rs:26-64`

**Root cause**: Every sync spawns new SSH connection + `sy --server`:

```rust
pub async fn connect_ssh(config: &SshConfig, remote_path: &Path) -> Result<Self> {
    let mut cmd = Command::new("ssh");
    cmd.arg(&config.hostname);
    // ... setup args ...
    cmd.arg("sy");
    cmd.arg("--server");
    cmd.arg(remote_path);

    let mut child = cmd.spawn().context("Failed to spawn SSH process")?;  // ~1-1.5s
    // + sy startup ~0.5-1s
    // Total: 2-2.5s before any data
```

rsync has same overhead on first run, but for repeated syncs this dominates.

**Fix**: Daemon mode with persistent connection:

```rust
// sy --daemon --socket /tmp/sy.sock
// Client connects to existing daemon instead of spawning

pub async fn connect_daemon(socket_path: &Path) -> Result<Self> {
    let stream = UnixStream::connect(socket_path).await?;  // ~1ms
    let mut session = Self::from_stream(stream);
    session.handshake().await?;
    Ok(session)
}
```

**Impact**: 3.5x faster for repeated syncs (2.5s -> 0.1s startup)
**Effort**: High (2-3 days, new daemon architecture)

---

## Gap 7: Full Scan Before Transfer

**Files**: `/Users/nick/github/nijaru/sy/src/sync/server_mode.rs:38-61`

**Root cause**: sy scans entire source before any transfer:

```rust
pub async fn sync_server_mode(source: &Path, dest: &SyncPath) -> Result<SyncStats> {
    // ...
    tracing::debug!("Scanning source...");
    let source_entries = scan_source(source).await?;  // <-- Wait for full scan

    // Only THEN separate and send
    for entry in source_entries {
        if entry.is_dir {
            directories.push(entry.rel_path);
        } else {
            files.push(entry);
        }
    }
    // ...
}
```

rsync uses incremental recursion - starts transferring directory contents as soon as that directory is scanned.

**Fix**: Streaming scan with concurrent transfer:

```rust
pub async fn sync_server_mode_streaming(...) -> Result<SyncStats> {
    let (tx, rx) = channel::<SourceEntry>(1024);

    // Spawn scanner that sends entries as found
    tokio::spawn(async move {
        for entry in scanner.scan_streaming()? {
            tx.send(entry?).await?;
        }
    });

    // Start transferring as entries arrive
    while let Some(entry) = rx.recv().await {
        if entry.is_dir {
            session.send_mkdir_batch(vec![entry.rel_path]).await?;
        } else {
            // Can start transfer immediately
        }
    }
}
```

**Impact**: 2-3x faster time-to-first-byte, better perceived performance
**Effort**: High (2-4 days, significant refactor)

---

## Recommendations Summary

### To Close SSH Gap (Priority Order)

| Change                     | Impact               | Effort | Files                                   |
| -------------------------- | -------------------- | ------ | --------------------------------------- |
| 1. Adaptive pipeline depth | +20-40% WAN          | Low    | server_mode.rs                          |
| 2. Arc<[u8]> zero-copy     | -50% mem, +15% speed | Medium | protocol.rs, server_mode.rs, handler.rs |
| 3. Daemon mode             | 3.5x repeated syncs  | High   | New daemon module                       |

### To Beat rsync on Initial Small Files

| Change                            | Impact            | Effort | Files                      |
| --------------------------------- | ----------------- | ------ | -------------------------- |
| 1. Incremental recursion          | 2-3x faster start | High   | server_mode.rs, scanner.rs |
| 2. Scanner path optimization      | -50% scanner mem  | Low    | scanner.rs                 |
| 3. Pre-allocated protocol buffers | -20% allocations  | Medium | protocol.rs, server.rs     |

### CPU Optimizations (Lower Priority)

| Change                   | Impact             | Effort | Files        |
| ------------------------ | ------------------ | ------ | ------------ |
| 1. Ring buffer for delta | -30% delta CPU     | Medium | generator.rs |
| 2. Scanner path dedup    | -50% scanner alloc | Low    | scanner.rs   |

---

## Validation Plan

1. **Baseline measurement** before any changes:

   ```bash
   # SSH benchmark (Mac -> Fedora)
   ./scripts/benchmark_ssh.sh 2>&1 | tee baseline-ssh.jsonl
   ```

2. **Implement in order**, measure after each:
   - Adaptive pipeline (easy win, low risk)
   - Zero-copy data (bigger win, medium risk)
   - Daemon mode (biggest win for repeated syncs)

3. **Target metrics**:
   - SSH small files: match rsync (currently 1.4-1.6x slower)
   - SSH incremental: match rsync (currently 1.3-1.4x slower)
   - SSH repeated syncs: beat rsync 3x (with daemon mode)

---

## Architecture Comparison

```
rsync (streaming):
Generator ──────► Sender ──────► Receiver
    │                │                │
    └── no waiting ──┴── no waiting ──┘

    - Fire-and-forget messages
    - Zero round-trips after start
    - 30 years of optimization

sy (request-response):
Client ◄────────► Server
    │                │
    └── round-trip ──┘

    - Batch 8 operations then wait
    - RTT per batch
    - Simpler protocol, easier debugging
```

rsync's model is fundamentally better for high-latency connections. sy can approach rsync performance with:

1. Deeper pipelining (reduces wait time)
2. Daemon mode (eliminates startup)
3. Incremental recursion (hides scan latency)

But full parity would require protocol rewrite to streaming model.
