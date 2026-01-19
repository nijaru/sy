# Streaming Protocol Design for sy v0.3.0

**Goal**: Replace request-response model with rsync-style streaming to eliminate latency floor on WAN connections.

**Date**: 2026-01-18

---

## Executive Summary

Current sy uses request-response with depth-8 pipelining. This creates inherent round-trip latency that cannot be overcome regardless of optimizations. rsync achieves superior WAN performance through unidirectional streaming with no ACKs in the critical path.

This design replaces sy's protocol entirely with a streaming model using three concurrent Tokio tasks (generator/sender/receiver) communicating via bounded channels.

---

## Architecture

### Process Model: Tokio Tasks

Use Tokio tasks instead of OS processes:

- Lower memory overhead (shared heap vs fork)
- Built-in async I/O
- Easier error propagation
- Single binary deployment

```
Push (local -> remote):
+--------------+     +--------------+     +--------------+
|  Generator   | --> |    Sender    | --> |   Receiver   |
| (local scan) |     | (delta/data) |     | (remote write)|
+--------------+     +--------------+     +--------------+
     |                     |                     |
     v                     v                     v
  scan files         compute deltas        write files
  send metadata      stream data           set permissions
  no waiting         no waiting            errors to log

Pull (remote -> local):
+--------------+     +--------------+     +--------------+
|  Generator   | --> |    Sender    | --> |   Receiver   |
| (remote scan)|     | (remote read)|     | (local write)|
+--------------+     +--------------+     +--------------+
```

### Key Insight

**No ACKs in critical path.** FILE_DONE messages are informational only - they update progress counters and error logs but never block the sender. TCP handles flow control at the transport layer.

---

## Initial Exchange (Before Streaming)

For delta sync to work without round-trips during transfer, the sender needs destination file checksums upfront. This happens during the HELLO exchange:

### Push Flow (local → remote)

```
CLIENT (local)                    SERVER (remote)
     |                                 |
     |-------- HELLO (push) --------->|
     |                                 | [scan dest]
     |<------- DEST_FILE_ENTRY -------|  (path, size, mtime, checksums)
     |<------- DEST_FILE_ENTRY -------|
     |<------- DEST_FILE_END ---------|
     |                                 |
     | [Generator has full dest state]|
     | [Streaming phase begins]       |
```

### Pull Flow (remote → local)

```
CLIENT (local)                    SERVER (remote)
     |                                 |
     |-------- HELLO (pull) --------->|
     | [scan local dest]              |
     |------- DEST_FILE_ENTRY ------->|  (checksums for delta)
     |------- DEST_FILE_END --------->|
     |                                 | [Generator has dest state]
     |                                 | [Streaming phase begins]
```

**Key point:** The initial exchange front-loads all destination metadata including block checksums for delta candidates. After this exchange completes, the streaming phase runs without any round-trips.

**Memory:** O(dest_files) for checksums. For 1M files with 64KB blocks on 1GB average file = ~16 checksums/file = ~400MB. Acceptable.

---

## Protocol v2 Messages

Protocol version 2. Clean break from v1 - no backward compatibility.

### Wire Format

Same framing as v1 for simplicity:

```
+----------+----------+-------------+
| len: u32 | type: u8 | payload     |
+----------+----------+-------------+
```

All multi-byte integers are big-endian. Strings are length-prefixed (u16 len + UTF-8).

### Message Types

| Type | Name            | Direction     | Purpose                          |
| ---- | --------------- | ------------- | -------------------------------- |
| 0x01 | HELLO           | bidirectional | Version/capability negotiation   |
| 0x02 | FILE_ENTRY      | G->S          | Source file metadata (streaming) |
| 0x03 | FILE_END        | G->S          | End of source file list          |
| 0x04 | DEST_FILE_ENTRY | R->G          | Dest file metadata + checksums   |
| 0x05 | DEST_FILE_END   | R->G          | End of dest file list            |
| 0x06 | DATA            | S->R          | File content (full or delta)     |
| 0x07 | DATA_END        | S->R          | End of data for one file         |
| 0x08 | DELETE          | G->R          | File to delete                   |
| 0x09 | DELETE_END      | G->R          | End of deletes                   |
| 0x0A | MKDIR           | G->R          | Directory to create              |
| 0x0B | SYMLINK         | G->R          | Symlink to create                |
| 0x0C | PROGRESS        | R->client     | Stats update (async)             |
| 0x0D | ERROR           | any           | Non-fatal error report           |
| 0x0E | FATAL           | any           | Fatal error, abort sync          |
| 0x0F | XATTR           | S->R          | Extended attributes for file     |
| 0x10 | DONE            | R->client     | Sync complete                    |

