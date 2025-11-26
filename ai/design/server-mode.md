# sy --server Mode Design

## Overview

Custom protocol for SSH sync, replacing per-file SFTP with a multiplexed stream protocol.

## Why

| Current (SFTP) | With --server |
|----------------|---------------|
| 1 round-trip per file op | Pipelined, no wait |
| No batching | Batched file lists |
| No delta built-in | Native delta support |
| External tar workaround | Clean protocol |
| ~3 files/sec over WAN | Target: 1000+ files/sec |

## How rsync Does It

```
local$ rsync -avz src/ user@host:dest/
  → SSH spawns: rsync --server -vlogDtprze.iLsfxCIvu . dest/
  → Communication via stdin/stdout over SSH channel
  → Custom wire protocol (not SFTP)
```

rsync protocol features:
- File list sent in batch (not per-file queries)
- Rolling checksums for delta (rsync algorithm)
- Multiplexed control/data streams
- Pipelined - sender doesn't wait for acks

## sy --server Design

### Invocation

```bash
# Local side spawns remote:
sy /src user@host:/dest
  → ssh user@host sy --server /dest

# Remote sy reads commands from stdin, writes responses to stdout
```

### Protocol

Binary protocol over stdin/stdout. All messages length-prefixed.

```
┌─────────┬──────────┬─────────────┐
│ len: u32│ type: u8 │ payload     │
└─────────┴──────────┴─────────────┘
```

- `len`: Total message length including header (big-endian)
- `type`: Message type byte
- `payload`: Type-specific data

Message types:

| Type | Name | Direction | Purpose |
|------|------|-----------|---------|
| 0x01 | HELLO | both | Version handshake |
| 0x02 | FILE_LIST | L→R | Batch file metadata |
| 0x03 | FILE_LIST_ACK | R→L | What remote has/needs |
| 0x04 | FILE_DATA | L→R | File content (streamed) |
| 0x05 | FILE_DONE | R→L | Ack file received |
| 0x06 | MKDIR_BATCH | L→R | Create directories |
| 0x07 | DELETE_BATCH | L→R | Delete files |
| 0x08 | CHECKSUM_REQ | L→R | Request checksums for delta |
| 0x09 | CHECKSUM_RESP | R→L | Rolling checksums |
| 0x0A | DELTA_DATA | L→R | Delta-encoded file |
| 0x10 | PROGRESS | R→L | Transfer stats |
| 0xFF | ERROR | both | Error with message |

### Payload Formats

All multi-byte integers are big-endian. Strings are length-prefixed (u16 len + UTF-8 bytes).

#### HELLO (0x01)
```
┌──────────────┬───────────────┬────────────────┐
│ version: u16 │ flags: u32    │ capabilities[] │
└──────────────┴───────────────┴────────────────┘

version: Protocol version (currently 1)
flags:
  bit 0: supports_delta
  bit 1: supports_compression
  bit 2: supports_xattrs
  bit 3: supports_acls
capabilities: Reserved for future extension
```

#### FILE_LIST (0x02)
```
┌────────────┬─────────────────────────────────────┐
│ count: u32 │ entries[]                           │
└────────────┴─────────────────────────────────────┘

entry:
┌──────────────┬───────────┬───────────┬─────────┬───────────┐
│ path: string │ size: u64 │ mtime: i64│ mode: u32│ flags: u8 │
└──────────────┴───────────┴───────────┴─────────┴───────────┘

flags:
  bit 0: is_dir
  bit 1: is_symlink
  bit 2: is_hardlink
  bit 3: has_xattrs
```

#### FILE_LIST_ACK (0x03)
```
┌────────────┬─────────────────────────────────────┐
│ count: u32 │ decisions[]                         │
└────────────┴─────────────────────────────────────┘

decision:
┌───────────┬────────────┐
│ index: u32│ action: u8 │
└───────────┴────────────┘

action:
  0: SKIP (already exists, same)
  1: CREATE (new file)
  2: UPDATE (exists, different - delta candidate)
  3: DELETE (exists on remote, not in source)
```

