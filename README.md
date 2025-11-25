# sy

> Modern file synchronization tool - rsync, reimagined

**sy** (pronounced "sigh") is a fast, modern file synchronization tool that's 2-11x faster than rsync for local operations. It's not a drop-in rsync replacement—it's a reimagining of file sync with verifiable integrity, adaptive performance, and transparent tradeoffs.

[![CI](https://github.com/nijaru/sy/workflows/CI/badge.svg)](https://github.com/nijaru/sy/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## Why sy?

**2-11x faster than rsync** for local operations:
- 8.8x faster for large files (50MB: 21ms vs 185ms)
- 60% faster for many small files (100 files: 25ms vs 40ms)
- 2x faster for idempotent syncs (8ms vs 17ms)
- 11x faster for real-world workloads (500 files: <10ms vs 110ms)

## Installation

### Homebrew (macOS)

```bash
brew tap nijaru/tap
brew install sy
```

### From crates.io

```bash
cargo install sy

# With optional features
cargo install sy --features acl         # ACL preservation (requires libacl on Linux)
cargo install sy --features s3          # S3 support
cargo install sy --features acl,s3      # Both features
```

### From Source

```bash
git clone https://github.com/nijaru/sy.git
cd sy
cargo install --path .
```

**Build requirements:**
- Rust toolchain (any recent stable version)
- Linux only: For ACL support (`--features acl`), install `libacl1-dev` (Debian/Ubuntu) or `libacl-devel` (Fedora/RHEL)
- macOS: ACL support works out of the box (native support)

**For SSH sync:** Install sy on both local and remote machines.

## Quick Start

```bash
sy /source /destination
```

That's it. Use `sy --help` for options.

> **Directory behavior:** sy follows rsync semantics - `/source` copies the directory, `/source/` copies contents only.

## Examples

### Backup & Sync
```bash
sy ~/project ~/backup                    # Basic backup
sy ~/src ~/dest --delete                 # Mirror (delete extra files)
sy ~/backup ~/original --verify-only     # Verify integrity
sy /source /dest --dry-run               # Preview changes
```

### Remote Sync
```bash
sy /local user@host:/remote              # SSH sync
sy /large user@host:/backup --bwlimit 1MB
sy /local s3://bucket/path               # S3 sync
```

### Advanced
```bash
sy ~/src ~/dest --exclude "*.log"        # With filters
sy ~/dev /backup --watch                 # Continuous sync
sy --bidirectional /laptop /backup       # Two-way sync
sy ~/src ~/dest -u                       # Skip files where dest is newer
sy ~/src ~/dest --ignore-existing        # Skip files that already exist
sy ~/src ~/dest --gitignore              # Respect .gitignore rules
sy ~/src ~/dest --gitignore --exclude-vcs # Developer workflow (no .git, respect .gitignore)
```

### S3 & Cloud Storage

sy supports AWS S3 and compatible services (Cloudflare R2, Backblaze B2, Wasabi, MinIO).

**Authentication:**
Standard AWS environment variables are supported:
```bash
export AWS_ACCESS_KEY_ID="your-key-id"
export AWS_SECRET_ACCESS_KEY="your-secret-key"
export AWS_REGION="us-east-1"
```

**Usage:**
```bash
# Basic S3 sync
sy /local/path s3://my-bucket/backups

# With custom region
sy /local/path s3://my-bucket/backups?region=eu-central-1

# With custom endpoint (e.g., Cloudflare R2)
sy /local/path s3://my-bucket/backups?endpoint=https://<accountid>.r2.cloudflarestorage.com
```

## Features

**Core Performance:**
- Parallel transfers and checksums
- Delta sync (rsync algorithm, O(1) memory)
- Checksum database (10-100x faster re-syncs)
- Compression auto-detection
- Sparse file optimization

**Transports:**
- Local filesystem
- SSH (requires sy on remote)
- S3/cloud storage support (AWS S3, Cloudflare R2, Backblaze B2)

**Reliability:**
- Multi-layer integrity (xxHash3 + BLAKE3)
- Atomic operations
- Resume support
- Dry-run and verify-only modes

**Advanced:**
- Bidirectional sync with conflict resolution
- Watch mode for continuous sync
- Rsync-style filters and .gitignore support
- Hooks, JSON output, config profiles
- Metadata preservation (symlinks, ACLs, xattrs)

## Platform Support

- ✅ **macOS**: Fully tested
- ✅ **Linux**: Fully tested
- ⚠️ **Windows**: Untested (should compile)

## Contributing

Contributions welcome! See [CONTRIBUTING.md](CONTRIBUTING.md).

Interested in:
- Windows testing
- Performance profiling
- Real-world feedback

## License

MIT License - see [LICENSE](LICENSE) file for details.
