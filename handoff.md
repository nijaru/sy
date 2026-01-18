# Handoff Document: Streaming Protocol v2 Implementation

**Date:** 2026-01-18
**From:** Claude (Opus)
**To:** Gemini Flash (or any AI agent)
**Project:** sy - Modern File Synchronization Tool

---

## STRICT RULES - READ FIRST

### Git Rules (NON-NEGOTIABLE)

1. **NEVER push to remote** - All work stays local until human reviews
2. **NEVER force push** - No `git push -f` under any circumstances
3. **NEVER commit to main** - You are on `feature/streaming-protocol-v2`
4. **Commit after each phase** - Small, focused commits with clear messages
5. **Commit message format:** `feat:`, `fix:`, `test:`, `docs:`, `refactor:`

### Code Rules (NON-NEGOTIABLE)

1. **Run verification after EVERY change:**
   ```bash
   cargo check && cargo test streaming && cargo clippy -- -D warnings
   ```
2. **All tests must pass** - Do not proceed if tests fail
3. **No clippy warnings** - Fix all warnings before committing
4. **Do not modify v1 protocol** - Files in `src/server/protocol.rs` are v1, leave them working
5. **Do not break existing tests** - Run `cargo test` (full suite) before final commit

### Scope Rules (NON-NEGOTIABLE)

1. **Only implement what's in the plan** - No "improvements" or "enhancements"
2. **Do not refactor unrelated code** - Stay focused on streaming module
3. **Do not add dependencies** without explicit need (bitflags already added)
4. **Do not create new files** outside `src/streaming/` unless plan specifies
5. **If stuck, stop and document** - Write what's blocking in `ai/STATUS.md`

---

## Project Context

### What is sy?

A Rust file synchronization tool (like rsync). Currently uses request-response protocol which has latency issues on WAN. We're rewriting to streaming protocol.

### Current State

| Item      | Value                                      |
| --------- | ------------------------------------------ |
| Branch    | `feature/streaming-protocol-v2`            |
| Phase 1   | COMPLETE (protocol.rs, channel.rs, mod.rs) |
| Remaining | Phases 2-7                                 |
| Tests     | 511+ passing, 18 new streaming tests       |

### Directory Structure

```
sy/
├── src/
│   ├── streaming/          # YOUR WORK GOES HERE
│   │   ├── mod.rs          # Done - public API
│   │   ├── protocol.rs     # Done - message types
│   │   ├── channel.rs      # Done - channel types
│   │   ├── generator.rs    # Phase 2 - YOU CREATE
│   │   ├── sender.rs       # Phase 3 - YOU CREATE
│   │   ├── receiver.rs     # Phase 4 - YOU CREATE
│   │   └── pipeline.rs     # Phase 5 - YOU CREATE
│   ├── server/
│   │   ├── mod.rs          # Phase 6 - MODIFY (add v2 dispatch)
│   │   ├── protocol.rs     # v1 protocol - DO NOT BREAK
│   │   └── handler.rs      # v1 handler - DO NOT BREAK
│   ├── transport/
│   │   └── ssh.rs          # Phase 7 - MODIFY (add v2 path)
│   ├── delta/              # Existing - USE these modules
│   │   ├── generator.rs    # Delta computation
│   │   ├── applier.rs      # Delta application
│   │   └── checksum.rs     # Block checksums
│   └── sync/
│       └── scanner.rs      # File scanning - USE this
├── ai/
│   ├── design/
│   │   ├── streaming-protocol-v0.3.0.md    # Full protocol spec
│   │   └── streaming-implementation-plan.md # Step-by-step guide
│   └── STATUS.md           # Update when done
└── handoff.md              # THIS FILE
```

---

## Implementation Plan Location

**Primary guide:** `ai/design/streaming-implementation-plan.md`

This file contains:

- Complete code for each phase
- Exact structs, functions, implementations
- Test code to copy
- Verification commands

**Protocol specification:** `ai/design/streaming-protocol-v0.3.0.md`

Reference this for:

- Message formats
- Wire protocol details
- Data flow diagrams

---

## Phase Summary

| Phase | File to Create/Modify                | Goal                              |
| ----- | ------------------------------------ | --------------------------------- |
| 2     | `src/streaming/generator.rs`         | Scan files, send to Sender        |
| 3     | `src/streaming/sender.rs`            | Read files, compute deltas        |
| 4     | `src/streaming/receiver.rs`          | Write files from Data messages    |
| 5     | `src/streaming/pipeline.rs`          | Wire up Generator→Sender→Receiver |
| 6     | `src/server/mod.rs`                  | Add v2 server handler             |
| 7     | `src/transport/ssh.rs`, `src/cli.rs` | Add --protocol-v2 flag            |

