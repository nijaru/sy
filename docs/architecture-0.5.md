# sy 0.5 architecture

`sy` 0.5 is a clean architectural cut, not an incremental migration of the 0.4 internals. Preserve useful user-facing semantics, but do not preserve internal APIs, transport layers, wire compatibility, or implementation structure solely for compatibility.

The 0.5 scope is one bounded synchronization engine for local filesystems and SSH in both directions, including confined source-link following, transactional type transitions, hardlink preservation, and verified deferred source removal. Object stores, bisync, automatic agent bootstrap, and functional Windows synchronization are outside this release. Windows compilation is not a filesystem capability claim.

This document describes the **target**, not a claim that every contract is implemented. Replacement transactions, identity validation, bounded namespace preflight, and endpoint conformance are release requirements; a passing happy-path transfer is not evidence that these contracts hold.

## Design principles

1. **One engine.** Local and remote synchronization share scanning, reconciliation, planning, scheduling, integrity, and safety semantics.
2. **One binary.** `sy` acts as both the user-facing client and the private remote agent. There is no `sy-remote` in 0.5.
3. **Bounded memory.** File size and tree size must not imply proportional resident memory on the common path.
4. **Transactional visibility.** Destination mutations are staged and verified before commit whenever the backend can support atomic replacement.
5. **Capabilities, not endpoint branches.** Transfer policy is chosen from explicit backend capabilities and workload characteristics.
6. **Cheap metadata first.** Expensive metadata, hashes, signatures, sparse extents, ACLs, and xattrs are demand-driven.
7. **Strong end-to-end integrity.** Remote transfers are BLAKE3-verified before commit by default. Size alone is never proof of correctness.
8. **Deletion is a commit phase.** An incomplete source view can never authorize deletion.
9. **The source is read-only.** `--remove-source-files` is the only exception and runs after terminal destination completion, strong content/requested-preservation proof, and original source-binding validation.
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
  main.rs
  lib.rs
