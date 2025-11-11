# Contributing to sy

## Development Setup

```bash
git clone https://github.com/nijaru/sy.git
cd sy

# Build and test
cargo build
cargo test

# Run locally
cargo run -- /source /dest --dry-run

# Format and lint
cargo fmt
cargo clippy
```

## Testing

```bash
# Run all tests
cargo test

# Run benchmarks
cargo bench
```

## Pull Request Process

1. Fork and create a feature branch
2. Write tests for new functionality
3. Run `cargo test && cargo clippy && cargo fmt`
4. Open PR with clear description
5. Wait for CI to pass

## Coding Standards

- Follow Rust conventions (`cargo fmt`)
- No clippy warnings
- Add tests for new features
- Update README if adding user-facing features

## Questions?

Open an issue: https://github.com/nijaru/sy/issues
