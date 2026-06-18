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
| `--compress <MODE>` | Compression: auto, always, never |
| `-j, --max-concurrent <N>` | Parallel transfers (default: all cores) |
| `--bwlimit <RATE>` | Bandwidth limit (e.g., 1MB, 500KB) |

### Sync Modes

```bash
# Mirror mode
sy /source /dest --delete

# Update only (skip newer dest files)
sy /source /dest --update

# Existing only (don't create new files)
sy /source /dest --existing

# Directories only (no recursion)
sy /source /dest --dirs
```

### Remote Sync

```bash
# Push to remote
sy /local user@host:/remote

# Pull from remote
sy user@host:/remote /local

# With SSH options
sy /local user@host:/remote --timeout 30
```

### Filters

```bash
# Exclude patterns
sy /source /dest --exclude "*.log" --exclude ".git"

# Exclude from file
sy /source /dest --exclude-from .syignore

# Include specific patterns
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
# Verify after write
sy /source /dest --verify

# Show itemized changes
sy /source /dest --itemize-changes
```

## Benchmarks

Preliminary benchmarks on macOS M3 Max with NVMe storage. Results vary significantly by hardware, file sizes, and workload.

| Scenario | sy | rsync | Speedup |
|----------|-----|-------|---------|
| 1000 × 1KB files | 189ms | 237ms | 1.25× |
| 10 × 10MB files | 29ms | 330ms | 11.5× |
| 1 × 100MB file | 38ms | 324ms | 8.6× |
| Incremental (no changes) | 33ms | 63ms | 1.9× |

**Note:** These are limited benchmarks on a single machine. We recommend running your own benchmarks for your specific use case.

Run benchmarks yourself:

```bash
cargo bench
```

## Configuration

sy reads `~/.config/sy/config.toml` for defaults:

```toml
# Example config
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

**sy is not a drop-in rsync replacement.** It uses the same mental model but a different protocol. For rsync-to-rsync compatibility, use rsync.

## Contributing

```bash
# Build
cargo build

# Test
cargo test

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt --check
```

## License

MIT