```

Delete superseded 0.4 engines, `streaming/`, `transport/`, and `sy-remote` once their required behavior is migrated. Thin direction/CLI adapters may remain; a directory name is not itself evidence of a second engine. `object.rs` and `bootstrap.rs` above describe later extensions, not 0.5 deliverables.

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

Reconciliation and semantic planning decide **what change is required**. Transfer planning decides **how to produce the bytes**. The scheduler decides **when work is allowed to run**. Endpoints own **whether and how a prepared change commits**; byte strategies never publish a replacement themselves.

One engine controller owns completed preflight, preorder directory preparation, bounded concurrent leaf work, draining workers, reverse deletion, and journal-owned directory finalization. Direction-specific adapters must not duplicate preservation, commit, or failure policy. The current direction-neutral controller belongs under `engine` once its dependencies are neutral; its location is not a reason to introduce a second controller.

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

Capability assertions are contracts and require conformance tests. A backend must not advertise a capability that its implementation silently ignores. Root/filesystem properties must not be inferred solely from an endpoint's OS or transport type.

The endpoint contract has one semantic capability vocabulary. Wire capability bits map into it at negotiation; protocol and direction adapters must not independently decide preservation or transaction semantics. Local and SSH implementations use the same transaction lifecycle, while retaining backend-native byte strategies. Extend the existing endpoint/staged-writer boundary rather than adding a parallel transaction framework.

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

## Original SOURCE authority

Extend the existing run-owned source root; do not create separate authority systems for scanning, following, exclusion, and removal. One owner retains the original root policy, an authenticated disk-backed observation catalogue, and bounded active native readers. Derived plan, discard, and removal journals reference those original observations rather than establishing competing identities.

Keep three kinds of evidence distinct:

- **Object identity** identifies the observed inode/content object. A genuine opened file owns byte reads and demand-driven preservation.
- **Binding evidence** records how that object was selected: original directory ancestors, parent/name, and any followed link identities and literal targets. Equal inode/content under a foreign parent is not the original binding.
- **Completion evidence** records the destination outcome, byte/preservation proof, and outstanding group/subtree/parent barriers. Neither of the first two proves publication or removal eligibility.

### Capture and validation

Capture each considered source name through the original root before filter, ignore, `.git`, depth, comparison, or planning omission. Record the original named kind/identity and opening parent; share directory ancestor records instead of copying a chain into every entry. An excluded or traversal-pruned directory protects its physical subtree without enumerating its descendants. Logical reconciliation paths and physical source bindings remain separate.

Following adds a resolution transcript and effective data observation; it never replaces the original selected binding. Every selected hardlink member retains its own binding even when one opened representative supplies bytes. Link retargeting invalidates the observation even when both targets are hardlinks to the same inode.

Before omission, required data/metadata reads, completion, and eventual unlink, validate the applicable original records. Reopening is permitted to validate recorded provenance, not to adopt a fresh parent, link, or leaf as the original. Byte readers validate the original opened object before and after work; requested preservation belongs to that same observation. Fail with a typed source-change error; retries, if offered, start a new bounded observation attempt rather than repairing old authority.

Catalogue records and derived references have bounded lengths, exact membership/exhaustion, and authentication anchored in trusted run-owned state. Scratch corruption, replacement, truncation, and worker failure are terminal; recomputing a checksum over a changed scratch file does not restore authority. Bound traversal stacks, sort/index buffers, descriptors, and readers. Disk use may scale with names; resident memory and held descriptors must not. Do not retain one FD per source entry or build a whole-tree inode map.

### SOURCE exclusion from destination effects

Completed preflight admits the full destination effect set against original SOURCE observations, including omitted entries. This covers replacement/discard roots and descendants, ordinary deletes, backup publication/retirement, directory metadata, and inode-changing metadata—not just transferred matched files. Names protected by a pruned source directory cannot become discard work. Namespace/binding effects and shared-inode effects require their respective proofs; equal bytes are irrelevant to either.

Use native evidence only within its qualified identity domain. A device/inode pair local to one host is not a cross-host disjointness certificate; a qualified filesystem UUID/file ID may establish object equivalence without establishing parent/name independence. Mount aliases and APFS logical/physical parent differences must not be hidden behind a path-prefix check. Where a potentially overlapping effect cannot be qualified safely, refuse that effect before mutation. Unknown evidence is neither permission nor a blanket refusal of unrelated SSH work already proved safe. Do not introduce a universal filesystem-provider or path-projection framework.

### Root lifetime

Local SOURCE and local PULL destination operations require original operator-address continuity. Remote roots are negotiated held directory authorities: relative operations may survive root relocation, but never adopt a replacement found at the configured spelling. A remote root's former absolute spelling does not automatically move with its FD. Following the operator-selected remote directory-link root is an explicit follow-once classification step; after capture, later operand-link changes cannot redirect the session.

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

Type transitions must be transactional rather than remove-then-create. Until a backend implements the required guarantee, reject unsupported transitions during complete preflight, before any destination mutation.

For a nondirectory-to-directory transition, stage the replacement subtree completely, including child work and final metadata, before switching visibility. Child completion inside that private subtree is not destination publication: no child receipt authorizes source removal until the enclosing transaction commits and required finalization succeeds.

The enclosing subtree owns publication through its original held staging-directory authority. After authenticated private preparation, individual source and creator file descriptors may close; do not retain one descriptor per descendant or hardlink group until parent publication. Record exact prepared membership and integrity/preservation evidence in bounded disk-backed manifests, then revalidate the original source observations and prepared namespace before atomic publication. This changes file-descriptor lifetime only for subtree-owned preparation, not ordinary standalone file/group transactions. A visible alias or a cached inode number is never a substitute for owned private staging.

Cross-subtree hardlink groups use one byte population, one inode-level metadata policy, bounded protected parking, and authenticated disk-backed membership/progress. The first selected member supplies ordinary metadata; explicit preservation overrides it. Creation, byte update, replacement, or topology repair uses SOURCE bytes, including quick-equal members; metadata-only/equal groups retain OLD bytes and isolate outside aliases when a write is needed. Every member validates its original source/destination authority, including effect-free members. No source removal is eligible until every member, enclosing subtree, and containing-parent finalization succeeds. Publishing one subtree does not imply whole-group rollback if another fails.

For directory-to-nondirectory replacement, protect excluded descendants and include all discarded descendants in completed preflight's exact deletion accounting. The replacement entry itself is counted as a replacement, not a duplicate delete candidate. Discarding nonempty directory contents requires explicit deletion authorization (`--delete`); an empty entry-kind replacement does not. Apply the threshold before any destination mutation, and assign discarded descendants to the transition transaction so ordinary delete replay cannot remove/count them a second time. Recursive removal is not a shortcut around deletion safety.

A discard catalogue is evidence, not authorization. The controller admits it only after both scans complete, protections and exact thresholds pass, and original SOURCE exclusion succeeds. Bind the exact OLD root and descendant inventory to the original planned transition. Revalidate before exchange, then retire only that authenticated inventory in reverse order through the held OLD authority. Enumeration may detect foreign children; it must never discover additional authorized deletes. Unknown children or displaced OLD bindings retain artifacts and produce an explicit cleanup failure.

The transition states are:

```text
DIR -> leaf: observed OLD / sealed discard inventory
             -> complete preflight admission
             -> prepared and verified leaf
             -> late SOURCE / OLD / inventory validation
             -> native exchange
             -> exact OLD retirement / required finalization
             -> completion