#### FILE_DATA (0x04)
```
┌───────────┬───────────┬──────────────┐
│ index: u32│ size: u64 │ data: bytes  │
└───────────┴───────────┴──────────────┘

For large files, sent in chunks:
┌───────────┬───────────┬────────────┬──────────────┐
│ index: u32│ offset: u64│ len: u32  │ data: bytes  │
└───────────┴───────────┴────────────┴──────────────┘
```

#### FILE_DONE (0x05)
```
┌───────────┬────────────┬───────────────┐
│ index: u32│ status: u8 │ checksum: [32]│
└───────────┴────────────┴───────────────┘

status:
  0: OK
  1: CHECKSUM_MISMATCH
  2: WRITE_ERROR
  3: PERMISSION_DENIED
```

#### MKDIR_BATCH (0x06)
```
┌────────────┬─────────────┐
│ count: u32 │ paths[]     │
└────────────┴─────────────┘
```

#### DELETE_BATCH (0x07)
```
┌────────────┬─────────────────────────┐
│ count: u32 │ entries[]               │
└────────────┴─────────────────────────┘

entry:
┌──────────────┬──────────┐
│ path: string │ is_dir: u8│
└──────────────┴──────────┘
```

#### CHECKSUM_REQ (0x08)
```
┌───────────┬─────────────┐
│ index: u32│ block_size: u32│
└───────────┴─────────────┘
```

#### CHECKSUM_RESP (0x09)
```
┌───────────┬────────────┬─────────────────────────┐
│ index: u32│ count: u32 │ checksums[]             │
└───────────┴────────────┴─────────────────────────┘

checksum:
┌──────────────┬───────────────┐
│ weak: u32    │ strong: [16]  │  (adler32 + md5 prefix)
└──────────────┴───────────────┘
```

#### DELTA_DATA (0x0A)
```
┌───────────┬────────────┬─────────────────────────┐
│ index: u32│ op_count: u32│ operations[]          │
└───────────┴────────────┴─────────────────────────┘

operation:
  COPY:   0x00 │ block_index: u32
  INSERT: 0x01 │ len: u32 │ data: bytes
```

#### PROGRESS (0x10)
```
┌────────────────┬───────────────┬───────────────┐
│ files_done: u32│ bytes_done: u64│ files_total: u32│
└────────────────┴───────────────┴───────────────┘
```

#### ERROR (0xFF)
```
┌────────────┬────────────────┐
│ code: u16  │ message: string│
└────────────┴────────────────┘

codes:
  1: PROTOCOL_ERROR
  2: IO_ERROR
  3: PERMISSION_DENIED
  4: NOT_FOUND
  5: CHECKSUM_MISMATCH
  100+: Application-specific
```

### Flow

```
1. HELLO exchange (version, capabilities)
2. L→R: FILE_LIST (all source files metadata)
3. R→L: FILE_LIST_ACK (need/have/delete decisions)
4. L→R: MKDIR_BATCH (create all needed dirs)
5. For each file to transfer:
   - If delta possible: CHECKSUM_REQ → CHECKSUM_RESP → DELTA_DATA
   - Else: FILE_DATA (streamed)
   - R→L: FILE_DONE (async, pipelined)
6. L→R: DELETE_BATCH (if --delete)
7. Close
```

### Pipelining

Key optimization: don't wait for FILE_DONE before sending next file.

```
L→R: FILE_DATA(file1)
L→R: FILE_DATA(file2)  ← sent immediately, no wait
L→R: FILE_DATA(file3)
R→L: FILE_DONE(file1)  ← async acks
R→L: FILE_DONE(file2)
...
```

### Multiplexing

Single SSH channel, multiplexed streams:
- Control messages (small, prioritized)
- Data stream (bulk file content)

### Delta Sync

Use existing sy delta implementation:
1. Request rolling checksums from remote
2. Compute delta locally
3. Send DELTA_DATA (operations: copy_block, insert_data)

## Implementation Phases

