# sy 0.5 architecture

`sy` 0.5 is a clean architectural cut, not an incremental migration of the 0.4 internals. Preserve useful user-facing semantics, but do not preserve internal APIs, transport layers, wire compatibility, or implementation structure solely for compatibility.

The target is one bounded synchronization engine that works across local filesystems, SSH hosts, and eventually object stores.

## Design principles

1. **One engine.** Local and remote synchronization share scanning, reconciliation, planning, scheduling, integrity, and safety semantics.
2. **One binary.** `sy` acts as both the user-facing client and the private remote agent. There is no `sy-remote` in 0.5.
3. **Bounded memory.** File size and tree size must not imply proportional resident memory on the common path.
4. **Transactional visibility.** Destination mutations are staged and verified before commit whenever the backend can support atomic replacement.
5. **Capabilities, not endpoint branches.** Transfer policy is chosen from explicit backend capabilities and workload characteristics.
6. **Cheap metadata first.** Expensive metadata, hashes, signatures, sparse extents, ACLs, and xattrs are demand-driven.
7. **Strong end-to-end integrity.** Remote transfers are BLAKE3-verified before commit by default. Size alone is never proof of correctness.
8. **Deletion is a commit phase.** An incomplete source view can never authorize deletion.
9. **The source is read-only.** `--remove-source-files` is the only exception and runs only after a confirmed destination commit.
10. **SSH is the authentication substrate, not the sync architecture.** Use the user's OpenSSH implementation and configuration rather than embedding a second SSH stack.

## Target module shape

The exact file split may evolve, but ownership should converge on this shape:

```text
src/
  cli/
  engine/
    session.rs
    reconcile.rs
    planner.rs
    scheduler.rs
    delete_journal.rs
  endpoint/
    mod.rs
    local.rs
    remote.rs
    object.rs
  transfer/
    whole.rs
    reflink.rs
    sparse.rs
    delta.rs
  protocol/
    frame.rs
    handshake.rs
    message.rs
    mux.rs
    client.rs
    server.rs
  remote/
    ssh.rs
    bootstrap.rs
    root.rs
  metadata/
  filter/
  integrity/
  bisync/
  main.rs
  lib.rs
```

Legacy `sync/`, `streaming/`, `transport/`, and `sy-remote` are temporary scaffolding only while equivalent 0.5 components come online.

## Core pipeline

```text
source EntryStream ─┐
                    ├─> Reconciler ─> SyncOp ─> Planner ─> WorkItem ─> Scheduler
  dest EntryStream ─┘                                             │
                                                                 ├─ whole copy
                                                                 ├─ reflink patch
                                                                 ├─ sparse copy
                                                                 ├─ rolling delta
                                                                 ├─ object multipart/server copy
                                                                 └─ metadata only
```

The reconciler decides **what semantic change is required**. The planner decides **how to perform it**. The scheduler decides **when work is allowed to run**.

Local/local, local/remote, and remote/local differ in endpoint implementation, not in reconciliation semantics:

```text
LocalScan  + LocalScan  -> Reconciler
LocalScan  + RemoteScan -> Reconciler
RemoteScan + LocalScan  -> Reconciler
```

## Paths and roots

All engine-internal paths are validated paths relative to an endpoint root. Do not pass arbitrary absolute paths through transfer/reconciliation APIs.

Introduce a strong relative-path type at the boundary instead of passing unconstrained `PathBuf`/`String` values through the engine. Wire paths must not require UTF-8 on Unix; preserve raw filesystem bytes where the platform supports them.

Remote roots are sent in the protocol handshake, not interpolated into an SSH shell command.

## Endpoint model

An endpoint owns a rooted namespace and semantic operations. The minimum core contract is:

- ordered bounded entry stream
- stat/metadata lookup
- streaming reader
- transactional staged writer
- create directory/link operations
- safe deletion
- demand-driven metadata reads/writes
- explicit capability discovery

The final core must not contain a whole-tree `scan() -> Vec<Entry>` fallback.

Capability assertions are contracts and require conformance tests. A backend must not advertise a capability that its implementation silently ignores.

Capabilities include, as applicable:

- atomic replacement
- streaming read/write
- staged pre-commit verification
- random read/write
- reflink/clone
- sparse extents
- server-side copy
- xattrs
- ACLs
- hard links
- platform flags
- modification-time precision
- rolling signatures
- whole-file hashing

## Entry metadata

The reconciliation entry should be lean. Base fields are roughly:

```text
relative path
entry kind
size
mtime
mode/permissions when needed for comparison semantics
symlink target when preserving links
```

Additional metadata is requested only when policy requires it:

- hardlink identity only with hardlink preservation
- xattrs only for an entry that will need xattr preservation
- ACLs only for an entry that will need ACL preservation
- BSD/platform flags only when requested
- sparse extents only when a chosen transfer strategy needs them
- content hashes only for checksum comparison or integrity work

Do not eagerly compute sparse layout, xattrs, ACLs, block signatures, or whole-file hashes for every scan entry.

## Reconciliation

