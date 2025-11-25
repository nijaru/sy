# Status

## Current State
- Version: v0.1.0 (released 2025-11-25)
- Test Coverage: **477+ tests passing** ✅
- **Current Build**: 🟢 PASSING

## v0.1.0 Breaking Changes - RELEASED

[Issue #11 feedback](https://github.com/nijaru/sy/issues/11#issuecomment-3573509820): defaults now match rsync behavior.

### Changes Made

| Change | Status |
|--------|--------|
| Flip `ScanOptions::default()` | ✅ Done |
| Add `--gitignore` flag (opt-in) | ✅ Done |
| Add `--exclude-vcs` flag (opt-in) | ✅ Done |
| Remove `--no-gitignore`, `--include-vcs` | ✅ Done |
| Remove `-b` short flag | ✅ Done |
| Add `-z` short flag for compress | ✅ Done |
| Add `-u`/`--update` flag | ✅ Done |
| Add `--ignore-existing` flag | ✅ Done |
| Update tests | ✅ Done |
| Update README.md | ✅ Done |
| Update CHANGELOG.md | ✅ Done |
| Bump version to 0.1.0 | ✅ Done |

### New Default Behavior

| Behavior | v0.0.x | v0.1.0 |
|----------|--------|--------|
| `.gitignore` | Respected (skip) | **Ignored (copy all)** |
| `.git/` dirs | Excluded | **Included** |

See `CHANGELOG.md` for migration guide.

### New Test Files
| File | Tests | Coverage |
|------|-------|----------|
| `tests/archive_mode_test.rs` | 10 | `-a`, `--include-vcs`, `--no-gitignore` |
| `tests/filter_cli_test.rs` | 11 | `--exclude`, `--include`, `--filter`, `--exclude-from` |
| `tests/comparison_modes_test.rs` | 8 | `--ignore-times`, `--size-only`, `--checksum` |
| `tests/size_filter_test.rs` | 9 | `--min-size`, `--max-size` |

### Bug Fixed
- `--filter` flag couldn't accept values starting with `-` (e.g., `--filter "- *.log"`)
- Added `allow_hyphen_values = true` to cli.rs

## Feature Flags
- SSH: Optional (enabled by default)
- Watch: Optional (disabled by default)
- ACL: Optional (Linux requires libacl-dev, macOS works natively)
- S3: Optional (disabled by default)

## Recent Work (v0.0.64)
- **Parallel Directory Scanning** - 1.5-1.7x faster for large directories
  - Uses `ignore::WalkParallel` with `crossbeam-channel` bridge
  - Dynamic selection: 30+ subdirs triggers parallel mode
  - Thread count capped at min(4, num_cpus)
  - 31 scanner tests for comprehensive coverage

## Benchmark Results
| Directory Structure | Sequential | Auto | Speedup |
|---------------------|------------|------|---------|
| 5,000 files / 50 subdirs | 18.9ms | 13.0ms | **1.45x** |
| 10,000 files / 100 subdirs | 40.1ms | 23.3ms | **1.72x** |
| 10,000 files / 200 subdirs | 42.2ms | 24.3ms | **1.74x** |

## Recent Releases

### v0.0.65 (Testing & Bug Fix)
- Fixed `--filter` flag to accept rsync-style patterns (e.g., `--filter "- *.log"`)
- 38 new integration tests for CLI flag behavior

### v0.0.64 (Performance)
- Parallel directory scanning with dynamic optimization
- Smart heuristic: counts subdirs, not files
- Scanner benchmark suite added

### v0.0.63 (Bug Fixes)
- Bisync timestamp overflow fix
- Size parsing overflow check
- CLI flag improvements (--no-resume)

### v0.0.62 (Performance)
- Parallel Chunk Transfers over SSH
- Adaptive Compression
- Adler32 Optimization (7x faster)