### Payload Formats

#### HELLO (0x01)

```
+---------------+------------+----------------+
| version: u16  | flags: u32 | root_path: str |
+---------------+------------+----------------+

version: 2 (protocol v2)
flags:
  bit 0: is_pull (1=pull, 0=push)
  bit 1: want_delete (--delete enabled)
  bit 2: want_checksum (--verify enabled)
  bit 3: want_compression (zstd on wire)
  bit 4: want_xattrs
  bit 5: want_acls
  bit 6-31: reserved
root_path: destination path (push) or source path (pull)
```

#### FILE_ENTRY (0x02)

```
+-------------+----------+-----------+---------+-----------+----------+
| path: str   | size: u64| mtime: i64| mode: u32| inode: u64| flags: u8|
+-------------+----------+-----------+---------+-----------+----------+
| [symlink_target: str if FLAG_SYMLINK]                               |
| [link_target: str if FLAG_HARDLINK]                                 |
+---------------------------------------------------------------------+

flags:
  bit 0: FLAG_DIR
  bit 1: FLAG_SYMLINK
  bit 2: FLAG_HARDLINK
  bit 3: FLAG_HAS_XATTRS
  bit 4: FLAG_SPARSE
```

Generator streams these continuously as scanning proceeds. No batching, no ACK.

**Note:** Receiver caches FILE_ENTRY metadata until corresponding DATA_END, then applies mtime/mode.

#### FILE_END (0x03)

```
+----------------+----------------+
| total_files: u64 | total_bytes: u64 |
+----------------+----------------+
```

Signals end of source file list. Sender knows no more files coming.

#### DEST_FILE_ENTRY (0x04)

Sent by Receiver during initial exchange. Includes block checksums for delta computation.

```
+-------------+----------+-----------+---------+----------+
| path: str   | size: u64| mtime: i64| mode: u32| flags: u8|
+-------------+----------+-----------+---------+----------+
| [checksums if FLAG_HAS_CHECKSUMS]                       |
+---------------------------------------------------------+

flags:
  bit 0: FLAG_DIR
  bit 1: FLAG_HAS_CHECKSUMS (file is delta candidate)

checksums (if FLAG_HAS_CHECKSUMS):
+---------------+-------------+-------------------+
| block_sz: u32 | count: u32  | entries: [...]    |
+---------------+-------------+-------------------+

checksum entry (20 bytes each):
+-------------+----------+----------+-------------+
| offset: u64 | weak: u32| strong: u64            |
+-------------+----------+----------+-------------+
```

Server computes checksums for files that are delta candidates (exist on dest, size > DELTA_MIN_SIZE). Generator uses these to determine which files need delta vs full transfer.

#### DEST_FILE_END (0x05)

```
+----------------+----------------+
| total_files: u64 | total_bytes: u64 |
+----------------+----------------+
```

Signals end of dest file list. Initial exchange complete, streaming phase begins.

#### DATA (0x06)

```
+------------+----------+----------+--------------+
| path: str  | offset: u64| flags: u8| data: bytes |
+------------+----------+----------+--------------+

flags:
  bit 0: FLAG_COMPRESSED (data is zstd compressed)
  bit 1: FLAG_DELTA (data is delta ops, not raw)
  bit 2: FLAG_FINAL (last chunk for this file)
```

If FLAG_DELTA is set, data contains delta operations:

```
delta_ops: [
  0x00 | offset: u64 | size: u32  // Copy from existing
  0x01 | len: u32 | bytes        // Insert literal
]
```

