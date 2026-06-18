# Generate man page from CLI definition
man:
    cargo run --bin man-gen

# Run all tests
test:
    cargo test

# Run clippy
lint:
    cargo clippy -- -D warnings

# Check everything before commit
check: lint test
    cargo fmt --check

# Format code
fmt:
    cargo fmt

# Build release
release:
    cargo build --release

# Run benchmarks
bench:
    cargo bench --no-run

# Clean build artifacts
clean:
    cargo clean

# Sync to fedora for testing
sync-fedora:
    rsync -av --exclude=target --exclude=.git --exclude=ai --exclude=.pi --exclude=.tasks . fedora:~/github/nijaru/sy/
