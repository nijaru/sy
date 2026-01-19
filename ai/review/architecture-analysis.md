# Architecture Analysis: sy File Synchronization

**Date**: 2026-01-18
**Context**: Evaluating PR with GCS support, daemon mode, and Python bindings
**Scope**: Module structure, transport layer, extension points, coupling

---

## Executive Summary

The sy architecture is **moderately extensible** for new transports but **not designed for daemon mode or FFI bindings**. The Transport trait provides a clean abstraction for storage backends (adding GCS is straightforward), but daemon mode and Python bindings would require significant architectural changes.

| Feature         | Difficulty | Recommendation                      |
| --------------- | ---------- | ----------------------------------- |
| GCS Transport   | Low        | Accept with minor changes           |
| Daemon Mode     | High       | Defer - requires event architecture |
| Python Bindings | High       | Defer - library separation needed   |

---

## 1. Module Structure

**Layout:**

```
src/
  main.rs           CLI entry
  lib.rs            Public exports (24 modules)
  error.rs          thiserror-based errors
  path.rs           SyncPath enum (Local/Remote/S3)

  sync/             Core sync engine
    mod.rs          SyncEngine<T: Transport>
    scanner.rs      FileEntry, parallel scanning
    strategy.rs     StrategyPlanner
    transfer.rs     Transferrer
    server_mode.rs  Binary protocol client
    checksumdb.rs   fjall-backed checksum cache

  transport/        Storage backends
    mod.rs          Transport trait (~580 lines, 25+ methods)
    local.rs        LocalTransport (COW-aware, delta sync)
    ssh.rs          SshTransport (connection pool, SFTP)
    s3.rs           S3Transport (object_store, multipart)
    server.rs       ServerSession (binary protocol client)
    dual.rs         DualTransport (cross-transport ops)
    router.rs       TransportRouter enum

  server/           Remote server mode (sy --server)
  delta/            rsync algorithm
  integrity/        xxHash3, BLAKE3
  compress/         zstd, lz4
  filter/           gitignore patterns
  bisync/           Bidirectional sync state
```

**Observations:**

1. Monolithic SyncEngine with 30+ config parameters - high coupling
2. Clean Transport trait with sensible defaults - easy to extend
3. Feature flags for SSH, S3, ACL, watch - good pattern
4. No library crate - all code in single binary crate

---

## 2. Transport Layer Analysis

### Transport Trait Quality

**Strengths:**

- 25+ methods with default implementations
- `FileInfo` type abstracts `std::fs::Metadata`
- Streaming support (`scan_streaming`)
- Progress callbacks
- Batching APIs (`create_dirs_batch`, `bulk_copy_files`)

**Weaknesses:**

1. Large surface area (25+ methods)
2. `metadata()` returns `std::fs::Metadata` - cannot construct for remote files
3. Path assumptions don't map cleanly to object store keys
4. No unified authentication abstraction

### TransportRouter Pattern

```rust
pub enum TransportRouter {
    Local(LocalTransport),
    Dual(DualTransport),
    #[cfg(feature = "s3")]
    S3(S3Transport),
}
```

Adding a new transport requires:

1. Implement Transport trait
2. Add variant to TransportRouter enum
3. Add match arms in 20+ methods (boilerplate)
4. Add path parsing to SyncPath enum
5. Add feature flag

**Issue**: TransportRouter has boilerplate match arms. Could use `Box<dyn Transport>` instead.

---

## 3. Extension Points

### Adding GCS Transport (Low Difficulty)

1. Add `gcs` feature flag in Cargo.toml
2. Create `src/transport/gcs.rs` implementing Transport trait
3. Add `SyncPath::Gcs { bucket, key, project }` variant
4. Update TransportRouter and path.rs

**Note**: `object_store` crate already supports GCS. Could refactor S3Transport to ObjectStoreTransport and reuse.

### Adding Daemon Mode (High Difficulty)

Current architecture is batch-oriented (parse args -> sync -> exit).

Daemon mode requires:

- Long-running event loop
- Job lifecycle management
- IPC mechanism (socket/HTTP)
- State persistence
- Signal handling

**Not a bolt-on feature. Requires fundamental rearchitecture.**

### Adding Python Bindings (High Difficulty)

Issues:

1. No library crate
2. SyncEngine requires owned Transport (not object-safe)
3. Heavy async usage - PyO3 async is complex
4. Configuration via CLI args, not API

**Requires library extraction and API design first.**

---

## 4. Component Coupling

### High Coupling

- SyncEngine constructor: 30+ parameters
- main.rs directly configures SyncEngine from CLI
- server_mode.rs tightly coupled to protocol/SSH

### Low Coupling

- Transport trait implementations are independent
- delta/, integrity/, compress/ are self-contained utilities

---

## 5. PR Recommendations

### GCS Support: Accept

If PR uses `object_store` (like S3Transport), integration is clean:

- Rename or share with S3Transport
- Add path parsing for `gs://` URLs
- Feature flag: `gcs = ["object_store/gcp"]`
- Require integration tests

### Daemon Mode: Defer

Requires fundamental architectural changes. Should be planned as separate phase after v1.0 stabilization.

### Python Bindings: Defer

Requires:

- Library crate extraction first
- API design separate from CLI
- Async/sync boundary handling

---

## 6. Architectural Improvements (Future)

| Improvement                | Benefit                      | Effort |
| -------------------------- | ---------------------------- | ------ |
| SyncEngine builder pattern | Reduce constructor params    | Low    |
| Config struct layer        | Separate CLI from core       | Medium |
| `Box<dyn Transport>`       | Reduce match arm boilerplate | Low    |
| Library crate extraction   | Enable bindings              | High   |

---

## Conclusion

**GCS**: Accept with review - fits current architecture
**Daemon**: Defer to future phase - needs event architecture
**Python**: Defer - extract library first

---

**Key Files for Reference:**

- `src/transport/mod.rs` - Transport trait definition (580 lines)
- `src/transport/s3.rs` - Template for cloud transports (380 lines)
- `src/transport/router.rs` - Transport dispatch (440 lines)
- `src/path.rs` - Path parsing with SyncPath enum (470 lines)
- `src/sync/mod.rs` - SyncEngine with high coupling (800+ lines)
