# sy

[![CI](https://github.com/nijaru/sy/actions/workflows/ci.yml/badge.svg)](https://github.com/nijaru/sy/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Fast file synchronization. Same mental model as rsync, built in Rust.

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

- **Parallel** — uses all cores by default, `-j 1` to limit
- **Delta sync** — only transfers changed blocks for large files
- **COW support** — reflink copies on APFS/Btrfs/XFS
- **rsync-compatible flags** — `--delete`, `--exclude`, `--compress`, `--progress`, etc.
- **SSH sync** — streaming protocol over SSH stdin/stdout
- **Integrity** — BLAKE3 checksums, xxHash3 verification

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
| `--progress` | Show progress for large files |
| `--stats` | Show transfer statistics |
| `--exclude <PATTERN>` | Exclude files matching pattern |
| `--exclude-from <FILE>` | Read exclude patterns from file |
| `--include <PATTERN>` | Include files matching pattern (use after `--exclude`) |
| `--compress` | Compress transfers (auto-detected) |
| `-j, --max-concurrent <N>` | Parallel transfers (default: all cores) |

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
# Verify writes by reading back
sy /source /dest --verify

# Show itemized changes
sy /source /dest --itemize-changes
```

## Feature Status

| Feature | Status | Notes |
|---------|--------|-------|
| Local sync (push) | Stable | Fully tested |
| Local sync (pull) | Stable | Fully tested |
| SSH push | Stable | Tested with key-based auth |
| SSH pull | Stable | Tested with key-based auth |
| Delta sync | Stable | xxHash3 block-level diffs |
| Filters (--exclude/--include) | Stable | rsync-style patterns |
| Delete mode (--delete) | Stable | With --max-delete safety threshold |
| Compression | Stable | Auto-detected, zstd |
| Hard links | Stable | Preserved on local sync |
| Symlinks | Stable | Preserved |
| Backup mode (--backup) | Stable | |
| Atomic writes | Stable | Temp file + rename, all paths |
| --bwlimit | Stable | Bandwidth limit in bytes/sec |
| --checksum | Stable | Compares checksums instead of mtime+size |
| --update / --existing | Stable | Comparison modes for selective sync |
| --ignore-times / --ignore-existing | Stable | Force transfer / skip existing |
| --verify | Stable | xxHash3 verify-after-write |
| --partial | Not implemented | |
| --stream | Not implemented | |
| --retry | Not implemented | SSH has internal retry only |
| Bisync | Experimental | Works for simple cases; complex conflict resolution is limited |
| S3/GCS endpoints | Experimental | Code complete, not tested against real infrastructure |

## Benchmarks

Benchmarks on macOS M3 Max with NVMe storage. Results vary by hardware, file sizes, and workload.

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
| Delta sync | Yes (xxHash3) | Yes (MD4) |
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
