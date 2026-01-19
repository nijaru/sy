# Performance Improvement Plan for v0.3.0

**Goal**: Beat rsync on ALL SSH metrics (currently 1.3-1.6x slower on incremental/delta)

**Date**: 2026-01-18

---

## Current Performance Gap

| Scenario                | Current Gap | Target |
| ----------------------- | ----------- | ------ |
| SSH small_files initial | 1.6x slower | parity |
| SSH small_files delta   | 1.4x slower | parity |
| SSH large_file delta    | 1.3x slower | parity |
| SSH source_code delta   | 1.4x slower | parity |

Root cause: Request-response protocol with depth-8 pipelining vs rsync's streaming model.

---

## Phase 1: Quick Wins (Low Effort, Medium Impact)

### 1.1 Adaptive Pipeline Depth

**Impact**: +20-40% throughput on WAN

**Location**: `/Users/nick/github/nijaru/sy/src/sync/server_mode.rs:23`

**Current**:

```rust
/// Number of delta checksum requests to pipeline before reading responses
const PIPELINE_DEPTH: usize = 8;
```

**Change**: Adaptive depth based on file count and measured RTT

```rust
/// Base pipeline depth (increases with latency/file count)
const PIPELINE_DEPTH_MIN: usize = 8;
const PIPELINE_DEPTH_MAX: usize = 64;

fn compute_pipeline_depth(file_count: usize, rtt_ms: Option<u64>) -> usize {
    // For many small files, deeper pipelining amortizes latency
    let base = if file_count > 500 {
        32
    } else if file_count > 100 {
        16
    } else {
        PIPELINE_DEPTH_MIN
    };

    // If we have RTT estimate, scale further for high latency
    match rtt_ms {
        Some(rtt) if rtt > 50 => base.min(PIPELINE_DEPTH_MAX),
        Some(rtt) if rtt > 20 => (base * 2).min(PIPELINE_DEPTH_MAX),
        _ => base,
    }
}
```

**Files to modify**:

- `src/sync/server_mode.rs`: Add RTT measurement in handshake, pass to pipeline depth calc
- `src/server/protocol.rs`: Add RTT field to HELLO response (server echoes timestamp)

**RTT Measurement**:

```rust
// In ServerSession::handshake()
let send_time = Instant::now();
hello.write(&mut self.stdin).await?;
self.stdin.flush().await?;
// ... read response ...
let rtt = send_time.elapsed();
tracing::debug!("SSH RTT: {:?}", rtt);
self.rtt_ms = Some(rtt.as_millis() as u64);
```

### 1.2 Batch Small File Transfers

**Impact**: +30-50% for many small files

**Location**: `/Users/nick/github/nijaru/sy/src/sync/server_mode.rs:148-199`

**Current**: Each file sent with full FILE_DATA framing overhead.

**Change**: Batch files under 8KB into single FILE_DATA messages

```rust
const SMALL_FILE_THRESHOLD: u64 = 8 * 1024;
const BATCH_SIZE_LIMIT: usize = 256 * 1024; // 256KB max batch

struct BatchedFileData {
    entries: Vec<(u32, Vec<u8>)>, // (index, data)
    total_size: usize,
}

impl BatchedFileData {
    fn can_add(&self, size: usize) -> bool {
        self.total_size + size <= BATCH_SIZE_LIMIT && self.entries.len() < 64
    }
}
```

**Protocol change**: New message type `FILE_DATA_BATCH` (0x14):

```
[len:u32][type:0x14][count:u16][
  [index:u32][size:u16][data:bytes]...
]
```

**Server change**: Unpack batch into individual file writes.

---

## Phase 2: Zero-Copy Data Path (Medium Effort, High Impact)

### 2.1 Arc<[u8]> for File Data

**Impact**: -50% memory allocations, +15-20% throughput

**Location**: `/Users/nick/github/nijaru/sy/src/sync/server_mode.rs:185`

**Current**:

```rust
session
    .send_file_data_with_flags(*idx, 0, *flags, data.clone())
    .await?;
```

Problem: `data.clone()` copies entire file contents.

**Change**: Use `Arc<[u8]>` or `bytes::Bytes` for zero-copy reference counting

```rust
// In protocol.rs
pub struct FileData {
    pub index: u32,
    pub offset: u64,
    pub flags: u8,
    pub data: bytes::Bytes,  // Changed from Vec<u8>
}

// In server_mode.rs
let data = bytes::Bytes::from(file_contents);
session.send_file_data_with_flags(*idx, 0, *flags, data).await?;
```

**Files to modify**:

- `src/server/protocol.rs`: Change `FileData.data` type
- `src/sync/server_mode.rs`: Use `bytes::Bytes`
- `src/transport/server.rs`: Update method signatures

**Dependency**: Add `bytes = "1"` to Cargo.toml

### 2.2 Scanner Path Deduplication

**Impact**: +5-10% scan speed, -40% scan memory

**Location**: `/Users/nick/github/nijaru/sy/src/sync/scanner.rs:259-274`

**Current**:

```rust
Ok(FileEntry {
    path: Arc::new(path),           // Arc #1
    relative_path: Arc::new(relative_path),  // Arc #2
    ...
})
```

Two `Arc<PathBuf>` per entry is wasteful since `relative_path = path.strip_prefix(root)`.

**Change**: Store only `path`, derive relative path on demand

```rust
pub struct FileEntry {
    pub path: Arc<Path>,  // Use Arc<Path> not Arc<PathBuf>
    pub root_prefix_len: usize,  // Cache strip point
    // Remove relative_path field
    ...
}

impl FileEntry {
    pub fn relative_path(&self) -> &Path {
        &self.path.as_ref()[self.root_prefix_len..]
    }
}
```

**Files to modify**:

- `src/sync/scanner.rs`: Change struct, update all construction
- `src/sync/server_mode.rs`: Update usages
- `src/sync/engine.rs`: Update usages
- Tests: Many will need updating

**Alternative (simpler)**: Keep both but use string interning for common prefixes.

---

## Phase 3: Protocol Optimizations (Medium Effort, Medium Impact)

### 3.1 Pre-allocated Protocol Buffers

**Impact**: -20% allocations, smoother throughput

**Location**: `/Users/nick/github/nijaru/sy/src/server/protocol.rs:186-212`

**Current**: Every message write creates new `Vec<u8>`:

```rust
pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
    let mut payload = Vec::new();  // Allocation per message
    payload.write_u32(self.entries.len() as u32).await?;
    ...
}
```

**Change**: Thread-local or session-scoped buffer pool

```rust
thread_local! {
    static WRITE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(64 * 1024));
}

pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<()> {
    WRITE_BUF.with(|buf| {
        let mut payload = buf.borrow_mut();
        payload.clear();  // Reuse capacity
        // ... write to payload ...
    })
}
```

### 3.2 Delta Window Ring Buffer

**Impact**: -30% CPU in delta generation

**Location**: `/Users/nick/github/nijaru/sy/src/delta/generator.rs:200-202`

**Current**:

```rust
// Shift window: remove processed bytes
window.drain(0..window_pos);
window_pos = 0;
```

`Vec::drain(0..n)` shifts entire buffer. For 256KB chunks, this is 256KB of memmove.

**Change**: Use ring buffer

```rust
use std::collections::VecDeque;

// Or custom ring buffer:
struct RingWindow {
    data: Vec<u8>,
    start: usize,  // Logical start position
    len: usize,    // Logical length
}

impl RingWindow {
    fn advance(&mut self, n: usize) {
        self.start = (self.start + n) % self.data.len();
        self.len -= n;
    }

    fn get(&self, logical_idx: usize) -> u8 {
        self.data[(self.start + logical_idx) % self.data.len()]
    }
}
```

**Files to modify**:

- `src/delta/generator.rs`: Replace `Vec<u8>` window with ring buffer
- Tests: Verify identical delta output

---

## Phase 4: Daemon Mode (High Effort, Very High Impact)

### 4.1 Persistent Server Process

**Impact**: 3.5x faster for repeated syncs (eliminates 2.5s startup)

**Design**: Server stays running, reused across syncs.

**Components**:

1. **Local daemon**: Unix socket at `$XDG_RUNTIME_DIR/sy.sock`
2. **SSH forwarding**: Forward local socket to remote daemon
3. **Protocol extension**: Keep-alive heartbeat, session multiplexing

**New files**:

- `src/daemon/mod.rs`: Daemon lifecycle
- `src/daemon/server.rs`: Unix socket server
- `src/daemon/client.rs`: Connect to daemon
- `src/daemon/forward.rs`: SSH socket forwarding

**CLI changes**:

```
sy daemon start [--port PORT]     # Start daemon
sy daemon stop                    # Stop daemon
sy daemon status                  # Check daemon
sy /src host:/dst --use-daemon    # Use daemon if available
```

**Protocol addition**:

```
KEEPALIVE (0x30): [timestamp:u64]
SESSION_START (0x31): [session_id:u64][root_path:string]
SESSION_END (0x32): [session_id:u64]
```

**Estimated implementation**: 1000+ lines, significant testing needed.

---

## Phase 5: Incremental Recursion (Very High Effort, High Impact)

