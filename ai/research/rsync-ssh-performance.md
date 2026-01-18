# How rsync Achieves SSH Performance

**Date**: 2026-01-18
**Context**: Understanding rsync's architecture to identify how sy could match or beat it

---

## The Core Insight

rsync's SSH performance comes from **architectural commitment to streaming**, not clever tricks. After the initial connection (1 round-trip), everything flows unidirectionally with no acknowledgments in the critical path.

---

## Key Techniques

### 1. Three-Process Pipeline

rsync runs three concurrent processes that stream to each other:

```
Generator ──────► Sender ──────► Receiver
(destination)     (source)       (destination)

- Walks files     - Computes      - Writes files
- Sends checksums   deltas        - No acks back
- No waiting      - Streams data
```

Each process sends as fast as possible. No synchronization between files.

### 2. Incremental Recursion (Protocol 30+)

**Before rsync 3.0**: Build complete file list, then transfer. Millions of files = minutes of delay.

**After rsync 3.0**: Stream file list entries as directories are scanned. Transfer begins after first few directories. Scanning and transfer run concurrently.

```
Time →
Old:  [====== scan ======][====== transfer ======]
New:  [scan──────────────────────]
           [transfer────────────────]
```

### 3. Fire-and-Forget Messages

The protocol is designed for one-way streaming:
- Generator emits checksum requests continuously
- Sender emits delta data continuously
- Receiver writes files continuously
- **No round-trips between files**
- TCP handles flow control

### 4. No Packet Framing

rsync's wire protocol has no formal structure - just bytes with context-dependent parsing:
- No per-message headers
- No length prefixes
- No acknowledgment packets
- Minimal protocol overhead

Tradeoff: "extremely difficult to document, debug or extend"

---

## What This Means for sy

### Current sy Architecture (Request-Response)

```
Client                          Server
   │                               │
   ├── CHECKSUM_REQ ──────────────►│
   │◄────────────── CHECKSUM_RESP ─┤  ← Round-trip
   ├── FILE_DATA ─────────────────►│
   │◄────────────── FILE_DONE ─────┤  ← Round-trip
   │                               │
```

Pipeline depth of 8 means 8 files in flight, but still fundamentally request-response.

### What rsync Does

```
Generator                    Sender                     Receiver
    │                           │                           │
    ├── checksums ─────────────►├── delta ─────────────────►│
    ├── checksums ─────────────►├── delta ─────────────────►│
    ├── checksums ─────────────►├── delta ─────────────────►│
    (no waiting)                (no waiting)                (no waiting)
```

No round-trips. Each stage just streams.

---

## Options to Beat rsync

| Option | Description | Impact | Effort | Breaks Protocol |
|--------|-------------|--------|--------|-----------------|
| A | Adopt rsync streaming model | High | Very High | Yes |
| B | Deeper pipelining (8→100+) | Medium | Low | No |
| C | Daemon mode (PR #13) | High (repeated) | Medium | No |
| D | Incremental recursion | High | High | Partial |
| E | Hybrid (B+C+D) | High | High | Partial |

### Option A: Full Streaming Model

Fundamental redesign to match rsync:
1. Separate into generator/sender/receiver
2. Remove all request-response
3. Let TCP handle flow control
4. Implement incremental recursion

### Option B: Deeper Pipelining

Incremental improvement:
1. Increase pipeline depth 8 → 64+
2. Adaptive depth based on RTT
3. Batch small files

### Option C: Daemon Mode

From PR #13 - eliminates startup overhead:
1. Persistent server on Unix socket
2. SSH socket forwarding
3. 3.5x faster repeated syncs

### Option D: Incremental Recursion

Start transferring before scan completes:
1. Stream file list as discovered
2. Interleave scanning and transfer
3. Complicates delete/resume logic

### Option E: Hybrid

Combine B + C + D for cumulative gains.

---

## Recommendation

**Practical path (without protocol rewrite):**
1. Daemon mode → 3.5x for repeated syncs
2. Deeper pipelining (8→64) → ~20-30% throughput
3. Incremental recursion → major latency reduction

**To truly beat rsync:**
Requires Option A - streaming model. Current request-response has inherent latency floor.

---

## Key Takeaway

rsync is fast because it designed for streaming from day one. sy's request-response model with pipelining is fundamentally different. We can approach rsync with optimizations, but matching it requires architectural change.