### Phase 1: Basic Protocol (MVP)
- [ ] `sy --server` flag, reads stdin/writes stdout
- [ ] HELLO handshake
- [ ] FILE_LIST / FILE_LIST_ACK
- [ ] FILE_DATA streaming (no delta)
- [ ] Basic error handling
- Estimated: 500-800 lines of new code

### Phase 2: Directories & Delete
- [ ] MKDIR_BATCH
- [ ] DELETE_BATCH
- [ ] Symlink handling

### Phase 3: Delta & Checksums
- [ ] CHECKSUM_REQ/RESP
- [ ] DELTA_DATA
- [ ] Wire up existing delta code

### Phase 4: Polish
- [ ] Progress reporting (periodic stats message)
- [ ] Resume support
- [ ] Compression (zstd on wire)
- [ ] xattrs/ACLs batching

## Code Structure

```
src/
  server/
    mod.rs        # --server entry point
    protocol.rs   # Message types, serialization
    handler.rs    # Server-side message handling
  transport/
    server.rs     # Client-side: spawn ssh, speak protocol
```

## Backwards Compatibility

- Auto-detect: try `sy --server`, fall back to SFTP if not available
- Version negotiation in HELLO
- Remote sy version doesn't need to match exactly

## Error Handling

### Connection Errors
```
1. SSH connection fails → Fall back to SFTP transport
2. HELLO version mismatch → ERROR(PROTOCOL_ERROR), fall back to SFTP
3. Unexpected disconnect → Resume support (track last FILE_DONE index)
```

### Transfer Errors
```
1. FILE_DONE(CHECKSUM_MISMATCH) → Retry file (up to 3 times)
2. FILE_DONE(WRITE_ERROR) → Log, continue with next file, report at end
3. FILE_DONE(PERMISSION_DENIED) → Log, continue, report at end
```

### Protocol Errors
```
1. Unknown message type → ERROR(PROTOCOL_ERROR), abort
2. Malformed payload → ERROR(PROTOCOL_ERROR), abort
3. Timeout (no message for 60s) → ERROR(IO_ERROR), abort
```

### Graceful Degradation
```rust
// Pseudo-code for transport selection
async fn sync(src, dest) {
    if dest.is_ssh() {
        match try_server_mode(dest).await {
            Ok(conn) => return server_sync(conn, src, dest).await,
            Err(e) => {
                tracing::info!("Server mode unavailable: {}, using SFTP", e);
                // Fall through to SFTP
            }
        }
    }
    sftp_sync(src, dest).await
}
```

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Protocol bugs | Extensive testing, version field, fallback to SFTP |
| Deadlocks | Async I/O, proper buffering, timeouts |
| Memory (large file lists) | Streaming in chunks of 10K files |
| Security | No new attack surface (just stdin/stdout over SSH) |
| Version skew | HELLO negotiates common features |

## Success Metrics

- [ ] Initial sync 485K files: <2 min (vs rsync 9:21)
- [ ] Incremental sync: <10s for no-change case
- [ ] Memory: <100MB for 1M file sync
- [ ] Fallback works: SFTP if remote sy unavailable/old

## Design Decisions

### Q1: Compression - per-message or stream-level?
**Decision: Stream-level zstd**
- Wrap entire stdin/stdout in zstd stream after HELLO
- HELLO negotiates compression support
- Better compression ratio than per-message
- Simpler implementation

### Q2: Parallel transfer - multiple channels or multiplexed?
**Decision: Single multiplexed channel (Phase 1), optional parallel (Phase 4)**
- Single channel is simpler, avoids connection overhead
- Pipelining provides most of the benefit
- Multiple channels add complexity (ordering, flow control)
- Can add `--parallel-channels N` later if bottlenecked

### Q3: Bidirectional - same protocol for push/pull?
**Decision: Yes, same protocol with role flag**
- HELLO includes `mode: u8` (0=push, 1=pull)
- Push: local scans, sends FILE_LIST, sends FILE_DATA
- Pull: remote scans, sends FILE_LIST, sends FILE_DATA
- Simplifies implementation and testing

---

**Status**: Planning
**Target**: v0.2.0
**Estimated effort**: 2-3 focused implementation sessions
