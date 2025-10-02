# Contributing to sy

## Development Setup

This project uses modern Rust tooling:

```bash
# Clone the repo
git clone https://github.com/nijaru/sy.git
cd sy

# Build and run
cargo build
cargo run -- ./src ./dest --preview

# Run tests
cargo test

# Run benchmarks
cargo bench

# Format code
cargo fmt

# Lint
cargo clippy
```

## Architecture Overview

See [DESIGN.md](DESIGN.md) for comprehensive technical decisions.

### Key Principles

1. **Performance First**
   - Parallel file transfers (multiple files at once)
   - Parallel chunk transfers (split large files into chunks)
   - Fast hashing (xxHash3 > MD5)
   - Smart compression (adaptive based on network/CPU)

2. **Great UX**
   - Beautiful progress bars (indicatif)
   - Clear error messages
   - Sensible defaults (no flag soup like rsync)
   - Config file support for common tasks

3. **Safety**
   - Atomic operations
   - Resume interrupted transfers
   - Preview mode by default (with --preview flag)
   - Verification with BLAKE3

## Project Structure

```
sy/
├── src/
│   ├── main.rs           # CLI entry point
│   ├── sync/             # Core sync logic
│   │   ├── delta.rs      # rsync algorithm
│   │   ├── parallel.rs   # Parallel transfers
│   │   └── resume.rs     # Resume support
│   ├── hash/             # Hashing (xxHash3, BLAKE3)
│   ├── compress/         # Compression (zstd, lz4)
│   ├── transport/        # SSH/local transport
│   └── ui/               # Progress bars, output
├── benches/              # Criterion benchmarks
│   ├── hash_bench.rs
│   └── compression_bench.rs
└── tests/                # Integration tests
```

## Implementation Phases

### Phase 1: Foundation (Current)
- [x] CLI skeleton with clap
- [x] Project structure
- [x] Documentation (README, DESIGN, CONTRIBUTING)
- [ ] Basic file walking (walkdir + ignore)
- [ ] Simple copy (no delta, no compression)

### Phase 2: Core Sync
- [ ] rsync algorithm (rolling hash + delta)
- [ ] xxHash3 integrity checks
- [ ] Skip unchanged files (size + mtime)
- [ ] Basic progress bars

### Phase 3: Parallelization
- [ ] Parallel file transfers (tokio)
- [ ] Parallel chunk transfers for large files
- [ ] Worker pool management
- [ ] Throughput benchmarks

### Phase 4: Compression
- [ ] File type detection (skip already-compressed)
- [ ] Adaptive compression (test network vs CPU)
- [ ] zstd/lz4 integration
- [ ] Compression benchmarks

### Phase 5: Advanced Features
- [ ] Resume interrupted transfers
- [ ] BLAKE3 verification mode
- [ ] gitignore/syncignore support
- [ ] Config file support (~/.config/sy/config.toml)
- [ ] Bandwidth limiting

### Phase 6: Transport
- [ ] SSH integration (use existing ssh, not custom)
- [ ] Remote path parsing (user@host:/path)
- [ ] Network error handling

## Testing Strategy

### Unit Tests
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_xxhash3_correctness() {
        // Verify hash function correctness
    }

    #[test]
    fn test_compression_selection() {
        // Test file type detection logic
    }
}
```

### Integration Tests
```rust
// tests/sync_test.rs
#[test]
fn test_full_sync() {
    // Create temp dirs, sync, verify
}

#[test]
fn test_resume_interrupted() {
    // Simulate network failure, resume
}
```

### Benchmarks
```rust
// benches/hash_bench.rs
fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("xxhash3 1GB", |b| {
        b.iter(|| xxh3::hash64(black_box(&data)))
    });
}
```

### Property Tests
```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_sync_idempotent(files: Vec<FileData>) {
        // Syncing twice should be identical
    }
}
```

## Coding Standards

### Style
- Follow Rust standard style (`cargo fmt`)
- Clippy warnings are errors (`cargo clippy -- -D warnings`)
- Descriptive variable names (no `tmp`, `x`, `foo`)
- Document public APIs

### Error Handling
```rust
// Use anyhow for CLI errors
use anyhow::{Context, Result};

fn sync_files() -> Result<()> {
    let files = read_dir(path)
        .context("Failed to read source directory")?;
    Ok(())
}

// Use thiserror for library errors
use thiserror::Error;

#[derive(Error, Debug)]
enum SyncError {
    #[error("Permission denied: {path}")]
    PermissionDenied { path: PathBuf },
}
```

### Performance
- Avoid allocations in hot paths
- Use `&str` over `String` where possible
- Profile with `cargo flamegraph` before optimizing
- Benchmark before/after for performance changes

## Research & References

### Papers
- "Data Synchronization: A Complete Theoretical Solution for Filesystems" (2022, MDPI)
- "File Synchronization Systems Survey" (arXiv:1611.05346)
- rsync algorithm: rolling hash + checksums

### Benchmarks Consulted
- rclone vs rsync: 4x speedup with parallel transfers (Jeff Geerling, 2025)
- xxHash vs MD5: 10x faster on 6GB files
- BLAKE3 vs SHA-2: 10x faster, parallelizable
- zstd vs lz4: zstd better ratio, lz4 faster (~500MB/s)

### Tools Analyzed
- **rclone**: Multi-thread streams, parallel transfers
- **rusync**: Minimalist Rust rsync
- **fd/rg/eza**: Modern CLI UX patterns

## Pull Request Guidelines

1. **Create feature branch**: `git checkout -b feature/parallel-chunks`
2. **Write tests**: Cover new functionality
3. **Benchmark if perf-related**: Show before/after numbers
4. **Update docs**: README, DESIGN.md if architecture changes
5. **Clean commits**: Squash WIP commits, write clear messages
6. **Run checks**:
   ```bash
   cargo test
   cargo clippy -- -D warnings
   cargo fmt -- --check
   ```

## Questions?

- **Design decisions**: See [DESIGN.md](DESIGN.md)
- **Issues**: [GitHub Issues](https://github.com/nijaru/sy/issues)
- **Discussions**: [GitHub Discussions](https://github.com/nijaru/sy/discussions)

## License

MIT - see [LICENSE](LICENSE)
