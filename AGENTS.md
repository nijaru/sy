# sy — Fast File Sync

`sy /source/ /dest` — rsync's mental model, Rust performance, sane defaults.

## For AI Agents

1. Read `ai/STATUS.md` (current state)
2. Read `ai/DESIGN.md` (what sy is, architecture)
3. Check tasks: `tk ls`
4. Reference `ai/DECISIONS.md` for rationale

## Project

| Attribute | Value |
|-----------|-------|
| Language | Rust (edition 2021) |
| Version | v0.3.0 |
| Tests | 510 passing, 12 ignored (SSH agent) |
| License | MIT |
| Positioning | "fd for find" — spiritual successor, not wire-compatible |

## Build & Verify

```bash
cargo build                    # Build
cargo test                     # Test (510 pass, 12 ignored SSH)
cargo clippy -- -D warnings    # Lint (zero warnings)
cargo fmt --check              # Format (no changes)
cargo bench --no-run           # Verify benchmarks compile
```

## Architecture (v0.4 target)

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

## Code Standards

| Aspect | Standard |
|--------|---------|
| Commit format | `type(scope): description` |
| Comments | WHY not WHAT |
| Error handling | `thiserror` (library), `anyhow` (binary) |
| Parallelism | All cores by default, `-j 1` to escape |
| State | Stateless core, filesystem is truth |

## Quirks

| Area | Knowledge |
|------|-----------|
| Hashing | xxHash3 ≠ rolling hash (Adler-32). Different purposes |
| Compression | Overhead on >4Gbps. Never compress local sync |
| Filesystems | COW + hard links conflict. Hard links force in-place (nlink > 1) |
| SSH | `sy --server` receives literal `~`. Must expand manually |
| S3 | 5MB multipart minimum. Small files use simple put |

## What We Skip

io_uring (security, complexity), CDC (backup dedup, not sync), persistent state (test before adding), QUIC (45% regression on fast networks), wire-compatible rsync (too much burden).

## Current Focus

v0.4 rewrite: SyncSession replaces SyncEngine god object. See `ai/STATUS.md`.

---

**Updated**: 2026-06-11
