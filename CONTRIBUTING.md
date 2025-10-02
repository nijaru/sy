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

See [DESIGN.md](DESIGN.md) section 2024-2092 for complete module organization.

```
sy/
├── src/
│   ├── main.rs                 # CLI entry point
│   ├── cli.rs                  # Argument parsing (clap)
│   ├── config.rs               # Config file parsing
│   │
│   ├── sync/
│   │   ├── mod.rs              # Sync orchestration
│   │   ├── scanner.rs          # Directory traversal
│   │   ├── strategy.rs         # Transfer strategy selection
│   │   ├── transfer.rs         # File transfer logic
│   │   ├── delta.rs            # Rsync algorithm
│   │   └── resume.rs           # Resume logic
│   │
│   ├── integrity/
│   │   ├── mod.rs
│   │   ├── hash.rs             # xxHash3, BLAKE3, Adler-32
│   │   ├── checksum.rs         # Block-level checksums
│   │   └── verify.rs           # Verification modes
│   │
│   ├── transport/
│   │   ├── mod.rs
│   │   ├── local.rs            # Local filesystem
│   │   ├── ssh.rs              # SSH custom protocol
│   │   ├── sftp.rs             # SFTP fallback
│   │   └── network.rs          # Network detection
│   │
│   ├── compress/
│   │   ├── mod.rs
│   │   ├── zstd.rs             # Zstandard
│   │   ├── lz4.rs              # LZ4
│   │   └── adaptive.rs         # Compression selection
│   │
│   ├── filter/
│   │   ├── mod.rs
│   │   ├── gitignore.rs        # Gitignore parser
│   │   ├── rsync.rs            # Rsync filter rules
│   │   └── engine.rs           # Filter matching engine
│   │
│   ├── progress/
│   │   ├── mod.rs
│   │   ├── tracker.rs          # Progress tracking
│   │   ├── display.rs          # Terminal UI
│   │   └── eta.rs              # ETA calculation
│   │
│   ├── metadata/
│   │   ├── mod.rs
│   │   ├── permissions.rs      # Unix permissions
│   │   ├── xattr.rs            # Extended attributes
│   │   └── acl.rs              # Access control lists
│   │
│   ├── error.rs                # Error types
│   ├── bandwidth.rs            # Token bucket rate limiting
│   └── ssh_config.rs           # SSH config parsing
│
├── tests/
│   ├── integration/            # Integration tests
│   ├── property/               # Property tests (proptest)
│   └── stress/                 # Stress tests
│
├── benches/                    # Criterion benchmarks
├── docs/                       # User documentation
├── .claude/
│   └── CLAUDE.md               # AI assistant context
├── Cargo.toml
├── README.md
├── DESIGN.md                   # Comprehensive technical design (2,400+ lines)
└── CONTRIBUTING.md             # This file
```

## Implementation Roadmap

See [DESIGN.md](DESIGN.md) sections 2198-2330 for complete roadmap details.

### Phase 1: MVP (v0.1.0) - **Current Phase**
**Goal**: Basic local sync working

- [x] Project structure
- [x] Documentation (README, DESIGN, CONTRIBUTING, CLAUDE.md)
- [ ] CLI argument parsing (clap)
- [ ] Local filesystem traversal (walkdir + ignore)
- [ ] File comparison (size + mtime)
- [ ] Full file copy (no delta)
- [ ] Basic progress display (indicatif)
- [ ] Unit tests

**Deliverable**: `sy /src /dst` works locally

### Phase 2: Network Sync (v0.2.0)
**Goal**: Remote sync via SSH

- [ ] SSH transport layer
- [ ] SFTP fallback
- [ ] Network bandwidth detection
- [ ] SSH config parsing
- [ ] Basic error handling

**Deliverable**: `sy /src remote:/dst` works

### Phase 3: Performance (v0.3.0)
**Goal**: Parallel transfers

- [ ] Parallel file transfers
- [ ] Parallel chunk transfers
- [ ] Adaptive compression
- [ ] Network detection (LAN vs WAN)
- [ ] Progress UI at scale

**Deliverable**: Fast sync for various scenarios

### Phase 4: Delta Sync (v0.4.0)
**Goal**: Rsync algorithm

- [ ] Adler-32 rolling hash
- [ ] Block signature generation
- [ ] Delta computation
- [ ] Resume support

**Deliverable**: Efficient updates of changed files

### Phase 5: Reliability (v0.5.0)
**Goal**: Multi-layer integrity

- [ ] Block-level checksums (xxHash3)
- [ ] End-to-end verification (BLAKE3)
- [ ] Verification modes
- [ ] Atomic operations
- [ ] Crash recovery

**Deliverable**: Verifiable integrity

### Phases 6-10
See [DESIGN.md](DESIGN.md) for:
- Phase 6: Edge cases & advanced features
- Phase 7: Scale to millions of files
- Phase 8: UX polish
- Phase 9: Testing & documentation
- Phase 10: v1.0 release

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