Source and destination entry streams are emitted in deterministic relative-path order and merge-joined in bounded memory.

The reconciler emits semantic operations such as:

```text
CreateDirectory
CreateFile
UpdateFile
ReplaceEntry
CreateSymlink
CreateHardlink
ApplyMetadata
Delete
Skip
```

Type transitions must eventually be transactional rather than rejected or performed as remove-then-create. The local replacement sequence is:

```text
prepare staged replacement
rename old destination -> same-directory tombstone
rename staged replacement -> destination
rollback old destination if second rename fails
remove tombstone after commit
```

Backend-specific implementations may provide a stronger single-step primitive.

### Source mutation / TOCTOU

A scan describes a candidate snapshot, not a guarantee that the source remains unchanged. Regular-file transfer should validate source identity when opening and again at completion using stable metadata available on the platform (for example device/inode/size/mtime/ctime). If the source changes during transfer, retry or fail that entry rather than commit an inconsistent snapshot.

## Exact bounded deletion

Do not use a full source `HashSet`, and do not rely on Bloom-filter false positives in the final design.

During a complete no-mutation preflight merge, append exact destination-only deletion candidates to an on-disk journal. The journal is bounded in RAM and stores records that can be replayed in reverse without keeping an offset index, for example:

```text
[record payload][record_len:u32]
```

After both scans complete successfully:

1. calculate the deletion threshold against the actual eligible destination scope;
2. reject before mutations if the threshold is exceeded;
3. perform non-delete work;
4. replay the delete journal in reverse/depth-safe order;
5. remove the journal.

Excluded descendants protect their ancestors from recursive deletion. Any source scan error disables deletion.

## Scheduler and backpressure

Every internal queue is bounded. The scheduler owns resource budgets rather than relying only on a file-count semaphore.

At minimum budget:

- active files
- bytes buffered/in flight
- metadata operations
- hashing/compression CPU work
- network frames/writes

Large files consume more of the byte budget than small files. A few multi-gigabyte transfers must not multiply resident memory by the nominal concurrency value.

Blocking filesystem/syscall work that is not genuinely asynchronous must stay off Tokio worker threads. CPU parallelism and I/O concurrency are separate controls.

## Transaction model

Visible destination state changes only at commit:

```text
prepare staging
  -> transfer / reconstruct / patch
  -> apply metadata that belongs on staging
  -> verify staged contents
  -> atomically commit
  -> apply only metadata that must be post-commit
```

Failure before commit leaves the previous destination visible. Temporary files use same-directory staging when atomic rename semantics require it.

For remote transfers, a disconnect or protocol failure must abort staging, not leave a partially updated destination.

## Local transfer strategies

### New regular file

Prefer the operating system's optimized copy primitive into same-filesystem staging. Avoid userspace read/write loops when the OS can perform the copy more efficiently.

### Changed regular file on a COW filesystem

For sufficiently large files with a low estimated change ratio:

1. reflink/clone the old destination into staging;
2. compare source and old destination in large blocks;
3. overwrite only changed ranges in the clone;
4. verify if required;
5. commit staging.

This preserves unchanged extents without making a rolling network delta the default local strategy.

### Non-COW changed file

Prefer an optimized sequential whole copy. Reading both old destination and source merely to reconstruct a local file is often slower than replacing it.

### Sparse files

Detect sparse layout on demand after the sparse strategy is selected. Preserve holes using native extent APIs where possible.

## Remote transport

0.5 uses external OpenSSH as the default transport:

```text
ssh -T <host> sy __serve
```

Benefits:

- honors normal `~/.ssh/config` behavior
- SSH agent/security-key support comes from the user's SSH
- ProxyJump/ControlMaster and other OpenSSH features work naturally
- removes the embedded `ssh2`/libssh2/OpenSSL stack
- simplifies static musl distribution

The user-provided remote root is never a shell argument. It is part of the binary protocol handshake.

## Protocol v3

Protocol v3 is a clean break. Do not extend v2 for compatibility.

### Framing

Frames have a fixed bounded header and a hard maximum payload size. A representative shape is:

```text
payload_len:u32
kind:u8
flags:u8
reserved:u16
stream_id:u32
payload
```

Decoders validate lengths before allocation and reject oversized/unknown-invalid input with typed protocol errors. Protocol/property tests must exercise truncated and adversarial frames.

### Handshake

The client sends:

```text
protocol version/range
build identity
operation (push/pull)
requested root
requested semantics
client capabilities
```

The server returns:

```text
selected protocol
build identity
OS/architecture
filesystem/protocol capabilities
negotiated semantics
```

Do not guess remote capabilities from endpoint type.

### Metadata phase

Exchange cheap ordered metadata first. No block signatures are included in the initial tree walk.

The same merge reconciler consumes the remote metadata stream while it arrives.

### On-demand signatures

Only changed large files that are plausible delta candidates request destination signatures:

```text
SignatureRequest(stream_id, path, block_size)
Signature(...)
Signature(...)
SignatureEnd
```

Signatures are generated and transmitted incrementally.

