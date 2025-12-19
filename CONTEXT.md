# Session Context - Verification Default Change

## What We Did

### 1. Made `--verify` Opt-in (Implemented)

**Files changed:**

- `src/cli.rs` - Simplified VerificationMode enum, removed `--mode` flag
- `src/main.rs` - Updated verification output display
- `src/integrity/mod.rs` - Marked Cryptographic variant as dead_code
- `README.md` - Updated docs
- `ai/STATUS.md` - Updated performance notes

**Result:** sy now matches rsync behavior by default (no post-write verification). Users who want integrity checking can enable it with `--verify`.

### 2. Performance Improvement

| Command        | 1000 small files |
| -------------- | ---------------- |
| `sy` (default) | 158ms            |
| `rsync -a`     | 174ms            |
| `sy --verify`  | 207ms            |

sy now beats rsync by ~10% on small files initial sync.

### 3. xattr Overhead Investigation

Profiled xattr stripping on macOS:

- With xattrs: 115ms
- Without xattrs: 107ms
- Overhead: ~8ms/1000 files (negligible)

**Decision:** Keep current behavior - stripping is correct for rsync compatibility and overhead is minimal.

## Current Performance

| Scenario                    | sy vs rsync        |
| --------------------------- | ------------------ |
| Local incremental/delta     | **sy 3x faster**   |
| Local large files           | **sy 44x faster**  |
| Local small files (initial) | rsync 1.3x faster  |
| Mixed workloads             | **sy 2.3x faster** |
| Bulk SSH transfers          | **sy 2-4x faster** |
| SSH incremental/delta       | rsync 1.3x faster  |

## CLI Changes

**Before:**

```bash
sy /src /dst                    # xxHash3 verification (default)
sy /src /dst --mode=fast        # No verification
sy /src /dst --mode=paranoid    # BLAKE3 + block verification
```

**After:**

```bash
sy /src /dst                    # No verification (default, rsync-like)
sy /src /dst --verify           # xxHash3 verification
```

## Other Tradeoffs Considered

1. **xattr stripping** - ~8μs/file, keep as-is for rsync compat
2. **Resume checkpointing** - already optimized (100 files), `--no-resume` available
3. **Parallelism** - `-j 1` documented for small files workloads
4. **Auto `-j 1`** - decided against, heuristics too complex for marginal gain

## Commands

```bash
# Run benchmarks
python scripts/benchmark.py
python scripts/benchmark.py --ssh nick@fedora

# Quick test
cargo build --release
time ./target/release/sy /tmp/src /tmp/dst
time ./target/release/sy /tmp/src /tmp/dst --verify
time rsync -a /tmp/src/ /tmp/dst2/
```

## Key Files

- `ai/STATUS.md` - Current project status
- `ai/DESIGN.md` - Architecture
- `README.md` - User documentation
