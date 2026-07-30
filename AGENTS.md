# sy — Fast File Sync

`sy /source/ /dest` — rsync's mental model, Rust performance, sane defaults.

## For AI Agents

1. Read `ai/brief.md` (current snapshot & state)
2. Read `ai/architecture.md` (what sy is, architecture)
3. Check tasks: `tk ls`
4. Reference `ai/decisions.md` for rationale

## Project

| Attribute | Value |
|-----------|-------|
| Language | Rust (edition 2021) |
| Version | v0.4.1 (main branch) |
| Tests | 630+ passing |
| License | MIT |
| Positioning | "fd for find" — spiritual successor, not wire-compatible |

## Build & Verify

```bash
cargo build                    # Build
cargo test                     # Test (572 pass, 12 ignored SSH)
cargo clippy -- -D warnings    # Lint (zero warnings)
cargo fmt --check              # Format (no changes)
cargo bench --no-run           # Verify benchmarks compile
cargo test --test sync_ssh -- --ignored --test-threads=1  # SSH integration (18 tests, sequential)
```

## Architecture

```
SyncSession
  ├── source: EndpointPair (Local | SSH)
  ├── dest: EndpointPair (Local | SSH | S3 | GCS)
  └── config: SyncConfig
        ↓
    select_strategy()
        ├── DirectLocal    (scan → plan → execute via Endpoint)
        ├── StreamingPush  (SSH connect → sy --server → streaming protocol)
        ├── StreamingPull  (reverse)
        └── ObjectStore    (S3/GCS, future)
```

SSH bypasses the Endpoint trait — streaming protocol operates over stdin/stdout directly. Endpoint is for local/S3/GCS only.

## Data Safety (Non-Negotiable)

sy moves people's files. Data loss is the worst possible bug. These rules exist because violations caused real bugs.

### Atomic Writes

Every destination write MUST use temp-file + rename. If the process crashes mid-write, the destination must be unchanged (old file intact) or complete (new file renamed). Never write directly to the final path.

```
# CORRECT (streaming receiver does this):
write to path.with_extension("sy.tmp")
fs::rename(temp_path, final_path)

# WRONG (local executor was doing this until it was caught):
fs::write(final_path, data)  // crash = partial file = data loss
```

This applies to: local file copies, streaming receiver, delta sync, any future endpoint (S3, GCS). No exceptions.

### Source is Read-Only

Never modify the source during sync. The only exception is `--remove-source-files`, which deletes source AFTER confirmed transfer. Deletion must be the last step — if anything fails before confirmation, source is untouched.

### Deletion Safety

Deletions are destructive and irreversible. Every delete path needs:
- Threshold check (--max-delete): refuse if too many files would be deleted
- Force override (--force-delete): explicit opt-in to bypass threshold
- Filter awareness: count only files the user can see (filtered), not internal state

### Test Every Error Path

Every `anyhow::bail!`, `return Err(...)`, and `?` that can fail is a potential data-corruption path. If you add an error path, add a test for it. Untested error paths are bugs waiting to happen.

## Streaming Protocol Lessons

These bugs were found in production testing. Each one caused silent data corruption or incorrect behavior.

### mtime Must Be Nanoseconds

The protocol stores mtime as `i64`. If you use `as_secs()` (integer seconds), two files modified within the same second are treated as identical. The scanner collects nanoseconds — the protocol must preserve them end-to-end:

```
scanner → generator → sender → protocol → receiver → file write
         (nanos)     (nanos)   (nanos)    (nanos)    (nanos)
```

If any hop truncates to seconds, same-second modifications are silently skipped.

### Permissions Must Propagate

The scanner provides `mode` via `PermissionsExt::mode()`. This must flow through the same pipeline as mtime. Default `0o644` is wrong — it discards executable bits, setuid, and ownership.

Every `FileEntry`, `FileEntryJson`, `CachedFile`, and protocol struct needs a `mode` field. On non-Unix platforms, use a sensible default (0o644).

### Server Doesn't Know About Client Filters

The server scans its directory with no filter context. In push mode, the server sends ALL dest entries (including `.git`, `.svn`, etc.) back to the client. The client's generator must apply the filter engine to DEST entries, not just source entries.

```rust
// CORRECT: filter both source and dest entries
pub fn add_dest_entry(&mut self, entry: DestFileEntry) {
    if let Some(ref filter) = self.config.filter {
        let path = Path::new(&entry.path);
        if filter.should_exclude(path, is_dir) {
            return;  // skip filtered dest entries
        }
    }
    // ... insert into dest_index
}
```

If you only filter source entries, the dest count is inflated by filtered directories, and deletion thresholds become meaningless.

### Filter Patterns Need Children

`--exclude .git` matches the directory itself but NOT its children. Always pair directory exclusions with glob children:

```rust
filter_engine.add_exclude(".git");
filter_engine.add_exclude(".git/**");
```

Without the `**` glob, `.git/refs`, `.git/hooks/*`, etc. pass through the filter.

## Code Standards

| Aspect | Standard |
|--------|---------|
| Commit format | `type(scope): description` |
| Comments | WHY not WHAT |
| Error handling | `thiserror` (library), `anyhow` (binary) |
| Parallelism | All cores by default, `-j 1` to escape |
| State | Stateless core, filesystem is truth |
| Writes | Atomic (temp + rename). No exceptions |

## Quirks

| Area | Knowledge |
|------|-----------|
| Hashing | xxHash3 ≠ rolling hash (Adler-32). Different purposes |
| Compression | Overhead on >4Gbps. Never compress local sync |
| Filesystems | COW + hard links conflict. Hard links force in-place (nlink > 1) |
| SSH | `sy --server` receives literal `~`. Must expand manually |
| S3 | 5MB multipart minimum. Small files use simple put |
| SSH tests | Must run `--test-threads=1` — shared remote state causes parallel failures |
| Filter engine | `should_exclude` matches exact paths; `**` glob needed for children |
| `--exclude-vcs` | Adds `.git`, `.git/**`, `.svn`, `.svn/**` etc. to filter engine |

## What We Skip

io_uring (security, complexity), CDC (backup dedup, not sync), persistent state (test before adding), QUIC (45% regression on fast networks), wire-compatible rsync (too much burden).

## Test Quality

- **Test error paths, not happy paths.** Happy paths are obvious. Error paths are where data loss happens.
- **Avoid redundant tests.** 24 variations of `should_compress` with different values don't add value. Test the boundary (0, threshold, max), not every integer.
- **Integration tests > unit tests for correctness.** Unit tests verify logic. Integration tests verify that the full pipeline preserves data end-to-end.
- **SSH tests are sequential.** Use `--test-threads=1`. Shared remote state causes flaky failures in parallel.
- **Every bail/Err needs a test.** If you can't test it, document why (e.g., hardware error).

## Current Focus

v0.4.1 tagged and released on `main` branch. See `ai/brief.md`.

---

**Updated**: 2026-07-29