### 5.1 Stream File List During Scan

**Impact**: 2-3x faster initial sync of large directories

**Current flow**:

```
1. Scan entire source → Vec<FileEntry>
2. Send complete FILE_LIST
3. Wait for FILE_LIST_ACK
4. Begin transfers
```

**New flow**:

```
1. Start scanning
2. As each directory completes:
   - Send FILE_LIST_CHUNK(entries)
   - Begin transferring those files
3. Continue scanning while transferring
4. Send FILE_LIST_END when done
```

**Protocol additions**:

```
FILE_LIST_CHUNK (0x15): [chunk_id:u32][entries...]
FILE_LIST_CHUNK_ACK (0x16): [chunk_id:u32][decisions...]
FILE_LIST_END (0x17): [total_files:u32]
```

**Complexity**: Delete handling, resume state, ordering guarantees.

**Recommendation**: Defer to v0.4.0+ after daemon mode proves out.

---

## Implementation Order

| Phase | Change                  | Impact        | Effort    | Blocks   |
| ----- | ----------------------- | ------------- | --------- | -------- |
| 1.1   | Adaptive pipeline depth | +20-40%       | Low       | -        |
| 1.2   | Batch small files       | +30-50% small | Low       | -        |
| 2.1   | Arc<[u8]> zero-copy     | +15-20%       | Medium    | -        |
| 2.2   | Scanner path dedup      | +5-10% scan   | Medium    | -        |
| 3.1   | Pre-allocated buffers   | +10%          | Low       | -        |
| 3.2   | Ring buffer delta       | +5-10% delta  | Medium    | -        |
| 4.1   | Daemon mode             | 3.5x repeated | High      | 1.1, 2.1 |
| 5.1   | Incremental recursion   | 2-3x initial  | Very High | 4.1      |

---

## Success Criteria

### v0.3.0 Targets (Phases 1-3)

| Scenario                | Current     | Target | Metric    |
| ----------------------- | ----------- | ------ | --------- |
| SSH small_files initial | 1.6x slower | parity | benchmark |
| SSH small_files delta   | 1.4x slower | parity | benchmark |
| Memory per file         | ~2KB        | ~1KB   | profiler  |
| Delta CPU               | baseline    | -30%   | profiler  |

### v0.4.0 Targets (Phase 4)

| Scenario              | Current | Target | Metric    |
| --------------------- | ------- | ------ | --------- |
| Repeated sync startup | 2.5s    | <0.7s  | benchmark |
| Memory idle daemon    | N/A     | <50MB  | profiler  |

---

## Specific Code Changes Summary

### src/sync/server_mode.rs

1. Line 23: Replace `PIPELINE_DEPTH` constant with `compute_pipeline_depth()` function
2. Line 185: Change `data.clone()` to `data` (with Bytes type)
3. Lines 148-199: Add batching for small files

### src/sync/scanner.rs

1. Lines 16-34: Remove `relative_path` field from `FileEntry`
2. Lines 259-274: Single Arc per entry, derive relative on demand

### src/delta/generator.rs

1. Lines 97-99: Replace `Vec<u8>` window with ring buffer
2. Lines 200-202: Remove `window.drain()`, use ring advance

### src/server/protocol.rs

1. Line 318-348: Change `FileData.data` from `Vec<u8>` to `bytes::Bytes`
2. All message writes: Use pre-allocated buffer pool
3. Add `FILE_DATA_BATCH` message type (0x14)

### src/transport/server.rs

1. Add `rtt_ms` field to `ServerSession`
2. Update `handshake()` to measure RTT
3. Update method signatures for Bytes type

---

## Benchmarking Plan

Before/after benchmarks for each phase:

```bash
# Local baseline
cargo build --release
./scripts/benchmark.py --local --scenarios all --runs 5

# SSH baseline
./scripts/benchmark.py --ssh user@host --scenarios all --runs 5

# After each phase, re-run both
```

Track metrics:

- Throughput (MB/s)
- Latency (time to first byte)
- Memory (peak RSS)
- Allocations (via dhat or heaptrack)

---

## Risk Assessment

| Risk                                 | Mitigation                                  |
| ------------------------------------ | ------------------------------------------- |
| Protocol changes break compatibility | Version negotiation in HELLO                |
| Batching complicates error handling  | Fail batch on any error, retry individually |
| Ring buffer bugs                     | Extensive fuzz testing                      |
| Daemon security                      | Unix socket perms, no network exposure      |

---

## Non-Goals for v0.3.0

- Full streaming model (rsync-style 3-process) - requires protocol rewrite
- QUIC transport - proven slower on fast networks
- Compression changes - current adaptive approach is sufficient
