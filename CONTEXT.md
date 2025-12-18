# Session Context - SSH Performance Optimization

## What We Did

### 1. Server-Side Parallelism (Implemented, Didn't Help SSH Gap)

**Files changed:**

- `src/server/handler.rs` - Rayon parallel block checksums, `compute_checksum_response()` function
- `src/server/mod.rs` - Concurrent CHECKSUM_REQ handling with tokio channels + `select!`

**Result:** Architecturally correct but didn't close SSH incremental/delta gap. Bottleneck is network latency, not CPU.

### 2. Checkpoint Frequency Fix (Implemented, Helped)

**File:** `src/cli.rs`

- Changed `checkpoint_files` default from 10 to 100
- Was writing resume state every 10 files, now every 100

### 3. README Honesty Update (Done)

Updated performance claims to be accurate:

- sy wins: incremental (3x), large files (8x), bulk SSH (2-4x)
- rsync wins: initial small files (~1.5x), SSH incremental (~1.3x)

## Current Performance

| Scenario                        | sy vs rsync        |
| ------------------------------- | ------------------ |
| Local incremental/delta         | **sy 3x faster**   |
| Local large files               | **sy 8x faster**   |
| SSH bulk transfers              | **sy 2-4x faster** |
| SSH incremental/delta           | rsync 1.3x faster  |
| Initial sync (many small files) | rsync 1.5x faster  |

## What's Left to Investigate

### Initial Sync Small Files Gap

sy spends 0.7s in syscalls vs rsync 0.2s for 1000 files. Likely causes:

1. Per-file xxHash3 verification after write
2. Per-file xattr operations
3. Parallelism overhead for tiny files

Benchmark results are variable (sometimes sy wins, sometimes loses by 5x).

### Potential Optimizations

1. **Skip verification on initial sync** - file didn't exist, nothing to verify against
2. **Batch xattr operations** - reduce per-file syscall overhead
3. **Sequential mode for small files** - parallelism overhead may exceed benefit

## Commands

```bash
# Run benchmarks
python scripts/benchmark.py                    # Local
python scripts/benchmark.py --ssh nick@fedora  # SSH
python scripts/benchmark.py --scenario small_files  # Specific scenario

# Quick test
cargo build --release
time ./target/release/sy /tmp/src /tmp/dst
time rsync -a /tmp/src/ /tmp/dst2/
```

## Key Files

- `ai/STATUS.md` - Current project status
- `ai/DESIGN.md` - Architecture
- `scripts/benchmark.py` - Benchmark runner
- `benchmarks/history.jsonl` - Benchmark history

## Uncommitted Changes

Run `git status` - should have:

- `src/cli.rs` - checkpoint_files 10→100
- Benchmark history updates