leaf -> DIR: observed OLD / owned private directory
             -> bounded child and hardlink preparation
             -> sealed membership / content / preservation evidence
             -> private reverse finalization and late validation
             -> native exchange
             -> required finalization / completion
```

A post-exchange retirement/finalization error is a committed failure, not rollback. Child preparation or native inventory checks alone never enable a public transition.

Prefer a backend atomic exchange/replacement primitive where supported. A fallback that renames the old destination to a tombstone and then installs staging has a missing-name window: it is **recoverable multi-step replacement**, not atomic visibility. Enabling that fallback requires an owned on-disk recovery record, identity-checked rollback, process-crash recovery, and safe tombstone cleanup. Recovery must preserve the last valid copy. Automatic destructive recovery requires ownership protected by backend primitives or an explicit exclusive-access assumption; if ownership cannot be established safely, retain recovery artifacts and stop for intervention rather than risk deleting an unrelated replacement. Identity checks alone do not establish atomic ownership under concurrent namespace mutation. Refuse transitions where the supported guarantee cannot be met.

State separately whether an operation provides namespace atomicity, recoverability, or power-loss durability. Any durability claim requires appropriate persistence ordering and tests; successful rename alone is not such a guarantee. Synchronization does not promise whole-run rollback.

### Source mutation / TOCTOU

A completed scan is not a filesystem snapshot. Native copy, reflink, sparse, hashing and streaming all consume the original SOURCE observation contract above, including applicable binding transcripts. A path-based optimization that cannot preserve that authority must fall back to a safe bounded strategy. Checks are observational race detection, not snapshot isolation or immunity to arbitrary concurrent in-place writers. The absence of a portable atomic lookup/check/mutation syscall is not by itself a reason to reject an otherwise supported observational contract.

Destination operations carry the expected state (absent or observed identity), revalidated before destructive commit/delete. Portable stat followed by rename/unlink is not atomic compare-and-swap. Use stronger backend preconditions where available and document residual concurrency limits rather than claiming separate syscalls close every race.

### Namespace preflight

Detect destination case/normalization aliases across planned source paths, existing destination paths, and ancestor prefixes before mutation. Each child name is governed by its containing destination directory, not a uniform root profile: descendant casefold flags and submounts can change the rules. Qualify existing parents through their original held-root ancestry; predict missing parents only from qualified native creation inheritance or a documented conservative model. Validate acquired children before admitting descendants. Where exact semantics are unavailable, reject ambiguous operations; generic Unicode normalization is not proof of filesystem equivalence.

Retain authenticated, disk-backed original parent qualifications through native effects. Revalidate the actual held parent before mutation admission, including directory metadata/finalization targets. Only the run's confirmed directory creations may advance predicted absence into observed directory authority. Starting another run creates another owner; old prepared work never adopts the new run's proof. These checks are observational, not immutable namespace snapshots.

Include actual and potentially publishing hardlink-member backup destinations in the same preflight. In-root backups share the destination collision set; an operator-selected external backup root has its own original held authority and bounded collision set. Retain early external acquisition failures without retrying the path, but require that authority only when the completed plan can produce a backup. Exact deletion replay must remain unchanged after backup declaration. No permanent backup parents may be created before all collision proofs and native seals succeed.

Collision detection must remain bounded even for one extremely wide directory. Use disk-backed records and bounded external sorting/merge (or an equivalently bounded exact algorithm), not a whole-tree or unbounded per-directory map. Bound record sizes, run buffers, merge fan-in and descriptors; propagate scratch I/O failures before mutation. Identical path spellings may deduplicate; distinct names must never silently overwrite one another.

## Exact bounded deletion

Do not use a full source `HashSet`, and do not rely on Bloom-filter false positives in the final design.

During a complete no-mutation preflight merge, append exact destination-only deletion candidates to an on-disk journal. The journal is bounded in RAM and stores records that can be replayed in reverse without keeping an offset index, for example:

```text
[record payload][record_len:u32]
```

After both scans complete successfully:

1. calculate the deletion threshold against the actual eligible destination scope;
2. reject before mutations if the threshold is exceeded;
3. perform non-delete work and drain admitted workers;
4. replay destination deletes in reverse/depth-safe order;
5. complete required containing-directory finalization;
6. seal terminal destination evidence, then run eligible deferred SOURCE removal;
7. close/join journal work and release owned scratch resources.

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

For replacement entries, the destination endpoint owns this lifecycle:

```text
validate source observation and expected destination
  -> prepare private staging
  -> transfer / reconstruct / patch
  -> apply staging-compatible preservation
  -> verify staged contents and source identity
  -> revalidate expected destination
  -> commit using the declared backend guarantee
  -> required post-commit finalization and ownership-sensitive cleanup
  -> qualified completion receipt
