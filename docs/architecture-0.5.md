# sy 0.5 architecture

`sy` 0.5 treats synchronization as a streaming reconciliation problem with capability-driven transfer strategies. The goal is to preserve rsync's useful mental model while avoiding whole-file buffering, endpoint-specific orchestration, and unnecessary remote work.

## Core pipeline

```text
source scan ──┐
              ├─> reconciler ─> sync-op stream ─> transfer planner ─> scheduler
 dest scan ───┘                                             │
                                                            ├─ whole copy
                                                            ├─ reflink patch
                                                            ├─ rolling delta
                                                            ├─ server-side copy
                                                            └─ metadata only
```

The common path should remain bounded in memory. Scanning, reconciliation, transfer planning, data movement, hashing, compression, and protocol framing should all apply backpressure rather than accumulating complete trees or complete files.

## Endpoint contract

An endpoint describes a namespace and the operations it supports. Transfer policy must be selected from capabilities rather than from endpoint type checks.

The v0.5 endpoint I/O contract is:

- streaming readers (`open_reader`)
- transactional staged writers (`begin_write`)
- explicit capability discovery
- semantic metadata operations

Whole-file `Vec<u8>` methods are migration shims only and should disappear once existing code paths have moved to the streaming contract.

Important capabilities include:

- atomic replacement
- streaming and random reads
- staged and random writes
- reflink / clone support
- sparse-file support
- server-side copies
- xattr / ACL / hardlink preservation
- modification-time precision

## Transactional writes

A destination write is staged privately and becomes visible only on commit:

```text
begin_write(path)
    -> write chunks
    -> apply metadata
    -> verify
    -> commit
```

Failure or abort should leave the previous destination intact whenever the backend can provide atomic replacement semantics. Local files stage in the destination directory so rename stays on the same filesystem.

## Scanning and reconciliation

The target design replaces `Vec<FileEntry>` tree snapshots with deterministic ordered entry streams. Source and destination streams can then be reconciled as a merge join instead of building full destination hash maps and task vectors.

Some features need targeted global state (for example hardlink groups), but the normal path must not pay full-tree memory costs for them.

Reconciliation answers only whether a path needs a semantic operation. It must not choose the byte-transfer algorithm.

## Transfer strategies

The transfer planner chooses an implementation from source/destination capabilities and file characteristics.

### Local

- new file: optimized whole-file copy into staging
- changed file with reflink: clone destination to staging and patch changed ranges
- changed file without reflink: optimized sequential whole-file copy
- metadata-only change: no data copy

A rolling rsync delta is generally not the default local strategy because reconstruction often costs more than a sequential filesystem copy unless reflink lets unchanged extents remain shared.

### SSH

Remote synchronization uses a two-stage protocol:

1. exchange cheap path/type/size/mtime/metadata information
2. request block signatures only for changed candidate files

Block sizes are selected per file and signatures are streamed. Delta generation emits copy/literal operations directly into a bounded wire queue; it never constructs a complete in-memory `Delta`.

### Object stores

Object stores should use backend-native primitives (multipart upload, ranged reads, server-side copy, backend checksums) rather than pretending to be POSIX filesystems.

## Integrity

Hashing is part of the data path rather than a whole-file post-pass whenever possible.

- rolling checksum: candidate block lookup only
- strong block hash: confirm candidate block matches
- BLAKE3 whole-file digest: end-to-end transfer verification

A successful transfer must never be inferred from file size alone.

## Backpressure and scheduling

Every queue is bounded. The scheduler eventually owns separate resource budgets for:

- metadata operations
- active files
- bytes in flight
- CPU hashing/compression work
- network writes

Large-file concurrency should be byte-weighted so a few huge files cannot create unbounded resident memory.

## Deletion safety

Deletion is a commit phase, not interleaved blindly with transfers. Source scan errors or other conditions that make the source view incomplete disable deletion unless explicitly overridden. Percentage thresholds are calculated against the actual deletion scope after filtering.

## Migration order

1. Introduce streaming readers and staged writers.
2. Move local file transfer off whole-file `Vec<u8>` buffers.
3. Introduce capability-driven transfer planning and restore reflink/sparse fast paths.
4. Make scanning/reconciliation incremental.
5. Redesign SSH metadata/signature negotiation and bounded delta streaming.
6. Add integrated strong verification.
7. Remove legacy `Transport` and whole-file endpoint APIs once bisync and auxiliary paths migrate.
8. Add object-store-native strategies only after the core architecture is stable.

The 0.5 branch is allowed to break internal APIs to reach this shape. CLI compatibility should be preserved unless there is a concrete user-facing reason to change it.