#### DATA_END (0x07)

```
+------------+-----------+
| path: str  | status: u8 |
+------------+-----------+

status:
  0: OK
  1: ERROR (receiver should log)
```

Sender sends this after all DATA chunks for a file. Receiver applies cached mtime/mode from FILE_ENTRY.

#### DELETE (0x08)

```
+------------+----------+
| path: str  | is_dir: u8|
+------------+----------+
```

Generator sends after FILE_END if --delete enabled. Lists files on dest not in source.

#### DELETE_END (0x09)

```
+--------------+
| count: u64   |
+--------------+
```

End of delete list.

#### MKDIR (0x0A)

```
+------------+---------+
| path: str  | mode: u32|
+------------+---------+
```

Generator sends as directories are discovered. Receiver creates immediately.

#### SYMLINK (0x0B)

```
+------------+--------------+
| path: str  | target: str  |
+------------+--------------+
```

#### PROGRESS (0x0C)

```
+-------------+--------------+------------------+------------------+
| files: u64  | bytes: u64   | files_total: u64 | bytes_total: u64 |
+-------------+--------------+------------------+------------------+
```

Receiver sends periodically (every 100 files or 10MB). Does not block sender.

#### ERROR (0x0D)

```
+------------+----------+--------------+
| path: str  | code: u16| message: str |
+------------+----------+--------------+

codes:
  1: IO_ERROR
  2: PERMISSION_DENIED
  3: NOT_FOUND
  4: CHECKSUM_MISMATCH
  5: DISK_FULL
  100+: application-specific
```

Non-fatal. Logged and sync continues.

#### FATAL (0x0E)

```
+----------+--------------+
| code: u16| message: str |
+----------+--------------+
```

Abort sync immediately. Used for protocol errors, connection issues.

#### XATTR (0x0F)

```
+------------+------------+---------------------------+
| path: str  | count: u16 | entries: [(name, value)]  |
+------------+------------+---------------------------+

entry:
+-------------+---------------+
| name: str   | value: bytes  |
+-------------+---------------+
```

Sent after DATA_END for files with FLAG_HAS_XATTRS. Receiver applies xattrs.

#### DONE (0x10)

```
+-------------+-------------+--------------+--------------+
| files_ok: u64| files_err: u64| bytes: u64 | duration_ms: u64|
+-------------+-------------+--------------+--------------+
```

Sync complete. Receiver sends when all DATA_END received and deletes processed.

---

## Data Flow

### Push Flow (local -> remote)

**Phase 1: Initial Exchange**

```
LOCAL                          SSH                           REMOTE
+-------------------+          |           +--------------------+
|    Generator      |          |           |     Receiver       |
+--------+----------+          |           +---------+----------+
         |                     |                     |
         |-------- HELLO ----->|-------- HELLO ----->|
         |                     |                     | [scan dest]
         |<-- DEST_FILE_ENTRY -|<-- DEST_FILE_ENTRY -| [with checksums]
         |<-- DEST_FILE_ENTRY -|<-- DEST_FILE_ENTRY -|
         |<--- DEST_FILE_END --|<--- DEST_FILE_END --|
         |                     |                     |
         | [has dest state]    |                     |
```

**Phase 2: Streaming Transfer**

```
LOCAL                          SSH                           REMOTE
+-------------------+          |           +--------------------+
|    Generator      |          |           |     Receiver       |
+--------+----------+          |           +---------+----------+
         |                     |                     |
         |---- FILE_ENTRY ---->|---- FILE_ENTRY ---->|
         |------ MKDIR ------->|------ MKDIR ------->|---> create dir
         |---- FILE_ENTRY ---->|---- FILE_ENTRY ---->|
         |---- FILE_END ------>|---- FILE_END ------>|
         |                     |                     |
+--------+----------+          |                     |
|     Sender        |          |                     |
+--------+----------+          |                     |
         |                     |                     |
         |------ DATA -------->|------ DATA -------->|---> write chunk
         |------ DATA -------->|------ DATA -------->|---> apply delta
         |---- DATA_END ------>|---- DATA_END ------>|---> set perms
         |                     |                     |
         |                     |                     |<--- PROGRESS
         |                     |                     |<--- ERROR (async)
         |                     |                     |
         |---- DELETE -------->|---- DELETE -------->|---> remove file
         |-- DELETE_END ------>|-- DELETE_END ------>|
         |                     |                     |<--- DONE
```

