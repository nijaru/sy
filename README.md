# sy

Modern rsync alternative in Rust - Fast, parallel file synchronization with smart compression and beautiful output.

## Why `sy`?

Modern CLI tools like `fd`, `rg`, `bat`, and `eza` showed that reimplementing classic Unix tools with better defaults and UX creates massive value. `sy` does the same for `rsync`:

- **Better defaults** - No more `-avz` incantations; sensible behavior out of the box
- **Parallel everything** - Multiple files + chunked large files (like rclone's `--multi-thread-streams`)
- **Smart compression** - Adaptive zstd/lz4 with file-type awareness, or skip entirely on fast networks
- **Beautiful output** - Progress bars, colors, human-readable sizes (not spam)
- **Intuitive CLI** - Simple commands that make sense

## Quick Start

```bash
# Basic sync
sy ./src remote:/dest

# Preview changes first
sy ./src remote:/dest --preview

# Fast mode (LZ4 compression)
sy ./src remote:/dest --fast

# Max compression for slow networks
sy ./src remote:/dest --max-compression

# Verify integrity after transfer
sy ./src remote:/dest --verify
```

## Key Features

### Performance
- **Parallel file transfers** - Configurable worker threads
- **Parallel chunk transfers** - Split large files, transfer chunks concurrently
- **Fast hashing** - xxHash3 (10x faster than MD5) for integrity checks
- **Smart delta sync** - rsync algorithm with modern optimizations

### Compression
- **Adaptive** - Tests network vs CPU speed, chooses best strategy
- **File-type aware** - Skips already-compressed files (.jpg, .mp4, .zip, etc.)
- **Modern algorithms** - zstd (balanced), lz4 (fast), or none (LAN speeds)
- **Auto-tuning** - Benchmarks first chunk, caches decision per connection

### Safety & Reliability
- **Atomic operations** - Safe transfers, no partial corruption
- **Resume support** - Continue interrupted transfers
- **Preview mode** - See changes before applying
- **Verification** - Optional BLAKE3 cryptographic checksums

### UX
- **gitignore support** - Respects `.gitignore` and `.syncignore` files
- **Config files** - Save common sync pairs in `~/.config/sy/config.toml`
- **Clear errors** - "Permission denied on /path/file.txt" not cryptic codes
- **Progress bars** - Real-time speed, ETA, file/overall progress

## Design Philosophy

Inspired by modern Rust CLI tools:
- **Minimal** - Short command like `fd`, obvious meaning
- **Smart** - Good defaults, don't require flags for common use
- **Fast** - Rust performance, parallel operations
- **Beautiful** - Terminal UX matters

## Technical Stack

- **rsync algorithm** - Rolling hash + checksums (state-of-the-art for delta sync)
- **xxHash3** - Fast integrity checks (non-cryptographic)
- **BLAKE3** - Cryptographic verification when requested
- **zstd/lz4** - Modern compression algorithms
- **tokio** - Async runtime for parallel operations
- **indicatif** - Beautiful progress bars

## Roadmap

- [ ] Core rsync algorithm implementation
- [ ] Parallel file transfers
- [ ] Parallel chunk transfers for large files
- [ ] Adaptive compression with file-type detection
- [ ] gitignore/syncignore support
- [ ] Config file support
- [ ] Bandwidth limiting
- [ ] Resume interrupted transfers
- [ ] Verification mode with BLAKE3
- [ ] SSH transport layer

## Development

Built with modern Rust tooling:

```bash
# Setup (using uv)
uv sync

# Run
uv run sy --help

# Test
uv run pytest

# Benchmark
uv run criterion
```

## Inspiration

- Modern CLI tools: `fd`, `rg`, `bat`, `eza`, `dust`
- Fast sync: `rclone` parallel transfers, `rsync` algorithm
- Research: xxHash3/BLAKE3 performance, zstd adaptive compression

## License

MIT
