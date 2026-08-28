# sy — Fast File Sync

`sy /source/ /dest` — rsync's useful mental model, rebuilt around a modern bounded synchronization engine.

## For AI Agents

The `v0.5-architecture` branch is a ground-up rewrite. Do not infer the target design from old 0.4 modules.

Read, in order:

1. `docs/architecture-0.5.md` — canonical target architecture and invariants.
2. `agent-context/ai/0.5-rewrite.md` — current rewrite state, known debt, and implementation order.
3. `nijaru/dotfiles/dot_agents/skills/rust-expert/SKILL.md` — required Rust/systems guidance.
4. `nijaru/dotfiles/dot_agents/skills/rust-cli/SKILL.md` when changing the CLI.

If old code disagrees with the 0.5 architecture docs, the docs define the intended direction unless a new finding demonstrates a better design.

## Project

| Attribute | Value |
|-----------|-------|
| Language | Rust |
| Release target | v0.5.0 |
| Branch | `v0.5-architecture` |
| License | MIT |
| Positioning | Fast file synchronization with rsync-like ergonomics; not rsync wire-compatible |

The branch remains Edition 2021 while legacy code is being deleted. Edition 2024 should be evaluated as a deliberate migration after the new architecture owns the build.

## Target Architecture

```text
source EntryStream ─┐
                    ├─> Reconciler ─> SyncOp ─> Planner ─> WorkItem ─> Scheduler
  dest EntryStream ─┘                                             │
                                                                 ├─ whole copy
                                                                 ├─ reflink patch
                                                                 ├─ sparse copy
                                                                 ├─ rolling delta
                                                                 ├─ object-native copy
                                                                 └─ metadata only
```

There is one synchronization engine. Local and remote endpoints feed the same reconciler.

There is one executable. `sy` is both the user-facing client and the private remote agent (`sy __serve`). `sy-remote` is legacy and must be removed before 0.5.

SSH uses the user's external OpenSSH process as the transport substrate. The final architecture does not embed `ssh2` or maintain a second SSH configuration parser.

## Non-Negotiable Safety Invariants

`sy` mutates user data. Data loss is the highest-severity bug.

### Transactional destination visibility

For replacements, write privately, verify, then commit. A failure before commit must leave the previous destination intact whenever the backend supports atomic replacement.

```text
prepare staging
  -> transfer / reconstruct / patch
  -> metadata
  -> verify
  -> atomic commit
```

Never write a replacement directly into the visible destination.

### Source is read-only

Do not mutate source data during synchronization. `--remove-source-files` is permitted only after the destination commit and required verification/preservation succeed.

### Detect source races

A scanner result is not a snapshot guarantee. Transfer code must detect source replacement/modification between scan, open, and completion and retry or fail rather than commit inconsistent bytes.

### Deletion is final

Deletion is not interleaved casually with transfer work. The source scan must complete successfully and the exact delete threshold must pass before any delete is authorized.

Excluded/protected destination descendants must protect ancestor directories from recursive deletion.

The final design uses a bounded exact on-disk deletion journal rather than a full source set or probabilistic Bloom membership.

### Remote root confinement

Do not trust lexical path normalization as a security boundary. A preexisting symlink component can escape a root.

Remote operations must be relative to a held root directory handle:

- Linux: `openat2`/`RESOLVE_BENEATH`-style confinement.
- macOS/other Unix: component-wise directory-FD traversal with no-follow semantics.

No user filesystem path belongs in the SSH shell command; roots and relative paths are protocol data.

## Bounded Resource Use

The common path must not materialize whole files or whole trees.

Every queue is bounded. The scheduler owns budgets for:

- active files;
- buffered/in-flight bytes;
- metadata operations;
- CPU hashing/compression work;
- network frames/writes.

Do not equate one 20 GiB file with one 4 KiB file for concurrency purposes.

Blocking filesystem/syscall work belongs in appropriate blocking execution, not on Tokio worker threads.

## Reconciliation and Metadata

Ordered source/destination streams are merge-joined. Reconciliation decides semantic operations only; transfer planning decides byte strategy.

