# Contributing to sy

## Development Setup

This project uses modern Rust tooling:

```bash
# Clone the repo
git clone https://github.com/nijaru/sy.git
cd sy

# Build and run
cargo build
cargo run -- ./src ./dest --dry-run

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
│   ├── config.rs               # Config file parsing ✅
│   │
│   ├── sync/
│   │   ├── mod.rs              # Sync orchestration
│   │   ├── scanner.rs          # Directory traversal ✅
│   │   ├── strategy.rs         # Transfer strategy selection ✅
│   │   ├── transfer.rs         # File transfer logic ✅
│   │   ├── resume.rs           # Resume logic ✅
│   │   ├── watch.rs            # Watch mode ✅
│   │   ├── output.rs           # JSON output ✅
│   │   └── ratelimit.rs        # Rate limiting ✅
│   │
│   ├── delta/                  # Rsync algorithm ✅
│   │   ├── mod.rs
│   │   ├── rolling.rs          # Adler-32 rolling hash ✅
│   │   ├── checksum.rs         # Block checksums ✅
│   │   ├── generator.rs        # Delta generation ✅
│   │   └── applier.rs          # Delta application ✅
│   │
│   ├── transport/              # ✅
│   │   ├── mod.rs
│   │   ├── local.rs            # Local filesystem ✅
│   │   ├── ssh.rs              # SSH transport ✅
│   │   ├── router.rs           # Path routing ✅
│   │   └── dual.rs             # Dual transport ✅
│   │
│   ├── ssh/                    # ✅
│   │   ├── mod.rs
│   │   ├── config.rs           # SSH config parsing ✅
│   │   └── connect.rs          # Connection management ✅
│   │
│   ├── compress/               # ✅
│   │   └── mod.rs              # Zstd compression ✅
│   │
│   ├── filter/                 # Planned (Phase 6+)
│   │   └── (gitignore support via ignore crate for now)
│   │
│   ├── progress/               # Planned (Phase 5+)
│   │   └── (indicatif for now)
│   │
│   ├── metadata/               # Planned (Phase 6+)
│   │   └── (basic permissions only for now)
│   │
│   ├── bin/
│   │   └── sy-remote.rs        # Remote helper binary ✅
│   │
│   ├── error.rs                # Error types ✅
│   ├── path.rs                 # Path utilities ✅
│   └── lib.rs                  # Library root ✅
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

**Note**: The project follows [docs/MODERNIZATION_ROADMAP.md](docs/MODERNIZATION_ROADMAP.md) for detailed implementation planning. See [DESIGN.md](DESIGN.md) sections 2198-2330 for technical design roadmap.

### ✅ Phase 1: MVP - **COMPLETE** (v0.0.1)
**Goal**: Basic local sync working

- [x] Project structure and documentation
- [x] CLI argument parsing (clap)
- [x] Local filesystem traversal (ignore crate)
- [x] File comparison (size + mtime)
- [x] Full file copy with platform optimizations
- [x] Progress display (indicatif)
- [x] .gitignore support
- [x] Dry-run and delete modes
- [x] Comprehensive test suite

**Deliverable**: ✅ `sy /src /dst` works locally

### ✅ Phase 2: Network Sync + Delta - **COMPLETE** (v0.0.2-v0.0.3)
**Goal**: Remote sync with delta algorithm

- [x] Transport abstraction layer
- [x] SSH transport (SFTP-based)
- [x] SSH config parsing (~/.ssh/config)
- [x] Remote scanner (sy-remote binary)
- [x] **Delta sync** (rsync algorithm)
- [x] Adler-32 rolling hash + xxHash3 checksums
- [x] Block-level updates
- [x] Adaptive block size calculation

**Deliverable**: ✅ `sy /src user@host:/dst` works with delta sync

### ✅ Phase 3: Parallelism + Optimization - **COMPLETE** (v0.0.4-v0.0.10)
**Goal**: Parallel execution and compression

- [x] Parallel file transfers (5-10x speedup)
- [x] Parallel checksum computation (2-4x faster)
- [x] Configurable worker count (` -j` flag)
- [x] TRUE O(1) rolling hash (verified 2ns per operation)
- [x] Streaming delta generation (constant memory)
- [x] **Full compression integration** (Zstd level 3, 8 GB/s)
- [x] Compression stats tracking
- [x] Zero clippy warnings

**Deliverable**: ✅ Production-ready sync with 2-11x performance vs rsync

### ✅ Phase 4: Modern Features - **COMPLETE** (v0.0.11-v0.0.13)
**Goal**: JSON output, config profiles, watch mode, and resume support

- [x] JSON output (v0.0.11) - Machine-readable NDJSON format
- [x] Config profiles (v0.0.11) - Reusable configurations
- [x] Watch mode (v0.0.12) - Continuous sync with file watching
- [x] Resume support (v0.0.13) - Automatic recovery from interrupts

**Deliverable**: ✅ Modern CLI features for scripting and automation

### ✅ Phases 5-11: Reliability & Advanced Features - **COMPLETE** (v0.0.14-v0.0.57)
**Goals**: Multi-layer integrity, advanced features, scale to millions of files

- [x] Block-level checksums (xxHash3)
- [x] End-to-end verification (BLAKE3)
- [x] Verification modes (verify-only, --verify)
- [x] Atomic operations
- [x] Resume support
- [x] Bidirectional sync with conflict resolution
- [x] Watch mode, hooks, filters
- [x] Checksum database for fast re-syncs
- [x] SSH connection pooling
- [x] S3/cloud storage (experimental)
- [x] Metadata preservation (ACLs, xattrs, symlinks, hardlinks)
- [x] Scale testing (millions of files)

**Deliverable**: ✅ Production-ready feature-complete sync tool

See [DESIGN.md](DESIGN.md) and [CHANGELOG.md](CHANGELOG.md) for detailed implementation history.

## Testing Strategy

**Current Status**: 484 tests across unit, integration, and performance categories.

### Unit Tests
Located in `src/*/tests.rs` modules:
```bash
cargo test --lib
```

Tests include:
- Core sync functionality
- Delta sync (rolling hash, checksums, generation, applier)
- Compression (Zstd roundtrip, heuristics, ratios)
- SSH config parsing
- CLI argument validation
- Scanner and strategy modules

### Integration Tests
Located in `tests/*.rs`:
```bash
cargo test --test integration_test    # Full sync scenarios
cargo test --test edge_cases_test      # Unicode, nesting, special chars
cargo test --test property_test        # Invariant testing
cargo test --test compression_integration  # Compression end-to-end
```

Tests include:
- Full sync workflows (create, update, delete)
- Compression integration
- Edge cases (unicode, deep nesting, large files)
- Property-based tests (idempotency, correctness)
- Single file sync

### Performance Regression Tests
```bash
cargo test --release --test performance_test
```

Tests ensure performance stays within bounds:
- 100 files < 500ms
- 1000 files < 3s
- Large file (10MB) < 1s
- Deep nesting < 500ms
- Idempotent sync < 200ms
- Gitignore filtering < 500ms
- Memory bounded (5000 files) < 10s

### Benchmarks (Criterion)
```bash
cargo bench                           # Run all benchmarks
cargo bench --bench comparative_bench # Compare vs rsync/cp
cargo bench --bench delta_bench       # Delta sync performance
cargo bench --bench compress_bench    # Compression throughput
```

See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for benchmark usage and tracking.

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
4. **Check performance**: Run regression tests and benchmarks
5. **Update docs**: README, DESIGN.md if architecture changes
6. **Clean commits**: Squash WIP commits, write clear messages
7. **Run checks**:
   ```bash
   cargo test
   cargo test --release --test performance_test
   cargo clippy -- -D warnings
   cargo fmt -- --check
   ```

### Performance-Related Changes

If your PR affects performance:

1. **Run performance regression tests**:
   ```bash
   cargo test --release --test performance_test -- --nocapture
   ```

2. **Benchmark against main branch**:
   ```bash
   ./scripts/bench-compare.sh main
   ```

3. **Include results in PR description**:
   ```markdown
   ## Performance Impact

   Benchmarked against main branch:
   - sync_small_files/100: -15.2% (faster) ✓
   - sync_large_files/10MB: +2.1% (within threshold)
   - All regression tests passing
   ```

4. **Update baselines if intentionally faster**:
   - Update thresholds in `tests/performance_test.rs`
   - Document improvement in CHANGELOG.md

See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for detailed performance testing guide.

## Questions?

- **Design decisions**: See [DESIGN.md](DESIGN.md)
- **Issues**: [GitHub Issues](https://github.com/nijaru/sy/issues)
- **Discussions**: [GitHub Discussions](https://github.com/nijaru/sy/discussions)

## License

MIT - see [LICENSE](LICENSE)
