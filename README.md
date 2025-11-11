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

See [docs/BENCHMARK_RESULTS.md](docs/BENCHMARK_RESULTS.md) for detailed benchmarks.

## Installation

### Homebrew (macOS)

```bash
brew tap nijaru/tap
brew install sy
```

### From crates.io

```bash
# Install sy (local + SSH sync)
cargo install sy

# With S3/cloud storage support (optional, experimental)
cargo install sy --features s3

sy --version
```

### From Source

```bash
git clone https://github.com/nijaru/sy.git
cd sy
cargo install --path .

# Or with S3 support
cargo install --path . --features s3
```

Requirements: Rust 1.70+

**For SSH sync:** Install sy on both local and remote machines. The remote server needs `sy` (or just `sy-remote`) in PATH.

## Quick Start

```bash
# Basic sync
sy /source /destination

# Preview changes (dry-run)
sy /source /destination --dry-run

# Mirror mode (delete extra files)
sy /source /destination --delete

# SSH sync (requires sy installed on remote)
sy /local user@host:/remote

# S3 sync (experimental, requires --features s3)
sy /local s3://my-bucket/backups/

# Bidirectional sync
sy --bidirectional /laptop/docs /backup/docs
```

### Trailing Slash Behavior (rsync-compatible)

sy follows rsync semantics for directory copying:

```bash
# Without trailing slash: copies directory itself
sy /a/myproject /target
# Result: /target/myproject/

# With trailing slash: copies contents only
sy /a/myproject/ /target
# Result: /target/ (contents copied directly)
```

See [docs/USAGE.md](docs/USAGE.md) for comprehensive examples.

## Features

### Core Performance
- **Parallel transfers**: Multiple files transferred simultaneously
- **Parallel checksums**: Fast integrity verification with xxHash3 and BLAKE3
- **Delta sync**: Block-level updates using rsync algorithm (streaming, O(1) memory)
- **Smart caching**: Checksum database for 10-100x faster re-syncs
- **Compression auto-detection**: Skips already-compressed files
- **SSH connection pooling**: Reuses connections for efficiency
- **Sparse file optimization**: Efficient handling of sparse files

### Transports
- **Local**: Fast local filesystem sync
- **SSH**: Remote sync over SSH (requires sy on remote)
- **S3/Cloud**: AWS S3, Cloudflare R2, Backblaze B2, Wasabi (experimental)

### Reliability
- **Multi-layer integrity**: xxHash3 (fast) and BLAKE3 (cryptographic) verification
- **Atomic operations**: Safe file updates
- **Resume support**: Automatic recovery from interruptions
- **Dry-run mode**: Preview changes before applying
- **Verify-only mode**: Audit backups without modifying files

### Advanced Features
- **Bidirectional sync**: 6 conflict resolution strategies
- **Watch mode**: Continuous sync with file monitoring
- **Filters**: Rsync-style patterns and .gitignore support
- **Hooks**: Pre/post sync automation
- **Metadata preservation**: Symlinks, hardlinks, ACLs, xattrs, BSD flags
- **JSON output**: Machine-readable progress and statistics
- **Config profiles**: Reusable sync configurations
- **Modern UX**: Beautiful progress bars and clear error messages

See [docs/FEATURES.md](docs/FEATURES.md) for detailed feature documentation.

## Common Use Cases

```bash
# Backup with verification
sy ~/project ~/backups/project --verify

# Sync with filters
sy ~/src ~/dest --exclude "*.log" --exclude "node_modules"

# Bandwidth-limited remote sync (sy must be installed on remote)
sy /large user@host:/backup --bwlimit 1MB

# Watch mode for continuous sync
sy ~/dev /backup --watch

# Performance monitoring
sy /source /dest --perf

# Verify backup integrity (read-only)
sy ~/backup ~/original --verify-only
```

## Platform Support

- ✅ **macOS**: Fully tested and supported
- ✅ **Linux**: Fully tested and supported (Fedora, Ubuntu, etc.)
- ⚠️ **Windows**: Untested - should compile but not officially supported
  - Some features unavailable (sparse file detection)
  - CI testing currently macOS and Linux only

See [docs/FEATURES.md](docs/FEATURES.md) for platform-specific feature details.

## Documentation

- [Usage Guide](docs/USAGE.md) - Comprehensive usage examples
- [Features](docs/FEATURES.md) - Detailed feature documentation
- [Design](DESIGN.md) - Technical design and architecture
- [Performance](docs/PERFORMANCE.md) - Performance analysis and benchmarks
- [Contributing](CONTRIBUTING.md) - Development setup and guidelines
- [Troubleshooting](docs/TROUBLESHOOTING.md) - Common issues and solutions

## Contributing

Contributions welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

Especially interested in:
- Windows testing and support
- Performance profiling for large datasets
- Real-world testing and feedback

## License

MIT License - see [LICENSE](LICENSE) file for details.
