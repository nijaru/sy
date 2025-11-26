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
| 0xFF | ERROR | both | Error with message |

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

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Protocol bugs | Extensive testing, version field |
| Deadlocks | Async I/O, proper buffering |
| Memory (large file lists) | Streaming, chunked lists |
| Security | No new attack surface (just stdin/stdout) |

## Success Metrics

- [ ] Initial sync 485K files: <2 min (vs rsync 9:21)
- [ ] Incremental sync: <10s for no-change case
- [ ] Memory: <100MB for 1M file sync

## Open Questions

1. Compression: per-message or stream-level?
2. Parallel file transfer: multiple SSH channels or single multiplexed?
3. Bidirectional: same protocol for push/pull?

---

**Status**: Planning
**Target**: v0.2.0
**Estimated effort**: 2-3 focused implementation sessions
