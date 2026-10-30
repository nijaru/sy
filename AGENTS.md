# sy — Fast File Sync

Build fast file synchronization with rsync-like ergonomics, not rsync wire compatibility.
Development on `main` tracks the unfinished 0.5 architecture; `README.md` owns delivered
usage and release status. Do not infer target contracts from historical 0.4 internals.

## Contract owners

- `docs/architecture-0.5.md` owns the target architecture, safety contracts, non-goals,
  and release gates. Read the affected sections before changing filesystem effects,
  source authority, preservation, deletion, transfer, or remote behavior. Keep those
  contracts there rather than copying simplified invariants into this file.
- Current source establishes implementation, not completion of the target design.
  Reconcile a demonstrated design problem at its owner; do not silently weaken a
  contract or preserve obsolete internals merely because they exist.
- Private knowledge retains prior intent, decisions, and research. Consult the
  relevant record when reconsidering a consequential choice, not as a startup ritual.
- Use `rust-reference` for Rust work and `rust-cli` when changing the CLI. Keep the
  existing `usage-rs` facade and generated help, manpage, and completions; do not add
  a parallel parser or hand-maintained command definition.
- Use the declared Rust edition and stable toolchain. An edition migration is a
  separate deliberate change, not part of routine architectural cleanup.

## Source navigation

- `src/engine/` owns reconciliation, planning, budgets, and shared work lifecycle.
- `src/endpoint/` and `src/rooted_fs/` own endpoint contracts and native filesystem
  authority/effects; byte-transfer strategies do not own publication policy.
- `src/transfer/` owns byte strategies; `src/protocol/` owns bounded wire contracts.
- `src/remote/` and `src/ssh/` own remote execution and external OpenSSH integration.
  The remote peer is the same `sy` binary's private `__serve` entry point.
- `src/cli/` owns command definitions; `src/bin/man-gen.rs` generates `man/sy.1`.

## Verification

`.github/workflows/ci.yml` owns the required platform, feature, security, generated
artifact, and static-musl checks. For implementation changes, use the affected
contract's regression checks and the repository gates:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo test --locked --all-features
cargo deny check
```

For command/help changes, regenerate and check the manpage with the matching
`usage-cli` version from CI:

```sh
cargo run --bin man-gen
git diff --exit-code man/sy.1
```

Linux and macOS require semantic tests; Windows compilation alone does not qualify
filesystem synchronization. Release qualification includes static-musl verification
and the safety/resource/fault gates in `docs/architecture-0.5.md#05-release-gates`.
Run Miri over relevant unsafe/path/reflink changes where practical before release.
Issue #26 concerns standalone binaries; do not respond to it unless the user asks.