Block size is adaptive. Start with a target of roughly 4096 blocks per file, rounded to a power of two and clamped around 4 KiB..1 MiB; benchmark and tune rather than hardcoding 4096 bytes.

### Delta streaming

The network path never constructs `Delta { ops: Vec<_> }`.

```text
source reader
  -> rolling matcher
  -> DeltaOp
  -> bounded queue
  -> protocol frame
  -> network
```

The receiver applies operations directly into staging. Copy operations reference the old destination; literal operations carry bounded data chunks.

### Multiplexing

Every independent file/signature transfer has a `stream_id`. One SSH byte stream can interleave work without requiring multiple SSH connections:

```text
file A signatures
file B whole-file data
file C metadata
file D delta literals
```

The scheduler controls fairness and byte budgets. Frame sizes remain moderate so one large file cannot monopolize the connection.

### Compression

Compression is negotiated and applied only where it earns its cost. Do not compress copy operations or signatures. Literal/file-data chunks may be compressed when sampling/extension policy predicts a benefit, and the wire flag is set only when compression actually reduced the payload.

## Integrity

### Remote

BLAKE3 is part of the normal remote data path:

```text
source bytes -> network
            \-> BLAKE3
```

The receiver hashes the fully reconstructed staged file. The sender's final file message carries the expected digest. A mismatch aborts staging before commit.

This is default transfer integrity, not merely a `--verify` feature.

### Local

Do not force a second full-file read after OS-native local copies by default; that can erase the performance advantage of native copy primitives. `--verify` can request strong pre-commit/post-copy verification when the user wants the extra I/O.

## Remote root security

Lexical `..` checks are insufficient because existing symlink components can escape a root.

The remote agent must operate relative to a held root directory handle and use platform-safe resolution:

- Linux: prefer `openat2` with `RESOLVE_BENEATH` and appropriate no-follow/no-magic-link flags.
- macOS/other Unix: walk components using directory FDs/openat-style APIs with no-follow semantics.

No remote mutation may follow a preexisting path component outside the negotiated root.

## One binary and remote bootstrap

0.5 removes `sy-remote`. The same executable has a private `__serve` entrypoint used only as the remote protocol peer.

Portable Linux release artifacts should include at least:

```text
x86_64-unknown-linux-musl
aarch64-unknown-linux-musl
```

alongside platform-native builds where useful. CI verifies the musl artifacts are actually static.

A later bootstrap layer may make remote installation automatic:

1. try `sy __serve` on the remote;
2. negotiate protocol/build compatibility;
3. if unavailable, detect remote OS/architecture;
4. obtain the matching trusted static agent;
5. upload to a versioned user cache;
6. verify its digest and mode;
7. execute the cached agent.

Do not upload the helper on every sync. Preinstalled/offline operation must remain possible.

## Object stores

S3/GCS-style endpoints come after the filesystem/SSH engine is stable. They use backend-native semantics:

- multipart upload
- ranged reads
- server-side copy
- backend checksums/ETags where semantically valid
- object-level atomic visibility

Do not model object stores as POSIX filesystems just to reuse code.

## CLI

0.5 may preserve familiar rsync-like user syntax while replacing the parser implementation. The final CLI should follow the repository `rust-cli` guidance: one `usage-rs` facade dependency, typed arguments, and generated help/manpage/completion artifacts rather than hand-maintained clap/manpage glue.

CLI migration should happen after the new engine boundaries are stable enough that parser work is not mixed with transfer/protocol debugging.

## Explicit removals before 0.5 release

The following are not part of the target architecture and should be deleted once their 0.5 replacements are live:

- `src/bin/sy-remote.rs`
- protocol v2 and the old `streaming/` generator/sender/receiver pipeline
- legacy `transport/`
- embedded `ssh2` connection stack and homegrown SSH config parser
- legacy `SyncEngine<T: Transport>`
- legacy `StrategyPlanner`
- whole-tree scan fallbacks in the core endpoint contract
- duplicate stats/planning/transfer representations
- compatibility-only caches and databases that are not proven useful in the new engine
- obsolete tests that assert 0.4 implementation details or log strings rather than user-visible semantics

## Non-goals by default

Do not add these unless measurement demonstrates a need:

- QUIC
- io_uring
- content-defined chunking
- a persistent distributed state service
- rsync wire compatibility
- multiple custom SSH implementations

## 0.5 release gates

0.5 is ready only when:

- format and Clippy are warning-free on all targets/features;
- Linux and macOS semantic/integration tests are green;
- cargo audit/deny are green;
- protocol decoding has adversarial/property coverage and bounded allocations;
- remote path confinement has dedicated escape/symlink tests;
- interrupted transfers prove old destinations survive;
- remote files are BLAKE3-verified before commit;
- deletion tests prove incomplete scans cannot delete data;
- Miri is run over relevant unsafe/path/reflink components where practical;
- realistic benchmarks cover many-small-files, huge files, unchanged trees, low/high change ratios, local COW/non-COW, and representative SSH links;
- release CI builds and verifies static musl Linux binaries;
- all replaced 0.4 architecture is actually removed rather than left as a second path.
