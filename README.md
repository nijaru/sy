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

### From crates.io (Recommended)

```bash
# Install sy (local + SSH sync)
cargo install sy

# With S3/cloud storage support (optional)
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

## Quick Start

```bash
# Basic sync
sy /source /destination

# Preview changes (dry-run)
sy /source /destination --dry-run

# Mirror mode (delete extra files)
sy /source /destination --delete

# SSH sync
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

This matters for:
- Directory structure: controls where files end up
- Bidirectional sync: ensures consistent paths on both sides
- SSH/S3 operations: works the same across all transports

See [docs/USAGE.md](docs/USAGE.md) for comprehensive examples.

## Key Features

- **Fast**: Parallel transfers, parallel checksums, smart caching
- **Reliable**: Multi-layer integrity verification (xxHash3, BLAKE3)
- **Smart**: Delta sync, compression auto-detection, sparse file optimization
- **Flexible**: Local, SSH, S3/cloud storage support
- **Safe**: Dry-run mode, deletion limits, automatic retry with resume
- **Modern**: Beautiful progress bars, JSON output, config profiles

**Advanced**:
- Bidirectional sync with 6 conflict resolution strategies
- Rsync-style filters and .gitignore support
- Hooks, watch mode, verify-only auditing
- Symlinks, hardlinks, ACLs, xattrs, BSD flags
- Checksum database for 10-100x faster re-syncs
- SSH connection pooling and sparse file transfer
- S3/cloud storage (experimental - AWS, R2, B2, Wasabi)

See [docs/FEATURES.md](docs/FEATURES.md) for detailed feature documentation.

## Common Use Cases

```bash
# Backup with verification
sy ~/project ~/backups/project --verify

# Sync with filters
sy ~/src ~/dest --exclude "*.log" --exclude "node_modules"

# Bandwidth-limited remote sync
sy /large user@host:/backup --bwlimit 1MB

# Watch mode for continuous sync
sy ~/dev /backup --watch

# Performance monitoring
sy /source /dest --perf

# Verify backup integrity (read-only)
sy ~/backup ~/original --verify-only
```

## Status

**Current Version: v0.0.56** - Production-ready for early adopters

Phases 1-11 complete (484 tests passing):
- Local & remote sync with delta algorithm
- Bidirectional sync with conflict resolution
- Parallel transfers, compression, sparse files
- Verification, integrity checking, resume support
- Advanced features: hooks, watch mode, filters
- S3/cloud storage (experimental - needs more testing)

See [CHANGELOG.md](CHANGELOG.md) for release history.

## Documentation

- [Usage Guide](docs/USAGE.md) - Comprehensive usage examples
- [Features](docs/FEATURES.md) - Detailed feature documentation
- [Design](DESIGN.md) - Technical design and architecture (2,400+ lines)
- [Performance](docs/PERFORMANCE.md) - Performance analysis and benchmarks
- [Contributing](CONTRIBUTING.md) - Development setup and guidelines
- [Troubleshooting](docs/TROUBLESHOOTING.md) - Common issues and solutions

Platform-specific guides:
- [Linux Support](docs/LINUX_SUPPORT.md)
- [macOS Support](docs/MACOS_SUPPORT.md)
- [Windows Support](docs/WINDOWS_SUPPORT.md)

## Comparison with rsync

| Feature | rsync | sy |
|---------|-------|-----|
| **Performance (local)** | baseline | **2-11x faster** |
| Parallel transfers | ❌ | ✅ |
| Parallel checksums | ❌ | ✅ |
| SSH connection pooling | ❌ | ✅ |
| Delta sync | ✅ | ✅ |
| Streaming delta (O(1) memory) | ❌ | ✅ |
| Cryptographic verification | ✅ MD5 | ✅ BLAKE3 |
| Compression auto-detection | ❌ | ✅ |
| S3/Cloud storage | ❌ | ✅ (experimental) |
| Bidirectional sync | ❌ | ✅ |
| Checksum database | ❌ | ✅ |
| Watch mode | ❌ | ✅ |
| JSON output | ❌ | ✅ |
| Modern UX | ❌ | ✅ |

See [docs/FEATURES.md](docs/FEATURES.md) for complete feature comparison.

## Contributing

Interested in contributing? We'd love help with:
- Cross-platform testing (Windows, Linux, macOS)
- Performance profiling for large datasets
- Real-world testing and feedback
- Documentation and tutorials

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup.

## License

MIT

## Acknowledgments

Inspired by **rsync**, **eza**, **fd**, **ripgrep**, and **Syncthing**.

Research: Jeff Geerling (2025) benchmarks, ACM 2024 QUIC analysis, ScienceDirect 2021 corruption studies.
