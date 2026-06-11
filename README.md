# sy

> Fast, modern file sync — rsync's mental model, Rust performance, sane defaults.

[![CI](https://github.com/nijaru/sy/workflows/CI/badge.svg)](https://github.com/nijaru/sy/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## Quick Start

```bash
sy /source /destination
```

Same trailing-slash semantics as rsync: `/source` copies the directory, `/source/` copies contents only.

## What sy Does Well

| Scenario | sy vs rsync | Why |
|----------|-------------|-----|
| Local sync (repeated) | 2-3x faster | Delta sync + parallel workers |
| Large files on COW FS | 40x+ faster | Reflink copies on APFS/BTRFS/XFS |
| Many files over SSH | 2x faster | Streaming protocol, 1 round-trip |
| Mixed local workloads | 2x faster | Parallel scan, hash, transfer |

## Where rsync Is Still Fine

- First-time local sync of small files (~1.1x faster)
- Incremental SSH updates (~1.3x faster — closing this gap is the v0.4 focus)
- rsync's massive flag set and ecosystem integrations

sy doesn't try to be wire-compatible with rsync. It's a modern tool that covers the same mental model for the common case.

## Installation

### Homebrew (macOS)

```bash
brew tap nijaru/tap
brew install sy
```

### From crates.io

```bash
cargo install sy

# Optional features
cargo install sy --features acl    # ACL preservation (Linux: requires libacl)
cargo install sy --features s3     # S3 support (experimental)
```

### From Source

```bash
git clone https://github.com/nijaru/sy.git
cd sy
cargo install --path .
```

**For SSH sync:** Install sy on both local and remote machines.

## Usage

```bash
# Basic
sy ~/project ~/backup                    # Local backup
sy ~/src ~/dest --delete                 # Mirror (remove extra files)
sy /source /dest --dry-run               # Preview changes

# Remote
sy /local user@host:/remote              # SSH sync
sy /local user@host:/backup --bwlimit 1MB

# Verification
sy ~/src ~/dest --verify                 # Verify writes (xxHash3)
sy ~/backup ~/original --verify-only     # Audit existing files

# Filters
sy ~/src ~/dest --exclude "*.log"
sy ~/src ~/dest --gitignore --exclude-vcs

# Advanced
sy --bidirectional /laptop /backup       # Two-way sync
sy ~/dev /backup --watch                 # Continuous sync
sy ~/src ~/dest -j 1                     # Sequential (many tiny files)
```

## Features

- **Delta sync** — Only transfers changed bytes (rsync algorithm)
- **Parallel transfers** — Configurable worker count (`-j`)
- **Streaming SSH** — Binary protocol, 1 round-trip delta, zstd compression
- **COW-aware** — Reflink copies on APFS/BTRFS/XFS (40x+ faster for large files)
- **Resume support** — Automatically resumes interrupted syncs
- **Integrity verification** — Optional xxHash3 checksums (`--verify`)
- **Bidirectional sync** — Two-way sync with conflict resolution
- **Watch mode** — Continuous file monitoring
- **S3 support** — AWS S3, Cloudflare R2, Backblaze B2 (experimental)
- **Metadata preservation** — Symlinks, permissions, xattrs, ACLs

## Platform Support

| Platform | Status |
|----------|--------|
| macOS | Fully tested |
| Linux | Fully tested |
| Windows | Untested (should compile) |

## Contributing

Contributions welcome! See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](LICENSE).