---

## Workflow for Each Phase

```
1. Read the phase in ai/design/streaming-implementation-plan.md
2. Create/modify the file exactly as specified
3. Add the module to src/streaming/mod.rs if new file
4. Run: cargo check
5. Run: cargo test streaming::<module>
6. Run: cargo clippy -- -D warnings
7. If all pass, commit:
   git add src/streaming/
   git commit -m "feat(streaming): implement <phase name>"
8. Move to next phase
```

---

## Existing Code to Use

### Scanner (for Generator)

```rust
use crate::sync::scanner::{Scanner, ScanOptions, ScanEntry};

let scanner = Scanner::new(ScanOptions::default());
let entries = scanner.scan(&root_path).await?;

// ScanEntry has: path, size, mtime, mode, is_dir, is_symlink, symlink_target, inode, nlink
```

### Delta Generator (for Sender)

```rust
use crate::delta::generator::DeltaGenerator;
use crate::delta::checksum::compute_block_checksums;

// Compute checksums for a file
let checksums = compute_block_checksums(&path, block_size).await?;

// Generate delta ops
let delta_gen = DeltaGenerator::new(block_size);
let ops = delta_gen.generate(&mut reader, &checksums).await?;
```

### Delta Applier (for Receiver)

```rust
use crate::delta::applier::DeltaApplier;

let applier = DeltaApplier::new();
applier.apply(&mut file, &delta_data).await?;
```

---

## Types Already Defined (Phase 1)

### In `src/streaming/protocol.rs`:

```rust
// Message types: Hello, FileEntry, DestFileEntry, Data, DataEnd, Delete, etc.
// Flags: HelloFlags, FileFlags, DestFileFlags, DataFlags
// Functions: read_frame(), write_frame(), encode(), decode()
```

### In `src/streaming/channel.rs`:

```rust
// FileJob - file info from Generator to Sender
// DeltaInfo - checksums for delta computation
// DestFileState - destination file state
// DestIndex - hashmap of dest files
// GeneratorMessage - enum for Generator output
// SyncStats - statistics
// file_job_channel() - create bounded channel
```

---

## Error Handling

Use `anyhow::Result` for all functions:

```rust
use anyhow::Result;

async fn my_function() -> Result<()> {
    // Use ? for error propagation
    let file = File::open(&path).await?;
    Ok(())
}
```

---

## Testing Pattern

Each module should have tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_something() {
        let tmp = TempDir::new().unwrap();
        // ... test code
    }
}
```

---

## Common Pitfalls to Avoid

1. **Forgetting to update mod.rs** - Every new file needs `pub mod <name>;`

2. **Using wrong imports** - Use `crate::streaming::` not `super::`

3. **Blocking in async** - Use `tokio::fs` not `std::fs`

4. **Missing feature flags** - Some code needs `#[cfg(unix)]`

5. **String vs PathBuf** - Protocol uses `String`, filesystem uses `PathBuf`

6. **Forgetting to flush** - Always `writer.flush().await?` after writes

---

## Verification Commands

```bash
# After each phase
cargo check
cargo test streaming
cargo clippy -- -D warnings

# Before final commit
cargo test  # Full test suite (511+ tests)
cargo build --release

# Check you haven't broken anything
cargo test server  # v1 protocol still works
```

---

## If You Get Stuck

1. **Compilation error:** Read the error carefully, check imports
2. **Test failure:** Print debug output, check test expectations
3. **Unclear requirement:** Read `streaming-protocol-v0.3.0.md`
4. **Missing function:** Check `src/delta/` and `src/sync/` for existing code
5. **Still stuck:** Write the problem to `ai/STATUS.md` under "Blockers"

---

## Final Checklist

Before declaring done:

- [ ] All 7 phases implemented
- [ ] `cargo check` passes
- [ ] `cargo test` passes (all 511+ tests)
- [ ] `cargo clippy -- -D warnings` passes
- [ ] Each phase has a separate commit
- [ ] No pushes to remote (check with `git log --oneline origin/main..HEAD`)
- [ ] `ai/STATUS.md` updated with completion status

---

## DO NOT

- Push to remote
- Modify `src/server/protocol.rs` (v1)
- Modify `src/server/handler.rs` (v1)
- Add features not in the plan
- Skip tests
- Ignore clippy warnings
- Make "improvements" to unrelated code
- Use `unwrap()` - use `?` instead
- Create files outside the plan

---

## Contact

If this handoff is unclear or you need clarification, the human should:

1. Ask Claude to clarify
2. Or read the detailed plan in `ai/design/streaming-implementation-plan.md`

Good luck!