### Channel Design

```rust
// Generator -> Sender (internal, not on wire)
struct FileJob {
    path: Arc<Path>,
    size: u64,
    mtime: i64,
    mode: u32,
    need_delta: bool,
    checksums: Option<Vec<BlockChecksum>>,  // From DEST_FILE_ENTRY
}

// Fixed-size channels with backpressure
const GENERATOR_CHANNEL_SIZE: usize = 1024;  // File entries
const SENDER_CHANNEL_SIZE: usize = 64;       // Large chunks
```

Generator receives dest state during Initial Exchange, then scans source and sends FileJob to Sender. Sender reads local files, uses cached checksums to compute deltas, streams DATA to Receiver.

### Pull Flow (remote -> local)

Roles swapped:

- Generator runs on remote (scans source)
- Sender runs on remote (reads source files)
- Receiver runs on local (writes files)

Initial Exchange: Local sends DEST_FILE_ENTRY with checksums to remote.

### Bidirectional Flow

For bisync, run two independent unidirectional flows:

1. Push A -> B
2. Push B -> A (after 1 completes)

No interleaving - simpler conflict resolution.

---

## Backpressure Handling

### Principle

TCP handles wire-level flow control. Channel bounds handle task-level flow control.

```rust
// Generator slows when Sender can't keep up
let (file_tx, file_rx) = bounded::<FileJob>(GENERATOR_CHANNEL_SIZE);

// Sender slows when wire can't keep up (TCP backpressure)
// No explicit mechanism needed - async write blocks naturally
```

### Scenario: Slow Receiver

1. TCP receive buffer fills on receiver
2. TCP send buffer fills on sender
3. async write() blocks in Sender task
4. Sender channel fills (bounded)
5. Generator blocks trying to send FileJob
6. Scanning pauses automatically

Whole pipeline throttles to receiver speed without explicit coordination.

### Scenario: Fast Network, Slow Disk

Same mechanism - disk writes block receiver, TCP buffers fill, sender blocks.

---

## Delete Handling

**Problem**: How to know what exists on dest but not source without complete scan?

**Solution**: Use dest file list from Initial Exchange (see above).

1. During Initial Exchange, Receiver streams DEST_FILE_ENTRY for all dest files
2. Generator builds hash set of dest paths
3. During source scan, Generator marks paths that exist in both
4. After FILE_END, Generator emits DELETE for each dest path not in source

```
[Initial Exchange - already completed]
Generator has: HashSet<dest_paths>

[Streaming Phase]
Generator                    Receiver
    |                           |
    | FILE_ENTRY (src/a) ------>|  [remove "a" from dest_paths set]
    | FILE_ENTRY (src/b) ------>|  [remove "b" from dest_paths set]
    | FILE_END ---------------->|
    |                           |
    | [remaining in dest_paths] |
    | DELETE (dest/c) --------->|  (c was on dest, not in source)
    | DELETE_END -------------->|
```

Memory: O(dest_files) for hash set. Acceptable for typical syncs (<1M files = ~100MB).

For extreme cases (>1M files), stream to temp file and sort-merge.

---

## Resume/Checkpoint

### Strategy

Checkpoint state to resume file after interruption.

```rust
struct ResumeState {
    session_id: u64,
    last_complete_path: Option<String>,  // Last DATA_END received
    bytes_transferred: u64,
    files_done: HashSet<String>,         // Completed files
}
```

### Checkpoint Writes

Every 100 files or 10MB, Receiver writes checkpoint to `~/.sy/resume/<session_id>`.

### Resume Flow

1. On connect, client sends `session_id` in HELLO
2. Server checks for matching ResumeState
3. If found, Generator skips files in `files_done`
4. Sender resumes from last offset if partial