Keep scan entries lean. Do not eagerly collect expensive metadata just because a field exists in an old `FileEntry`:

- xattrs and ACLs are demand-driven;
- BSD/platform flags are demand-driven;
- hardlink identity is needed only when preserving hardlinks;
- sparse extents are transfer-time work;
- hashes/signatures are computed only when comparison/integrity/strategy requires them.

Type transitions must become transactional replacements rather than remove-then-create or permanent errors.

## Transfer Policy

### Local

- New files: native optimized copy into staging.
- Changed large files on COW filesystems: reflink old destination to staging and patch changed ranges when measurement predicts a win.
- Changed non-COW files: generally native whole copy rather than rsync-style local reconstruction.
- Sparse files: discover/copy native extents on demand.

### Remote

Protocol v3 is a clean break from v2:

1. Client/server handshake and capability negotiation.
2. Cheap ordered metadata stream.
3. Same merge reconciler as local sync.
4. Request adaptive block signatures only for changed large delta candidates.
5. Stream signatures and delta operations through bounded queues.
6. Multiplex independent file work with stream IDs over one SSH byte stream.
7. BLAKE3 source/staged result before destination commit.

Do not extend protocol v2 for compatibility.

## Protocol Standards

- Fixed bounded frame header and hard maximum payload size.
- Validate lengths before allocation.
- Typed protocol errors.
- Adversarial/property tests for truncation, oversized lengths, and invalid values.
- Wire paths must not silently lose non-UTF-8 Unix filenames.
- Block size is adaptive; do not hardcode 4096-byte signatures for every file.
- Do not build `Delta { ops: Vec<_> }` on the network path; emit operations incrementally.
- Compression flags reflect actual compressed payloads only.

## One Binary and Static Linux

Issue #26 asks for standalone/musl binaries. It aligns with the target architecture but do not respond to the issue unless the user explicitly asks.

Release goals include static Linux artifacts for at least:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

The remote peer is the same `sy` executable running `__serve`; no separate `sy-remote` artifact.

A later bootstrap mechanism may upload/cache a trusted compatible static agent when `sy` is not already installed remotely.

## Code Standards

Follow the user's `rust-expert` skill. In particular:

| Aspect | Standard |
|--------|----------|
| Errors | typed library errors; contextual application errors |
| Recoverable failures | no `unwrap()` |
| Unsafe | every block requires a `// SAFETY:` invariant comment |
| Async | project runtime (Tokio), bounded channels, blocking work isolated |
| Domain data | strong types/enums at boundaries instead of loose strings/integers |
| Comments | explain invariants/WHY, not line-by-line WHAT |
| Visibility | do not publish APIs accidentally |

For CLI work, follow `rust-cli`: migrate to the `usage-rs` v6 facade rather than adding new clap infrastructure; generate help/manpages/completions from the usage spec.

## Verification Commands

During the rewrite, use the repository's stable toolchain and keep these gates meaningful:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check
```

Run Miri over relevant unsafe/path/reflink code where practical before the 0.5 release.

Integration tests should assert externally meaningful semantics and safety invariants. Delete or rewrite tests whose only purpose is checking obsolete 0.4 strategy names, logs, or architecture.

## Legacy Code Policy

The following are scaffolding, not architecture:

- `src/bin/sy-remote.rs`
- old `src/transport/`
- protocol-v2 `src/streaming/`
- `SyncEngine<T: Transport>`
- legacy `StrategyPlanner`
- embedded `ssh2` stack/config parser
- whole-tree scan fallbacks
- duplicate planners/stats/transfer types

Do not invest in making these elegant. Replace their required behavior in the new engine, migrate callers, then delete them.

## What We Are Not Building by Default

Do not add without benchmark/evidence:

- QUIC
- io_uring
- content-defined chunking
- persistent distributed state
- rsync wire compatibility
- a custom second SSH implementation

## Current Focus

Build the new 0.5 architecture under clean `engine/`, `protocol/`, and `remote/` boundaries, then physically remove the old stacks. See `agent-context/ai/0.5-rewrite.md` for the exact current sequence.
