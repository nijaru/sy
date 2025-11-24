# TODO

## Active Release: v0.1.0 (Production Readiness)

### Critical

- [x] ~~**Bisync timestamp overflow** (`src/bisync/state.rs:421-427`)~~ Fixed in 61f450c
  - ~~`as_nanos() as i64` silently truncates timestamps, corrupting state files~~
  - ~~Also `.unwrap()` on `duration_since(UNIX_EPOCH)` panics on pre-epoch times~~

### High Priority

- [x] ~~**Duplicate `format_bytes()` function**~~ Fixed in 61f450c
  - ~~Two identical implementations - extracted to `resource::format_bytes()`~~

- [x] ~~**CLI flag design: `--resume`**~~ Fixed in 61f450c
  - ~~Added `--no-resume` flag (idiomatic)~~

- [x] ~~**Size parsing overflow** (`src/cli.rs:41`)~~ Fixed in 61f450c
  - ~~Added overflow check for values exceeding u64::MAX~~

### Medium Priority

- [ ] **Parallel scanner** (`src/sync/scanner.rs`) - DEFERRED
  - Current: Uses `WalkBuilder::build()` (serial iterator)
  - Need: `build_parallel()` with visitor pattern - significant refactor
  - Benefit: 2-4x speedup on directories with many subdirectories
  - Complexity: High - requires changing StreamingScanner from Iterator to channel-based

- [ ] **Sequential filter loading** (`src/main.rs:248-340`) - LOW VALUE
  - Filter files are typically small (dozens of patterns)
  - Parallelization would add complexity with minimal benefit

- [x] ~~**Archive mode docs** (`src/cli.rs:356-372`)~~ Fixed
  - ~~Flag interactions now documented in help text~~

- [x] ~~**S3 validation timing**~~ Fixed
  - ~~Moved S3+bidirectional check to CLI validation~~

- [ ] **Incomplete features with TODOs**
  - `src/sync/mod.rs:106` - `verify_only` field unused
  - `src/sync/transfer.rs:27` - BSD flags (macOS-only)
  - `src/transport/ssh.rs:1193` - BSD flags not serialized in SSH protocol

### Low Priority

- [ ] **Lock poisoning** (`src/sync/mod.rs` - multiple locations)
  - `monitor.lock().unwrap()` calls would cascade failures if thread panics

- [ ] **Unsafe `get_unchecked`** (`src/delta/rolling.rs:120`)
  - Safe due to u8 bounds but fragile if contract changes

### Platform Support

- [ ] Implement sparse file detection (Windows DeviceIoControl)
- [ ] Verify ACL mapping across platforms
- [ ] Test path handling edge cases (UNC paths, drive letters)

### Future Features (Post-v0.1.0)

- [ ] Re-evaluate SIMD for checksums if bottlenecks reappear
- [ ] Replace `libssh2` with pure Rust implementation for better portability
- [ ] Enable full two-way sync for cloud storage