### Edge Case: File Changed During Interrupt

Compare mtime/size at resume. If different, transfer from scratch.

---

## Error Handling Without ACKs

### File-Level Errors

Receiver logs errors and continues. Sender never waits for confirmation.

```rust
// Receiver
match write_file(&path, &data).await {
    Ok(()) => stats.files_ok += 1,
    Err(e) => {
        send_error(&path, IO_ERROR, &e.to_string()).await;
        stats.files_err += 1;
    }
}
// Continue to next file regardless
```

### Sync-Level Errors

At DONE, Receiver reports summary:

- files_ok: successfully written
- files_err: failed (see ERROR messages)

Client decides whether to report success or failure based on error count.

### Protocol Errors

Unknown message type or malformed data -> FATAL -> abort immediately.

### Connection Lost

- Sender detects write error -> abort, exit
- Receiver detects read EOF -> write checkpoint -> exit
- Resume from checkpoint on reconnect

---

## Edge Cases

### Symlinks

Sent inline with FILE_ENTRY:

```
FILE_ENTRY(path="link", flags=FLAG_SYMLINK, symlink_target="target")
```

Receiver creates symlink directly. No DATA message for symlinks.

### Permissions

Mode sent in FILE_ENTRY. Receiver sets after DATA_END (file fully written).

```rust
// After all data written
std::fs::set_permissions(&path, Permissions::from_mode(entry.mode))?;
```

### Extended Attributes

If HELLO flags include want_xattrs:

1. FILE_ENTRY has FLAG_HAS_XATTRS set
2. After DATA_END, Sender sends XATTR message (0x0F)
3. Receiver applies xattrs after file is written

See XATTR message format in Protocol section above.

### Sparse Files

If FILE_ENTRY has FLAG_SPARSE:

1. DATA messages may have gaps (offset jumps)
2. Receiver uses SEEK_HOLE/SEEK_DATA or fallocate to preserve sparseness

```rust
// Receiver handling sparse
if entry.flags & FLAG_SPARSE != 0 {
    file.seek(SeekFrom::Start(data.offset))?;
    // Don't write zeros - leave as hole
    if !data.data.is_empty() {
        file.write_all(&data.data)?;
    }
}
```

### Hard Links

Tracked by inode. Generator detects same inode for different paths:

```
FILE_ENTRY(path="file1", inode=12345, ...)
FILE_ENTRY(path="file2", flags=FLAG_HARDLINK, link_target="file1")
```

Receiver creates hard link instead of copying data.

---

## Implementation Plan

### Files to Create

```
src/
  streaming/
    mod.rs           # Public API
    protocol.rs      # v2 message types
    generator.rs     # Scanner + metadata sender
    sender.rs        # File reader + delta computer
    receiver.rs      # File writer
    resume.rs        # Checkpoint state
    channel.rs       # Bounded channels + types
```

### Files to Modify

| File                      | Changes                              |
| ------------------------- | ------------------------------------ |
| `src/server/mod.rs`       | Dispatch to streaming handler for v2 |
| `src/transport/server.rs` | Protocol version negotiation         |
| `src/main.rs`             | Add `--protocol-v2` flag (temporary) |
| `src/sync/server_mode.rs` | Delegate to streaming module         |
| `Cargo.toml`              | No new deps (already have crossbeam) |

### Files to Delete (after v2 stable)

- `src/server/handler.rs` (v1 handler)
- `src/server/protocol.rs` (v1 messages)

### Implementation Order

#### Phase 1: Protocol Foundation (Week 1)

- [ ] `streaming/protocol.rs`: v2 message types with tests
- [ ] `streaming/channel.rs`: FileJob, channel types
- [ ] Handshake: v2 negotiation in HELLO

#### Phase 2: Generator (Week 1-2)

- [ ] `streaming/generator.rs`: Scanner integration
- [ ] Stream FILE_ENTRY as scanned (no batching)
- [ ] MKDIR inline with discovery
- [ ] Unit tests with mock channels