```

Failure before commit leaves the previous destination intact under the backend's declared guarantee. Staging must be on the required filesystem and remain private even while applying permissive modes/ACLs; use a protected staging namespace or an equivalent proven mechanism.

Xattrs, ACLs and other staging-compatible preservation belong before commit, not in a second visible-path RPC. Remote preservation addresses a session-bound private transaction/stream identity with bounded payloads and validated phase transitions. Assemble metadata as xattrs -> mode/time -> ACL -> explicit-mode proof -> flags. Preserve numeric identities and raw ACL order. Retire private inode anchors before flags; defer flags when they conflict with remaining namespace operations.

Fulfilled requested state avoids setters. The basic effect-free lane requires xattrs, ACLs and flags all unrequested; an explicitly empty extended request is still a request. Named FILE/parent observation may suffice for this lane but grants no byte, setter, publication, or source-removal authority. Mandatory SOURCE, OLD/default, backup, namespace, or verification reads remain errors when inaccessible: no temporary chmod, fake FD, path setter, or blanket permission-error waiver. Skip OLD metadata reads only for fields precisely superseded by captured SOURCE requests.

Only metadata inherently incompatible with commit belongs afterward, such as selected immutable flags. Distinguish pre-commit abort, committed-but-required-finalization-failed, cleanup-pending, and uncertain completion. A post-commit failure cannot be reported as rollback. Receipts bind original source/destination observations and actual required verification/preservation outcomes; an inode number, item count, NoOp bookkeeping proof, or private-subtree preparation result is not a qualified completion receipt. Destination deletion waits for successful required main work; source removal has the stronger terminal gate below.

Directory metadata finalization remains journal-owned after descendant work and deletion. Metadata-only changes on existing inodes have explicit backend semantics, not a fictitious atomic replacement. Hardlink group metadata has one inode-level owner; group bookkeeping must also be bounded.

The endpoint serializes mutation admission against disconnect/cancellation after late validation and before the native effect. This includes file publication, directory/link work, backups, in-place metadata, destination deletes, and deferred SOURCE unlink—not just transferred files. Closing admission is nonblocking; do not make terminal actors wait behind filesystem I/O. Work not yet admitted aborts owned staging; admitted native work may finish and cannot be promised interruption or rollback.

The controller/session retains worker and journal ownership until ordinary shutdown has called `close()` and joined all admitted work. Dropping or aborting a future cannot perform an async join. Report operation and drain/cleanup errors together when both occur. Uncertain destination completion or failed required finalization/cleanup prevents SOURCE removal from starting. During removal, cancellation stops new admission but an admitted unlink may finish: retain candidates not yet admitted, join admitted work, and report confirmed/uncertain partial removal without promising rollback. Strategies never admit or commit effects independently of their endpoint owner.

### Deferred SOURCE removal

`--remove-source-files` adds one final phase after required work, worker drain, destination deletion, and every containing-parent/group/subtree finalization. Ordinary local copy need not hash twice, but removal eligibility always requires strong byte parity and successful requested preservation, not quick equality. Qualify either a published destination or a strongly verified existing destination; a same-binding NoOp, unchanged symlink, lost receipt, or uncertain required completion retains SOURCE.

Removal addresses the original selected nondirectory binding. With link following, remove the selected file-link itself only after its original link transcript and effective bytes qualify; do not implicitly unlink the followed referent. Retain directory links. Multiple logical selections reaching one native unlink binding are coalesced, and every associated destination completion must succeed before that one unlink. Different hardlink names remain distinct bindings.

Use a sealed authenticated removal journal referencing the original SOURCE catalogue and exact destination completions. Before the first unlink, validate all eligible destination evidence and all original source bindings. Before each candidate, repeat the late destination check/assertion and source-binding validation, then acquire mutation admission and unlink. No gate is a snapshot: report confirmed removals and uncertain native effects accurately if a later candidate fails. Do not promise rollback of earlier unlinks.

Only confirmed own SOURCE unlinks may advance narrowly defined native continuation state for remaining source aliases/parents. Authenticate that progress and preserve all unaffected identity/binding fields. SOURCE never adopts destination retirement lineage, fresh observations, or SOURCE markers. Cancellation and replay errors retain ownership until admitted work is joined.

For PULL, the server owns original scan observations and its final removal journal. The client sends session-bound terminal destination completion assertions, not paths or freshly supplied identities that establish SOURCE authority. Assertions bind exact source observations, destination membership, strong digest/requested preservation, outcome, and all enclosing barriers. They attest client-side completion; the server cannot independently inspect or certify the client's filesystem. Missing, duplicate, incomplete, wrong-session, or uncertain assertions fail before removal.

A complete sealed assertion set and explicit removal-phase request are required before any server unlink. The client must finish local work before requesting that phase and retain its destination owner through assertion revalidation and joined completion. For each candidate, the server requests a fresh, phase/candidate-bound assertion after the client revalidates that candidate's original destination proof; validating the stored assertion bytes again is not the late destination check. The server then revalidates original SOURCE bindings and exact dependencies before unlink admission. These separate checks remain observational, not cross-host atomicity. A lost final response may follow confirmed server unlinks; report uncertainty and re-observe on a later run, not automatic replay of an old session. Existing 3.12 fetch/group completion ACKs remain read-only and never acquire removal meaning.

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

The clone primitive must preserve regular staging ownership. Prefer an FD-to-FD clone into the genuine created stage. A syscall that creates a name but returns no creator FD does not automatically qualify a later pathname open as creator authority; a private directory's mode alone is not exclusivity against same-UID substitution. Without a separately qualified acquisition and failure-cleanup contract, do not advertise that native clone strategy—use whole copy. Unknown named clone results are not wildcard cleanup authority, and OLD clone metadata must not leak into SOURCE replacement semantics. Linux FD cloning is implemented; macOS currently uses native whole copy. Native COW support alone does not establish a usable backend capability.

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

Pass the original host alias to OpenSSH and only explicit user overrides. Do not parse and reconstruct SSH configuration: `Include`, `Match`, wildcard precedence, identities, and jump-host behavior belong to OpenSSH. Remove unused embedded SSH dependencies as well as their callers. The session owns subprocess shutdown, exit status and error propagation.

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
typed source or destination operand
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

The implemented exact wire contract is **3.13**, with no earlier-minor compatibility,
including no 3.12 compatibility. This minor adds read-only delta PULL; the
original SOURCE-reference, remote-follow, and terminal PULL-removal additions
remain integration requirements, not implemented claims. Integrate those changes
into the same new exact 3.13 contract, not an incremental minor chain. Negotiate
explicit source-follow/removal intent and capabilities; refuse unsupported
requested semantics before scans or mutation. Do not reinterpret 3.12 messages
or add compatibility shims.
Version refusal precedes operand classification or filesystem root opening.
`SessionOpen` is `operation:u8, operand_request, path_len:u32, native_path_bytes`:

- Operation: 1=Push, 2=Pull, 3=PreviewPush.
- Operand request: 0=Source followed by `contents:u8` (0/1); 1=Destination
  followed by source shape. Roles must agree with the operation.
- Source shape: 0=Directory contents/no basename, 1=Directory plus basename,
  2=Leaf plus physical source name. Names are `byte_len:u32, native_bytes`.

`SessionReady` is `capabilities:u64, modtime_precision_ns:u64,
resolved_operand, source_shape_presence:u8, [source_shape], pending:u8`.
It supplies no uniform destination namespace profile. Resolved operand is 0=Tree or 1=Entry plus name.
Presence and pending are 0/1. Pull returns source shape and cannot be pending;
push returns no source shape. Every path/name length is bounded by
`MAX_WIRE_PATH_BYTES` before allocation, and all tags are validated.

Classification retains the selected directory FD, or the parent FD plus exact
physical leaf name. Source names are never virtually renamed. Directory
basename/trailing-slash semantics bind actual tree roots; selected leaves use
one-name scans and the shared selected-leaf reconciler. An operator-selected
destination directory symlink may be followed initially, then remains FD-bound.

Missing destination parents are pending, not created by handshake or scanning.
Pending parents require qualified or conservative creation predictions. After complete preflight selects a
Create, `AcquireRoot` (frame kind 33) carries its destination `RelativeWirePath`,
not a new root. An Entry-bound session requires that exact selected name. The
server drains active handlers before acquisition and rebuilds every handler from
the captured root FD. Its Ack carries the updated `SessionReady`. Acquisition retains the sealed
original parent catalogue and checks actual acquired-directory rules before work. Acquisition creates permanent parents from
the original held ancestor, refuses foreign existing names, and does not claim
private creator ownership, cleanup authority, or whole-run rollback. Local
acquisition also requires original operator-path continuity; remote authority
permits relocation of the held ancestor/root. Neither adopts a replacement after
capturing its original root FD.

`NamespaceRequest` (43, `FINAL` only) is a read-only Push/PreviewPush RPC.
Its action is `0=Begin`, `1=Qualify` plus `parent_tag:u8`, `2=Seal`,
`3=QualifyBackup` plus the same parent encoding, or `4=ValidateBackupAddress`
plus `path_len:u32, relative_wire_path`. A parent is `0=Root` or
`1=Directory` plus `path_len:u32, relative_wire_path`; lengths are bounded before
allocation. Entry-bound ordinary queries permit only Root. Explicit backup
parent queries may observe root-confined destination directories, matching the
existing COPY backup destination scope; they grant no sibling SOURCE data,
metadata-setter or removal authority. Backup address validation checks literal
native spelling beneath the original held root, including containing-directory
components. Native lookup reaching an entry with no exact directory-entry
spelling is an alias, not permission to overwrite an unselected kept name. This
read-only proof runs before sealing and is repeated at native backup admission
and publication; an explicitly spelled existing backup slot retains its normal
overwrite policy.
Begin releases its opener's frame permits and drains previous handlers before
binding a new run owner. Qualify and Seal use that owner even while the root is
pending; acquisition does not replace it. Seal follows completed collision proof
and successful scan drain, not merely exhaustion of a declaration stream.

`NamespaceResult` (44, `FINAL|ACK_REQUIRED`) is `0=Begun`, `1=Qualified` plus
`namespace_semantics:u8`, `2=Sealed`, or `3=BackupAddressValidated`, then an empty
unflagged Ack on the same stream. The profile encodes case and normalization axes separately; wrong-phase,
truncated, trailing, reserved-axis and noncanonical values fail. Returned profile
bytes are collision-preflight inputs, never substitute native original authority
or SOURCE byte/removal evidence. PULL qualifies its local destination through
the same native owner without a namespace RPC to its remote SOURCE.

`SourceIgnoreRequest` (frame kind 39, `FINAL` only) is a read-only Pull RPC
for local destination deletion protection. Its payload is
`respect_gitignore:u8, is_directory:u8, path_len:u32, relative_wire_path`.
Both booleans are strictly 0/1. The bounded path uses the existing canonical
component codec and encodes the source's physical name, not a renamed local
destination name; selected-leaf scope checks require that exact source name.
`SourceIgnoreResult` (frame kind 40, `FINAL|ACK_REQUIRED` only) contains exactly
one `protected:u8` (0/1), followed by an empty unflagged `Ack` on the same stream.
Truncated, trailing, noncanonical, and oversized path payloads are rejected.

The Pull server matches through the remote source's held root and source-side
ignore policy. Query-derived directory ancestry must be resolved no-follow
through held directory handles. Trusted source-policy configuration is a narrow
read-only exception to data-root confinement: retain parent/global rules and
explicit configuration pointers, including configuration-file symlinks. This
exception never permits following a peer-selected directory ancestor or widens
file-data, metadata, or mutation authority. Local destination deletion scope
applies the remote source's ignore matching during completed no-mutation
preflight; any RPC failure prevents all mutation. The query grants no source
mutation/removal, directory-discard, or copy-links authority. It is mandatory
in the exact 3.13 contract (introduced in 3.12) and requires no additional capability bit.

### Original SOURCE references

In the integrated exact contract, server-emitted scan entries carry cheap session-bound references into the server-owned observation catalogue. Validate requested paths/members against those records; peer-supplied stat identities or a second scan cannot replace original provenance. Bind fetch, signature/delta, metadata, hardlink, and removal requests to their original observations. Source group/footer completion validates exact membership and closes readers; it remains read-only until the separately admitted terminal removal phase. Bound declaration, assertion, and replay records before allocation and authenticate disk spills.

### Metadata phase

Exchange cheap ordered metadata first. No block signatures are included in the initial tree walk.

The same merge reconciler consumes the remote metadata stream while it arrives.

`ScanRequest` carries scan_flags:u8, metadata_flags:u8, max_depth:u32, scope:u8 (0=Tree, 1=Entry). Entry adds byte_len:u32 and bounded native path bytes in the sender's platform encoding. The server validates exactly one relative name before admitting a producer, even for an absent preview root; it inspects that name through the held root FD without sibling enumeration or directory descent. Tree remains a distinct intentional query.

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

Use the same safety contract in both SSH directions. Delta PULL uses
`DeltaFetchRequest` (frame kind **41**, empty flags), requiring negotiated
`DELTA_PULL` (capability bit **15**) and BLAKE3. Its payload is
`block_size:u32, basis_size:u64, basis_identity:[u8;32], FileFetchRequest`;
the nested source request retains its existing compression, preservation, and
relative-path codec. The basis identity is an opaque client OLD observation,
not a remote path or filesystem authority. No new operation-frame kinds are
allocated.

The client owns a genuine original OLD read FD, opening-parent evidence, and
private native staging. The server sends `DeltaFetchReady` (frame kind **42**,
empty flags and empty payload) only after byte/CPU admission and original SOURCE
capture. The client must await this exact readiness before generating or uploading
signatures; an Ack is never readiness. Queued request openers stay cheap and
bounded, with no unadmitted signature frames consuming the router-global inbound
window needed by admitted streams' footers and ACKs. Readiness is server-to-client
only, occurs once, and is invalid in the signature/data/completion phases.
The client uploads unflagged `Signature` (kind 8) frames followed
by an unflagged `SignatureEnd` (kind 9) on the request stream. Block sizes use the
existing adaptive power-of-two policy, 4 KiB..1 MiB. Announcements are capped at
65,536 blocks before ingestion; block indices, individual lengths, exact total
size/count, and echoed OLD identity must agree. Truncated, duplicate, oversized,
noncanonical, and wrong-phase uploads fail rather than restarting as whole copy.

The SOURCE-only server validates its original scan catalogue when capturing
`SourceFile` before upload, validates it after upload and around incremental
matching, and retains it through client completion. Source bytes come only from
its genuine read-only FD. Wire SOURCE references remain an integration requirement
on this same owner; uploaded signatures never establish SOURCE provenance or
grant remote mutation authority. Fetch admission separately budgets resident
signature-index bytes and hashing/compression CPU, not just active stream count.
The response is requested `FileXattrs`/`FileAcls`, bounded `Data`/`DeltaCopy`, then
`FileEnd` with `FINAL|ACK_REQUIRED` and the normal source BLAKE3 digest. Any peer
frame during generation, including an early Ack or late signature, fails before
the footer. An empty unflagged Ack follows successful local commit and remains
read-only, with no removal meaning.

The client reconstructs COPY ranges only from the FD used to generate signatures,
checks OLD bounds and source output size, verifies reconstructed and staged bytes,
and validates the original OLD object, opening parent, and local operator-root
continuity before native publication. The genuine local destination owner also
checks configured-root continuity immediately after native publication and around
required finalization. Losing that address after publication is a committed
failure: no proof, ACK, or removal authority. These local checks do not make remote
held-root relocation a failure. Native OLD metadata defaults are copied
from that FD; explicit SOURCE preservation skips only precisely superseded OLD
fields. Preservation frames cannot arrive after data or satisfy an unrequested
field. Literal compression uses the existing None/Auto/Always policy and sets
COMPRESSED only for smaller actual zstd payloads; COPY and signatures stay raw.

Ordinary changed regular files with observed OLD size at least 10 MiB select delta
when the peer supports it and the adaptive index fits the cap. New files, small
or nonregular bases, unsupported peers, and bases exceeding the cap select whole
copy before transfer. Sealed hardlink groups retain their existing whole-file
SOURCE-lease lifecycle. A failed admitted delta never falls back to whole copy.

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

Compression is negotiated; do not compress copy operations or signatures. `Auto` samples the first file-data chunk and uses the elapsed-time model to decide whether to compress this transfer. `Always` attempts every chunk, including after an incompressible first chunk. Both send raw bytes unless the actual compressed payload is smaller; only smaller zstd payloads carry the `COMPRESSED` frame flag.

Pull `FileFetchRequest` carries size:u64, scanned identity:[u8;32], preservation_flags:u8, compression:u8, then the relative wire path. Compression values are 0=None, 1=Auto, 2=Always; other values are invalid. Preservation retains xattrs/ACLs bits 1/2; the former boolean compression bit 0 is invalid. Compression intent never substitutes for the actual per-frame representation flag.

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

No peer-selected DATA, METADATA, MUTATION, or policy-query ancestry may escape the held root. Trusted source-policy configuration remains the separate narrow read-only exception described in the handshake contract.

### Confined SOURCE link following

Follow only with explicit source-follow intent. An operator-selected directory symlink may be followed once during root classification; bind the actual directory FD before descendant work. Descendant resolution is SOURCE-only: destination symlinks never become traversal authority. Preserve original link identity/literal target and ancestor bindings in the SOURCE catalogue, validate around omission/read/completion, and enforce bounded path bytes, active depth, link hops, and cycle detection. Failure or unsupported qualification must propagate; do not silently skip a link.

Relative targets resolve through the held-root component stack; `..` cannot pop above its root. Absolute targets keep native/literal meaning, not virtual-root rebasing. Reject escape-and-reentry paths even if lexical normalization would end inside the root. `RESOLVE_IN_ROOT` is not an implementation of these absolute semantics.

Absolute following has one explicit read-only root-address qualification exception: inspect the operator-selected root address, or a native FD-derived candidate spelling, solely to establish that its bounded directory prefix still names the held root. A returned path, lexical prefix, or cached root identity alone is not proof. Native alias/binding qualification must establish that the literal target's prefix denotes this authority; ambiguous aliases or unqualified roots fail. Revalidate qualification on every absolute resolution step, and resolve the remaining suffix only through the held-root resolver.

This exception permits no arbitrary outside-target stat, data/metadata read, or mutation. Do not open an outside referent and then accept it because its inode matches. A root may relocate while relative operations remain valid, but an absolute link using an obsolete root spelling fails unless that spelling still natively qualifies. Changing the configured path never redirects a held session. Supported qualification is backend evidence, not a universal path-projection API.

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
- interrupted transfers and injected preservation/commit failures prove old destinations survive pre-commit aborts, with explicit post-commit failure outcomes;
- one endpoint conformance suite exercises local and SSH transaction/capability semantics;
- source/destination race tests cover native whole copy, reflink, sparse and streaming paths, including foreign-parent substitution with the same inode and link retargeting between hardlinked targets;
- original SOURCE exclusion covers omitted/pruned entries and all admitted destination effects, including backups and directory metadata; unqualified overlapping physical aliases refuse before mutation;
- namespace preflight proves bounded RAM/descriptors on wide/deep trees and refuses collisions before mutation;
- supported directory transitions have failure/process-crash recovery coverage, exact discard ownership, and no cleanup of newly arrived foreign children;
- private subtree/cross-subtree hardlink tests prove bounded descriptors, exact membership, one byte population, and all publication/finalization barriers;
- local/PUSH/PULL removal tests prove complete gates, late checks, duplicate-binding dependency handling, own-unlink continuation, and safe lost-receipt/cancellation outcomes;
- confined following tests cover operator directory links, relative/absolute targets, native root relocation, escape/reentry, cycles, non-UTF-8 names, and qualification failure;
- scratch authentication and protocol tests reject tampered references, replay/phase errors, invalid lengths, and incomplete terminal assertion sets before effects;
- Linux/macOS tests exercise supported optional features semantically, not only through compilation;
- remote files are BLAKE3-verified before commit;
- deletion tests prove incomplete scans cannot delete data;
- Miri is run over relevant unsafe/path/reflink components where practical;
- realistic benchmarks cover many-small-files, huge files, unchanged trees, low/high change ratios, local COW/non-COW, and representative SSH links in both directions, including whole/delta policy and compression; pin artifacts/environment/cache ordering, verify correctness outside timing, and separate profiling from timed comparisons;
- release CI builds and verifies static musl Linux binaries;
- all replaced 0.4 architecture is actually removed rather than left as a second path.
