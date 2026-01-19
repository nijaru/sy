# Code Review: Streaming Protocol v2 & Delta Sync

**Reviewer:** Gemini CLI Agent
**Date:** 2026-01-19
**Scope:** `src/streaming/`, `src/delta/`, `src/sync/server_mode.rs`

## Overview

A thorough review of the Protocol v2 implementation reveals a high-performance architecture that successfully implements pipelined streaming. However, several critical safety and performance issues were identified that could lead to crashes (OOM) or protocol violations when handling large-scale datasets or large individual files.

---

## Critical Issues (Must Fix)

### 1. Memory Exhaustion in Delta Generation
- **Location:** `src/delta/generator.rs:56` (`generate_delta_streaming`)
- **Severity:** ERROR
- **Issue:** The implementation claims "constant memory usage" but accumulates all `DeltaOp`s in a `Vec`. If a file is large and has no matches, the `literal_buffer` and `ops` vector grow linearly with file size.
- **Risk:** System OOM when syncing multi-gigabyte files with significant changes.
- **Fix:** Refactor to an iterator-based or callback-based approach that yields `DeltaOp`s as they are generated, flushing literals periodically.

### 2. Protocol Frame Size Violation
- **Location:** `src/streaming/sender.rs:170` (`send_delta`)
- **Severity:** ERROR
- **Issue:** All delta operations for a file are serialized into a single `Data` message. If the serialized delta operations exceed `MAX_FRAME_SIZE` (64MB), the frame is rejected by the receiver.
- **Risk:** Permanent sync failure for large, highly fragmented files.
- **Fix:** Chunk large delta operation lists into multiple sequential `Data` messages.

---

## Performance & Scalability Issues

### 3. Sequential Checksumming in Initial Exchange
- **Location:** `src/streaming/receiver.rs:136` (`scan_dest`)
- **Severity:** WARN
- **Issue:** `compute_checksums` is called sequentially inside the scan loop.
- **Impact:** Significant latency bottleneck. The "Initial Exchange" phase should utilize a parallel task pool to compute checksums for multiple files simultaneously.

### 4. Excessive Syscalls in Receiver
- **Location:** `src/streaming/receiver.rs:434` (`apply_delta_static`)
- **Severity:** WARN
- **Issue:** The "original" source file is opened and closed for every `Data` chunk received during a delta sync.
- **Impact:** Massive syscall overhead (thousands of opens/closes for GB-sized files).
- **Fix:** Cache the original file handle in the `PendingFile` struct until the file transfer is complete.

### 5. Blocking Scanner Latency
- **Location:** `src/streaming/generator.rs:77` (`Generator::run`)
- **Severity:** WARN
- **Issue:** The scanner builds the entire file list in memory before sending the first `FileJob`.
- **Impact:** High "Time to First Byte" (TTFB) and high peak memory usage for large repositories (e.g., 1M+ files).
- **Fix:** Refactor `Scanner` to stream entries via a channel or iterator as they are discovered.

---

## Security & Robustness

### 6. Frame Allocation Denial of Service
- **Location:** `src/streaming/protocol.rs:777` (`read_frame`)
- **Severity:** WARN
- **Issue:** `read_frame` allocates a `Vec<0u8; len]` immediately after reading the length header.
- **Risk:** A malicious or corrupted peer can trigger immediate allocation of 64MB of memory.
- **Fix:** Read data into a pre-allocated/pooled buffer or use `BytesMut` with a controlled growth strategy.

---

## Minor Improvements (Nits)

### 7. Hardcoded File Modes
- **Location:** `src/streaming/receiver.rs:159`
- **Issue:** Metadata for file modes is lost in pull syncs because the `Scanner` doesn't currently preserve `st_mode`.

### 8. Unstructured Shutdown
- **Location:** `src/streaming/pipeline.rs:114`
- **Issue:** The client sends a dummy `Done` message to signal completion, which isn't cleanly handled by the receiver's loop logic.

---

## Verdict: Needs Work

The core logic is sound, but the OOM risks and protocol violations are regressions from the stability of v1. Addressing issues #1 and #2 is a priority for the next release.