#### Phase 3: Sender (Week 2)

- [ ] `streaming/sender.rs`: Read files, compute deltas
- [ ] DATA chunking (256KB chunks)
- [ ] Integration with existing delta module
- [ ] Zero-copy with bytes::Bytes

#### Phase 4: Receiver (Week 2-3)

- [ ] `streaming/receiver.rs`: Write files
- [ ] Handle DATA, DATA_END, MKDIR, SYMLINK
- [ ] Permission/xattr application
- [ ] PROGRESS emission

#### Phase 5: Integration (Week 3)

- [ ] Wire up to existing SSH transport
- [ ] End-to-end push flow
- [ ] End-to-end pull flow
- [ ] Error propagation

#### Phase 6: Delete + Resume (Week 3-4)

- [ ] DELETE message flow
- [ ] Checkpoint writes
- [ ] Resume on reconnect

#### Phase 7: Polish (Week 4)

- [ ] Compression (zstd on wire)
- [ ] Benchmarks vs v1 and rsync
- [ ] Remove v1 code path

### Testing Strategy

#### Unit Tests

- Protocol roundtrip for each message type
- Channel backpressure behavior
- Delta computation correctness

#### Integration Tests

- Push: local -> local (via pipes)
- Pull: local -> local (via pipes)
- Delete: verify correct files removed
- Resume: interrupt mid-sync, verify continuation
- Sparse: verify holes preserved
- Symlinks: verify targets correct
- Permissions: verify mode set

#### Benchmark Tests

- 1000 small files (1KB)
- 100 medium files (1MB)
- 10 large files (100MB)
- Compare: v1 pipelined, v2 streaming, rsync

#### SSH Tests (manual)

- Mac -> Fedora via Tailscale
- Same scenarios as local
- Measure: throughput, latency to first byte

---

## Success Criteria

| Metric                  | Current v1  | Target v2  | rsync    |
| ----------------------- | ----------- | ---------- | -------- |
| SSH small_files initial | 1.6x slower | parity     | baseline |
| SSH small_files delta   | 1.4x slower | 10% faster | baseline |
| SSH large_file delta    | 1.3x slower | parity     | baseline |
| Time to first byte      | 2.5s        | <0.5s      | ~0.3s    |
| Memory (1M files)       | ~2GB        | <500MB     | ~300MB   |

---

## Risks and Mitigations

| Risk                 | Mitigation                                            |
| -------------------- | ----------------------------------------------------- |
| Increased complexity | Comprehensive tests, gradual rollout                  |
| Harder debugging     | Structured logging per task, session IDs              |
| Ordering issues      | Clear task responsibilities, no shared state          |
| Memory pressure      | Bounded channels, streaming (no full file list)       |
| Backward compat      | Protocol version negotiation, v1 fallback (temporary) |

---

## Alternatives Considered

### 1. Deeper Pipelining Only

Keep v1, increase pipeline depth 8 -> 64+.
**Rejected**: Still fundamentally request-response. Cannot eliminate ACK latency.

### 2. Multiple SSH Channels

Parallel transfers over multiple SSH channels.
**Rejected**: Adds complexity, ordering issues. Single stream with streaming is simpler and sufficient.

### 3. QUIC Transport

Replace TCP with QUIC for stream multiplexing.
**Rejected**: Measured 45% slower on fast networks. TCP+BBR is better.

### 4. OS Processes (like rsync)

Fork generator/sender/receiver as separate processes.
**Rejected**: Higher overhead, harder error handling. Tokio tasks are lighter and integrate better with existing async code.

---

## References

- rsync protocol documentation: https://rsync.samba.org/how-rsync-works.html
- sy v1 protocol: `/Users/nick/github/nijaru/sy/ai/design/server-mode.md`
- Performance analysis: `/Users/nick/github/nijaru/sy/ai/research/rsync-ssh-performance.md`
- Current implementation: `/Users/nick/github/nijaru/sy/src/sync/server_mode.rs`

---

**Status**: Ready for implementation
**Target**: v0.3.0
**Estimated effort**: 4 weeks focused development
