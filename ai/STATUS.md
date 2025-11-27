# Status

## Current State
- Version: v0.1.1 (released 2025-11-26)
- Test Coverage: **492+ tests passing** ✅
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

## Recent Work (Unreleased - v0.1.2)

### sy --server mode (Phase 1 - MVP)

**Status**: Implemented (Experimental)

| Feature | Status | Notes |
|---------|--------|-------|
| Protocol Definition | ✅ Done | Binary length-prefixed protocol |
| Server Handler | ✅ Done | Scan, Compare, Receive |
| Client Session | ✅ Done | SSH execution, Handshake, Streaming |
| Integration Test | ✅ Done | `tests/server_mode_test.rs` passing |
| CLI Integration | ✅ Done | Default for local→remote SSH |

**Benchmark Results (Mac → Fedora over Tailscale, ~30 MB/s link):**

| Test | rsync | sy | Notes |
|------|-------|-----|-------|
| 98MB/1000 files fresh | 3.3-3.5s | 3.3-3.5s | Parity |
| 488MB/5000 files fresh | 15.7s | 15.4s | Network-bound |
| No-op (all exist) | 1.3s | **0.48s** | **sy 2.7x faster** |
| Partial update (3 files) | **0.37s** | 0.46s | rsync wins (delta) |

**Key findings:**
- Fresh copies: parity (both hit ~31 MB/s network limit)
- Skip detection: sy 2.7x faster (better mtime comparison)
- Partial updates: rsync wins (delta sync sends only changes)

**Optimizations Applied (Session 2025-11-26):**
- ✅ Tilde (`~`) expansion in server root path
- ✅ Root directory creation on server
- ✅ Directory entries filtered from file list
- ✅ Server mode is now default for local→remote SSH
- ✅ Pipelined transfers (send all files, then read acks)
- ✅ Pre-read files with sync I/O (faster for small files)
- ✅ Batch flush (single flush after all sends)
- ✅ Return proper SyncStats with byte counts

**Phase 2 Refactoring (Session 2025-11-26):**
- ✅ MKDIR_BATCH protocol message for batched directory creation
- ✅ SYMLINK_BATCH protocol message for batched symlink creation
- ✅ Protocol flags for directory/symlink/hardlink/xattrs
- ✅ Server handler supports MKDIR_BATCH, SYMLINK_BATCH
- ✅ Client sends directories before files, symlinks after files
- ✅ Proper stats tracking (dirs_created, symlinks_created, files_updated)
- ✅ 12 new unit tests for protocol/handler

**Next Steps (Phase 3)**:
1. Delta sync (CHECKSUM_REQ/RESP, DELTA_DATA)
2. Compression option
3. Progress reporting

### SSH Transfer Optimizations (Interim)

| Optimization | Status | Impact |
|--------------|--------|--------|
| Batch mkdir | ✅ Done | 44K dirs in ~0.56s (was N round-trips) |
| Tar bulk transfer | ✅ Done | 100-1000x faster for bulk new files |
| Bulk transfer integration | ✅ Done | Auto-triggers for 100+ files |

**Testing results:**
- Batch mkdir: Working, tested
- Tar streaming: Working (132 files in 0.2s)
- Symlinks: Preserved correctly

**Known issues:**
- Tilde (`~`) in SSH paths not expanded (pre-existing bug) - **Confirmed critical for server mode**
- Tar approach is workaround, not SOTA

### Bug Fixes (discovered via 531K file sync test)
| Bug | Fix | File |
|-----|-----|------|
| Symlink `ln -s` fails if exists (SSH) | Use `ln -sf` (force) | `src/transport/ssh.rs:1941` |
| Symlink not overwritten (local) | Remove existing before create | `src/transport/local.rs:911-914` |
| Checkpoint save fails for SSH | Add `dest_is_remote` flag to SyncEngine | `src/sync/mod.rs:114,1300,1449` |
| Verification fails for SSH | Add `compute_checksum` to DualTransport | `src/transport/dual.rs:212-236` |
| Symlinks not detected in scan | Use `symlink_metadata` not `entry.metadata` | `src/sync/scanner.rs:217` |
| Symlink overwrite not working | Return `Create` for symlinks in planner | `src/sync/strategy.rs:407-421` |

**Root cause**: SSH/symlink codepaths were undertested.

### New Tests
| File | Tests | Coverage |
|------|-------|----------|
| `tests/symlink_overwrite_test.rs` | 5 | Symlink sync (empty, overwrite, skip identical) |
| `src/transport/dual.rs` | 3 | DualTransport compute_checksum routing |

### Planning Phase Optimization (v0.1.1 - Major SSH Performance Fix)
- **Problem**: Syncing 531K files over SSH took ~90 min just for planning
- **Root cause**: Sequential per-file SSH stat calls
- **Solution**: Batch destination scan + in-memory comparison
- **Result**: ~1000x fewer network round-trips

| Before | After |
|--------|-------|
| 531K SSH stat calls | 1 batch scan |
| ~90 min planning | ~30 sec planning |

**Changes**:
- `src/sync/mod.rs`: Scan destination once, build HashMap, compare in-memory
- `src/sync/strategy.rs`: Added `plan_file_with_dest_map()` for batch planning
- Progress indicator: Shows "Scanning destination..." and "Comparing X/Y files"

## Previous Work (v0.0.64)
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