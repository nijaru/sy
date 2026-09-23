# sy

[![CI](https://github.com/nijaru/sy/actions/workflows/ci.yml/badge.svg)](https://github.com/nijaru/sy/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Fast file synchronization. Same mental model as rsync, built in Rust.

This README documents the 0.5 rewrite (`v0.5-architecture` branch). The
published `cargo install sy` crate is still 0.4.x; build from this branch to
track 0.5.

## Install

```bash
cargo install sy
```

## Quick Start

```bash
# Basic sync
sy /source /destination

# Preview changes
sy /source /destination --dry-run

# Mirror (delete extra files)
sy /source /destination --delete

# Remote sync via SSH
sy /local user@host:/remote
sy user@host:/remote /local
```

## Features

- **Parallel** — bounded concurrent transfers, `-j N` to tune
- **Delta sync** — only transfers changed blocks for large files
- **COW support** — reflink copies on APFS/Btrfs/XFS
- **rsync-compatible flags** — `--delete`, `--exclude`, `--compress`, `-i`, etc.
- **SSH sync** — custom v3 protocol over SSH stdin/stdout (`sy __serve`)
- **Integrity** — BLAKE3-verified transfers; race-checked, staged atomic commits

## Usage

```
sy [OPTIONS] <SOURCE> <DESTINATION>
```

### Common Flags

| Flag | Description |
|------|-------------|
| `-n, --dry-run` | Preview changes without applying |
| `-d, --delete` | Delete files not in source |
| `-v, --verbose` | Increase verbosity (repeatable) |
| `-q, --quiet` | Suppress output |
| `--stats` | Show transfer statistics |
| `--exclude <PATTERN>` | Exclude files matching pattern |
| `--exclude-from <FILE>` | Read exclude patterns from file |
| `--include <PATTERN>` | Include files matching pattern (use after `--exclude`) |
| `--compress` | Compress transfers (auto-detected) |
| `-j, --parallel <N>` | Parallel transfers (default: 10) |

### Sync Modes

```bash
# Mirror mode
sy /source /dest --delete

# Directories only (no recursion)
sy /source /dest --dirs
```

### Remote Sync

```bash
# Push to remote
sy /local user@host:/remote

# Pull from remote
sy user@host:/remote /local

# With SSH timeout
sy /local user@host:/remote --timeout 30
```

### Filters

```bash
# Exclude patterns
sy /source /dest --exclude "*.log" --exclude ".git"

# Exclude from file
sy /source /dest --exclude-from .syignore

# Include specific patterns (after exclude)
sy /source /dest --exclude "*" --include "*.rs"
```

### Backup & Safety

```bash
# Backup before overwrite
sy /source /dest --backup

# Custom backup directory
sy /source /dest --backup --backup-dir /backups

# Custom suffix
sy /source /dest --backup --suffix .bak

# Force delete when threshold exceeded
sy /source /dest --delete --force-delete
```

### Verification

```bash
# Verify staged content against the source before commit
sy /source /dest --verify=after

# Audit file integrity without modifying anything
sy /source /dest --verify=only

# Show itemized changes
sy /source /dest --itemize-changes
```

Remote transfers are always BLAKE3-verified against the source before the
destination commit; `--verify` adds staged verification on local copies.

## Feature Status

| Feature | Status | Notes |
|---------|--------|-------|
| Local sync | Stable | Staged atomic commits, race-checked |
| SSH push | Stable | v3 protocol over `ssh host sy __serve` |
| SSH pull | Stable | Whole-file fetch with staged verification |
| Delta sync (push) | Stable | Rolling weak checksums + BLAKE3 strong block signatures |
| Filters (--exclude/--include) | Stable | rsync-style patterns, `--filter`, templates |
| Delete mode (--delete) | Stable | With `--max-delete` safety threshold |
| Compression (-z) | Stable | zstd, auto/always/never |
| Hard links (-H) | Stable | Preserved on local sync |
| Symlinks | Stable | `--links=preserve/follow/skip` |
| Backup mode (--backup) | Stable | Replacements and deletions |
| Atomic writes | Stable | Private staging, verified, atomic replace |
| Preservation (-X/-A/-F) | Stable | xattrs/ACLs/flags; applied to staging before commit |
| --bwlimit | Stable | Paced at the byte stream |
| --checksum | Stable | BLAKE3 content comparison instead of mtime+size |
| --update / --existing | Stable | Comparison modes for selective sync |
| --ignore-times / --ignore-existing | Stable | Force transfer / skip existing |
| --verify | Stable | Staged verification (`after`/`only`) |
| Directory type transitions | Not implemented | Refused in preflight before any mutation |
| Bidirectional sync (bisync) | Removed | Not part of 0.5 |
| S3/GCS endpoints | Not implemented | Planned after the filesystem/SSH engine |

## Benchmarks

Historical 0.4-era measurements (macOS M3 Max, NVMe); the 0.5 engine will be
re-benchmarked before release. Results vary by hardware, file sizes, and
workload.

| Scenario | sy | rsync | Speedup |
|----------|-----|-------|---------|
| 1000 × 1KB files | 189ms | 237ms | 1.25× |
| 10 × 10MB files | 29ms | 330ms | 11.5× |
| 1 × 100MB file | 38ms | 324ms | 8.6× |
| Incremental (no changes) | 33ms | 63ms | 1.9× |

Run benchmarks yourself:

```bash
cargo bench
```

## Configuration

sy reads `~/.config/sy/config.toml` for defaults:

```toml
max_concurrent = 8
compress = "auto"
exclude = [".git", "node_modules", "*.pyc"]
```

## Comparison to rsync

| Feature | sy | rsync |
|---------|-----|-------|
| Local sync speed | Fast (parallel) | Sequential |
| Delta sync | Yes (rolling + BLAKE3) | Yes (MD4) |
| COW reflinks | Yes | No |
| SSH sync | Yes | Yes |
| Wire protocol | Custom | rsync protocol |
| Incremental | Yes | Yes |
| Compression | zstd | zlib |

**sy is not a drop-in rsync replacement.** Same mental model, different protocol. For rsync-to-rsync compatibility, use rsync.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and workflow.

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for release history.

## License

MIT
