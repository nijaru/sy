# sy

> Modern file synchronization tool - rsync, reimagined

**sy** (pronounced "sigh") is a fast, modern file synchronization tool inspired by the UX of `eza`, `fd`, and `ripgrep`. It's not a drop-in rsync replacement - it's a reimagining of file sync with verifiable integrity, adaptive performance, and transparent tradeoffs.

## Why sy?

**sy is 2-11x faster than rsync** for local operations:
- ✅ **8.8x faster** than rsync for large files (50MB: 21ms vs 185ms)
- ✅ **60% faster** than rsync for many small files (100 files: 25ms vs 40ms)
- ✅ **2x faster** for idempotent syncs (no changes: 8ms vs 17ms)
- ✅ **11x faster** for real-world workloads (500 files: <10ms vs 110ms)

See [docs/BENCHMARK_RESULTS.md](docs/BENCHMARK_RESULTS.md) for detailed benchmarks.

## Status

✅ **Phase 1 MVP Complete** - Basic local sync working!
✅ **Phase 2 Complete** - SSH transport + Delta sync implemented! (v0.0.3)
✅ **Phase 3 Complete** - Parallel transfers + UX polish! (v0.0.4-v0.0.9)
✅ **Phase 3.5 Complete** - Full compression + parallel checksums! (v0.0.10)
✅ **Phase 4 Complete** - JSON output, config profiles, watch mode, resume support! (v0.0.11-v0.0.13)
✅ **Phase 5 Complete** - BLAKE3 verification, symlinks, sparse files, xattrs! (v0.0.14-v0.0.16)
✅ **Phase 6 Complete** - Hardlink & ACL preservation! (v0.0.17)
✅ **Phase 7 Complete** - Rsync-style filters & remote→local sync! (v0.0.18)
✅ **Phase 8 Complete** - Cross-transport delta sync & xxHash3! (v0.0.19-v0.0.21)
✅ **Phase 9 Complete** - Developer Experience (Hooks ✅, Ignore templates ✅, Improved dry-run ✅) (v0.0.22)
✅ **Phase 10 Complete** - S3/Cloud Storage (AWS S3, Cloudflare R2, Backblaze B2, Wasabi!) (v0.0.22)
✅ **Phase 11 Complete** - Scale (Incremental scanning ✅, Bloom filters ✅, Cache ✅, O(1) memory!) (v0.0.22)
✅ **Performance Monitoring** - Detailed performance metrics with `--perf` flag! (v0.0.33)
✅ **Error Reporting** - Comprehensive error collection and reporting! (v0.0.34)
✅ **Pre-Transfer Checksums** - Compare checksums before transfer to skip identical files! (v0.0.35)
✅ **Checksum Database** - Persistent SQLite cache for 10-100x faster re-syncs! (v0.0.35)
✅ **Verify-Only Mode** - Audit file integrity without modification, JSON output! (v0.0.36)
✅ **Compression Auto-Detection** - Content-based sampling for smart compression! (v0.0.37)
✅ **Enhanced Progress Display** - Byte-based progress with transfer speed and current file! (v0.0.38)
✅ **Bandwidth Utilization JSON** - Performance metrics including bandwidth % in JSON output! (v0.0.39)
✅ **SSH Connection Pooling** - True parallel SSH transfers with N workers = N connections! (v0.0.42)
✅ **SSH Sparse File Transfer** - Automatic sparse file optimization for 10x bandwidth savings! (v0.0.42)
✅ **Bidirectional Sync** - Two-way sync with automatic conflict resolution, 6 strategies! (v0.0.43)
✅ **SSH Bidirectional Sync** - Bisync works with remote servers (local↔remote, remote↔remote)! (v0.0.46)
🚀 **Current Version: v0.0.46-dev** - 410 tests passing!

[![CI](https://github.com/nijaru/sy/workflows/CI/badge.svg)](https://github.com/nijaru/sy/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

See [DESIGN.md](DESIGN.md) for comprehensive technical design (2,400+ lines of detailed specifications).

## Installation

### From crates.io (Recommended)

```bash
# Install sy and sy-remote
cargo install sy

# Verify installation
sy --version
```

### From Source

```bash
# Clone the repository
git clone https://github.com/nijaru/sy.git
cd sy

# Build and install
cargo install --path .

# Verify installation
sy --version
```

### Man Pages (Optional)

Man pages are included in the repository. To install them:

```bash
# Clone if not already done
git clone https://github.com/nijaru/sy.git
cd sy

# Install man pages (Unix/Linux/macOS)
sudo mkdir -p /usr/local/share/man/man1
sudo cp man/*.1 /usr/local/share/man/man1/

# Verify installation
man sy
man sy-remote
```

### Requirements

- Rust 1.70+ (for installation)
- Git (for .gitignore support)

## Quick Start

```bash
# Basic sync
sy /source /destination

# Preview changes (dry-run)
sy /source /destination --dry-run

# Detailed dry-run with file sizes and byte impact (Phase 9)
sy /source /destination --dry-run --diff

# Mirror mode (delete extra files in destination)
sy /source /destination --delete

# Quiet mode (only show errors)
sy /source /destination --quiet

# Verbose logging
sy /source /destination -v      # Debug level
sy /source /destination -vv     # Trace level

# Parallel transfers (10 workers by default)
sy /source /destination -j 20   # Use 20 parallel workers

# Single file sync
sy /path/to/file.txt /dest/file.txt

# File size filtering (new in v0.0.9+)
sy /source /destination --min-size 1KB      # Skip files < 1KB
sy /source /destination --max-size 100MB    # Skip files > 100MB
sy /source /destination --min-size 1MB --max-size 50MB  # Only 1-50MB files

# Rsync-style filters (new in v0.0.18+)
sy /source /destination --filter="+ *.txt" --filter="- *"       # Include only .txt files
sy /source /destination --filter="- dir1/" --filter="+ *"       # Exclude dir1 and its contents
sy /source /destination --filter="+ */" --filter="+ *.rs" --filter="- *"  # Only .rs files in all directories

# Include/Exclude patterns (new in v0.0.9+)
sy /source /destination --exclude "*.log"                       # Skip log files
sy /source /destination --exclude "node_modules"                # Skip node_modules
sy /source /destination --include "*.txt" --exclude "*"         # Include only .txt files
sy /source /destination --exclude "*.tmp" --exclude "*.cache"   # Multiple patterns

# Bandwidth limiting (new in v0.0.9+)
sy /source /destination --bwlimit 1MB                  # Limit to 1 MB/s
sy /source user@host:/dest --bwlimit 500KB             # Limit remote sync to 500 KB/s

# Watch mode (new in v0.0.12+)
sy /source /destination --watch                        # Continuous sync on file changes

# JSON output (new in v0.0.11+)
sy /source /destination --json                         # Machine-readable NDJSON output
sy /source /destination --json | jq                    # Pipe to jq for processing

# Config profiles (new in v0.0.11+)
sy --profile backup-home                               # Use saved profile
sy --list-profiles                                     # Show available profiles
sy --show-profile backup-home                          # Show profile details

# Resume support (new in v0.0.13+)
sy /large /destination                                 # Interrupt with Ctrl+C
sy /large /destination                                 # Re-run to resume from checkpoint

# Verification modes (new in v0.0.14+)
sy /source /destination --verify                       # BLAKE3 cryptographic verification
sy /source /destination --mode fast                    # Size + mtime only (fastest)
sy /source /destination --mode standard                # + xxHash3 checksums (default)
sy /source /destination --mode verify                  # + BLAKE3 end-to-end (cryptographic)
sy /source /destination --mode paranoid                # BLAKE3 + verify every block (slowest)

# Symlink handling (new in v0.0.15+)
sy /source /destination --links preserve               # Preserve symlinks as symlinks (default)
sy /source /destination -L                             # Follow symlinks and copy targets
sy /source /destination --links skip                   # Skip all symlinks

# Hardlink preservation (new in v0.0.17+)
sy /source /destination -H                             # Preserve hard links
sy /source /destination --preserve-hardlinks           # Same as -H

# ACL preservation (new in v0.0.17+)
sy /source /destination -A                             # Preserve ACLs (Unix/Linux/macOS)
sy /source /destination --preserve-acls                # Same as -A

# BSD file flags preservation (new in v0.0.41+, macOS only)
sy /source /destination -F                             # Preserve BSD file flags (macOS hidden, immutable, etc.)
sy /source /destination --preserve-flags               # Same as -F

# Archive mode (new in v0.0.18+) - equivalent to -rlptgoD
sy /source /destination -a                             # Archive mode: recursive, links, perms, times, group, owner, devices
sy /source /destination --archive                      # Same as -a (rsync compatibility)
sy /source /destination -a -X -A -H -F                 # Full-fidelity backup (archive + xattrs + ACLs + hardlinks + flags)

# Individual metadata flags (new in v0.0.18+)
sy /source /destination -p                             # Preserve permissions only
sy /source /destination -t                             # Preserve modification times only
sy /source /destination -g                             # Preserve group (requires permissions)
sy /source /destination -o                             # Preserve owner (requires root)
sy /source /destination -D                             # Preserve device files (requires root)
sy /source /destination -ptg                           # Combine flags (perms + times + group)

# File comparison modes (new in v0.0.18+, enhanced in v0.0.35)
sy /source /destination --ignore-times                 # Always compare checksums (ignore mtime)
sy /source /destination --size-only                    # Only compare file size (skip mtime checks)
sy /source /destination -c                             # Pre-transfer checksums: skip if content identical
sy /source /destination --checksum                     # Same as -c (rsync compatibility)

# Pre-transfer checksum benefits (v0.0.35+):
# - Skip transfers when content unchanged (even if mtime changed)
# - Detect bit rot (content changed but mtime unchanged)
# - Uses xxHash3 (15 GB/s) for fast comparison
# - Saves bandwidth on re-syncs of touched but unmodified files

# Checksum database for faster re-syncs (new in v0.0.35+)
sy /source /destination --checksum --checksum-db=true  # First sync: stores checksums in database
sy /source /destination --checksum --checksum-db=true  # Second sync: 10-100x faster (cache hits!)
sy /source /destination --checksum --checksum-db=true --clear-checksum-db  # Clear cache and start fresh
sy /source /destination --checksum --checksum-db=true --prune-checksum-db  # Remove stale entries
# Database: .sy-checksums.db in destination, ~200 bytes per file

# Verify-only mode - audit without modifying (new in v0.0.36+)
sy /source /destination --verify-only                   # Compare checksums, report mismatches
sy /source /destination --verify-only --json            # JSON output for scripting
# Exit codes: 0 = all match, 1 = mismatches/differences, 2 = errors
# Reports:
#   - Files that match (checksum comparison)
#   - Files that mismatch (content differs)
#   - Files only in source (missing from dest)
#   - Files only in destination (extra files)
# Use cases: Verify backup integrity, detect corruption, audit sync results

# Deletion safety (new in v0.0.18+)
sy /source /destination --delete --delete-threshold 75  # Allow up to 75% of files to be deleted
sy /source /destination --delete --force-delete         # Skip safety checks (dangerous!)
# Note: Default threshold is 50%, prompts for confirmation if >1000 files

# Hooks (new in Phase 9)
sy /source /destination                                 # Automatically runs hooks from ~/.config/sy/hooks/
sy /source /destination --no-hooks                      # Disable hook execution
sy /source /destination --abort-on-hook-failure         # Abort sync if hooks fail (default: warn)
# Hooks: pre-sync.sh runs before sync, post-sync.sh runs after with stats

# Ignore templates (new in Phase 9)
sy /rust-project /backup --ignore-template rust         # Use Rust template (target/, Cargo.lock)
sy /node-app /backup --ignore-template node             # Use Node template (node_modules/, dist/)
echo "build/" > /project/.syignore                      # Project-specific .syignore (auto-loaded)
sy /project /backup --ignore-template rust --ignore-template node  # Combine multiple templates
# Templates: ~/.config/sy/templates/{name}.syignore, see templates/ directory for examples

# S3/Cloud Storage (new in Phase 10)
sy /local/path s3://my-bucket/backups/                  # Upload to S3
sy s3://my-bucket/backups/ /local/restore/              # Download from S3
sy /project s3://my-bucket/project?region=us-west-2     # Specify region
sy /data s3://my-bucket/data?endpoint=https://r2.example.com  # Custom endpoint (R2, B2, etc.)

# S3 Authentication: Uses AWS credentials from:
# - Environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)
# - ~/.aws/credentials profile
# - IAM role (when running on AWS)

# SSH sparse file transfer (new in v0.0.42+)
sy /vm/images/disk.vmdk user@host:/backup/     # Auto-detects sparse files
# 10GB VM image with 1GB data: transfers 1GB instead of 10GB (10x bandwidth savings)
# 100GB database with 20GB data: transfers 20GB instead of 100GB (5x bandwidth savings)
# Automatic detection on Unix (allocated_size < file_size)
# Graceful fallback if sparse detection fails or not supported

# Incremental scanning with cache (new in Phase 11 / v0.0.22)
sy /large-project /backup --use-cache                   # Enable directory cache for faster re-syncs
sy /large-project /backup --use-cache                   # 2nd run: 1.67-1.84x faster (uses cache)
sy /large-project /backup --clear-cache                 # Clear cache and re-scan everything
# Cache file: .sy-dir-cache.json stores directory mtimes + file metadata
# Expected speedup: 10-100x on large datasets (>10k files)
```

## Features

### ✅ What Works Now (v0.0.22)

**Local Sync (Phase 1 - Complete)**:
- **Smart File Sync**: Compares size + modification time (1s tolerance)
- **Git-Aware**: Automatically respects `.gitignore` patterns
- **Safe by Default**: Preview changes with `--dry-run`
- **Progress Display**: Beautiful progress bars with indicatif
- **Flexible Logging**: From quiet to trace level
- **Edge Cases**: Handles unicode, deep nesting, large files, empty dirs
- **Single File Sync**: Sync individual files, not just directories
- **File Size Filtering**: `--min-size` and `--max-size` with human-readable units (KB, MB, GB, TB)
- **Exclude Patterns**: `--exclude` flag with glob patterns (e.g., `*.log`, `node_modules`)
- **Bandwidth Limiting**: `--bwlimit` flag to control transfer rate (e.g., `1MB`, `500KB`)

**Delta Sync (Phase 2 - Complete)**:
- **Rsync Algorithm**: TRUE O(1) rolling hash (2ns per operation, verified constant time)
- **Adler-32 + xxHash3**: Fast weak hash + strong checksum
- **Block-Level Updates**: Only transfers changed blocks, not entire files
- **Adaptive Block Size**: Automatically calculates optimal block size (√filesize)
- **Streaming Implementation**: Constant ~256KB memory for files of any size
- **Remote Operations**: Enabled for all SSH/SFTP transfers
- **Local Operations**: Enabled for large files (>1GB threshold)
- **Smart Heuristics**: Automatic activation based on file size and transport type
- **Progress Visibility**: Shows compression ratio in real-time (e.g., "delta: 2.4% literal")

**Parallel Execution (Phase 3 - Complete)**:
- **Parallel File Transfers**: 5-10x faster for multiple files
- **Parallel Checksums**: 2-4x faster block checksumming (v0.0.10)
- **Configurable Workers**: Default 10, adjustable via `-j` flag
- **Thread-Safe Stats**: Accurate progress tracking with Arc<Mutex<>>
- **Semaphore Control**: Prevents resource exhaustion
- **Error Handling**: Collects all errors, reports first failure

**UX & Polish (v0.0.10+)**:
- **Color-Coded Output**: Green (created), yellow (updated), cyan (transfer stats), magenta (delta sync)
- **Performance Metrics**: Duration and transfer rate displayed in summary
- **Enhanced Dry-Run** (Phase 9):
  - Clear "Would create/update/delete" messaging
  - `--diff` flag shows detailed file sizes for changed files
  - **Byte statistics summary**: Shows bytes to add/change/delete
  - Example: `sy /src /dst --dry-run --diff` shows detailed impact preview
- **Better Errors**: Actionable suggestions (e.g., "check disk space", "verify permissions")
- **CLI Examples**: Built-in help with common usage patterns
- **Delta Sync Visibility**: Real-time compression ratio and bandwidth savings
- **Compression Stats**: Files compressed and bytes saved displayed in summary
- **File Size Filtering**: `--min-size` and `--max-size` flags with human-readable units
- **Exclude Patterns**: `--exclude` flag for flexible glob-based filtering
- **Bandwidth Limiting**: `--bwlimit` flag for controlled transfer rates

**Compression (Phase 3.5 - Complete)**:
- **Performance** (benchmarked):
  - LZ4: 23 GB/s throughput
  - Zstd: 8 GB/s throughput (level 3)
- **Smart Detection** (v0.0.37 - NEW!):
  - **Content Sampling**: Tests first 64KB with LZ4 (~3μs overhead)
  - **10% Threshold**: Only compress if >10% savings (ratio <0.9)
  - **Auto-Detection**: Catches compressed files without extensions (minified JS, executables, etc.)
  - **CLI Control**: `--compression-detection` (auto|extension|always|never)
  - **BorgBackup-inspired**: Proven approach from production backup tool
- **Smart Heuristics**:
  - Local: never compress (disk I/O bottleneck)
  - Network: content-based detection (auto mode)
  - Skip: files <1MB, pre-compressed formats (jpg, mp4, zip, pdf, etc.)
  - Skip: incompressible data detected via sampling
- **Status**:
  - ✅ Module implemented and tested (28 unit tests, +12 new)
  - ✅ Integration tests pass (5 tests, proven end-to-end)
  - ✅ Benchmarks prove 50x faster than originally assumed
  - ✅ Production integration complete (v0.0.10)
  - ✅ Content-based auto-detection (v0.0.37)
  - ✅ Compression stats tracked and displayed
  - ✅ 2-5x reduction on text/code files

**Advanced Features (Phase 4 - Complete)**:
- **JSON Output** (v0.0.11):
  - Machine-readable NDJSON format for scripting
  - Events: start, create, update, skip, delete, summary
  - Auto-suppresses logging in JSON mode
  - Example: `sy /src /dst --json | jq`
- **Config Profiles** (v0.0.11):
  - Save common sync configurations
  - Config file: `~/.config/sy/config.toml`
  - Commands: `--profile`, `--list-profiles`, `--show-profile`
  - CLI args override profile settings
- **Watch Mode** (v0.0.12):
  - Continuous file monitoring for real-time sync
  - 500ms debouncing to avoid excessive syncing
  - Graceful Ctrl+C shutdown
  - Cross-platform (Linux, macOS, Windows)
- **Resume Support** (v0.0.13):
  - Automatic recovery from interrupted syncs
  - State file: `.sy-state.json` in destination
  - Flag compatibility checking
  - Skips already-completed files on resume

**Developer Experience (Phase 9 - In Progress)**:
- **Hooks** (Phase 9):
  - Pre-sync and post-sync hook execution
  - Auto-discovered from `~/.config/sy/hooks/`
  - Environment variables for sync context (SY_SOURCE, SY_DESTINATION, SY_FILES_*, etc.)
  - Cross-platform support (Unix: .sh/.bash/.zsh/.fish, Windows: .bat/.cmd/.ps1/.exe)
  - Configurable failure handling: `--abort-on-hook-failure` or warn and continue (default)
  - Example use cases: Notifications, backups, Slack alerts, custom validation
  - Fully tested (4 unit tests)
- **Ignore Templates** (Phase 9):
  - `.syignore` files: sy-specific ignore patterns (like `.gitignore`)
  - Global templates: `~/.config/sy/templates/{name}.syignore`
  - CLI flag: `--ignore-template <name>` (repeatable)
  - Auto-discovery: `.syignore` loaded automatically from source directory
  - Priority order: CLI flags > .syignore > templates > .gitignore
  - Built-in templates: rust, node, python (see `templates/` directory)
  - Example: `sy /project /backup --ignore-template rust`
- **Improved Dry-Run** (Phase 9):
  - `--diff` flag shows detailed file sizes for changed files
  - Byte statistics summary displays total bytes to add/change/delete
  - Clear color-coded output (yellow for changes, red for deletions)
  - Example: `sy /src /dst --dry-run --diff --delete`
  - Output includes: file counts + byte impact analysis

**Performance Monitoring (v0.0.33)**:
- **Detailed Metrics** with `--perf` flag:
  - Total time broken down by phase (scanning, planning, transferring)
  - Files processed (created, updated, deleted)
  - Data transferred and read
  - Average transfer speed and file processing rate
  - Bandwidth utilization (if rate limit set)
- **Thread-Safe Collection**:
  - Arc<Mutex<PerformanceMonitor>> with AtomicU64 counters
  - Real-time tracking during parallel execution
  - Zero overhead when not enabled
- **Example Output**:
  ```
  Performance Summary:
    Total time:      0.00s
      Scanning:      0.00s (26.6%)
      Planning:      0.00s (5.1%)
      Transferring:  0.00s (64.4%)
    Files:           20 processed
      Created:       20
    Data:            1.95 MB transferred, 1.95 MB read
    Speed:           858.75 MB/s avg
    Rate:            5 files/sec
  ```
- **Use Cases**:
  - Performance tuning and optimization
  - Identifying bottlenecks (scan vs transfer)
  - Benchmarking different configurations
  - Monitoring large-scale syncs

**Error Reporting (v0.0.34)**:
- **Comprehensive Error Collection**:
  - All errors collected during parallel execution
  - Sync continues for successful files (up to max_errors threshold)
  - Users see ALL problems at once, not just first failure
- **Detailed Error Context**:
  - File path where error occurred
  - Action that failed (create/update/delete)
  - Full error message with actionable details
- **Beautiful Formatting**:
  - Color-coded output (red header, yellow action tags)
  - Clear numbering for multiple errors
  - Total error count summary
- **Example Output**:
  ```
  ⚠️  Errors occurred during sync:

  1. [update] /path/to/file.txt
     Permission denied (os error 13)

  2. [create] /path/to/other.txt
     Disk quota exceeded

  Total errors: 2
  ```
- **Benefits**:
  - Fix all problems in one go instead of iteratively
  - Clear identification of problematic files
  - Better debugging and troubleshooting
  - Detailed context for every failure

**Pre-Transfer Checksums (v0.0.35)**:
- **Smart Content Comparison** with `--checksum` / `-c` flag:
  - Computes checksums **before** transfer to detect identical files
  - Uses xxHash3 (Fast mode) for comparison - **15 GB/s throughput**
  - Skips transfer if checksums match, even if mtime differs
  - Transfers only if checksums differ
- **Key Benefits**:
  - **Bandwidth Savings**: Skip files where only mtime changed (touched but not modified)
  - **Bit Rot Detection**: Detect corruption when content changed but mtime unchanged
  - **Fast Re-Syncs**: Ideal for scenarios where files are frequently touched
  - **Minimal Overhead**: xxHash3 adds ~5% overhead on SSDs
- **Usage**:
  ```bash
  # Compare checksums before transfer
  sy /source /destination --checksum

  # Short form
  sy /source /destination -c

  # Use with dry-run to see what would be skipped
  sy /source /destination -c --dry-run --diff
  ```
- **Current Scope**:
  - ✅ Local→Local sync (fully working)
  - 📋 Remote support (planned for follow-up)
- **Implementation**:
  - Checksums computed during planning phase
  - Stored in SyncTask for potential future use (checksum database)
  - Zero overhead when flag not enabled

**Checksum Database (v0.0.35)**:
- **Persistent Checksum Cache** with `--checksum-db` flag:
  - Stores file checksums in SQLite database (`.sy-checksums.db` in destination)
  - Automatically reuses cached checksums for unchanged files (mtime + size validation)
  - Skips expensive I/O operations on subsequent syncs
  - **10-100x speedup** for re-syncs with `--checksum` flag
- **Key Benefits**:
  - **Instant Verification**: Database lookups (<1ms) vs. file I/O (50-200ms per file)
  - **Massive Speedup**: Re-syncs complete in milliseconds instead of seconds/minutes
  - **Automatic Cache Invalidation**: mtime or size change triggers recomputation
  - **Safe by Default**: Never trusts stale checksums
- **Usage**:
  ```bash
  # Enable checksum database (must use with --checksum)
  sy /source /destination --checksum --checksum-db=true

  # First sync: Computes and stores checksums (normal speed)
  # Second sync: Instant checksum retrieval from database (10-100x faster!)

  # Clear database before sync (fresh start)
  sy /source /destination --checksum --checksum-db=true --clear-checksum-db

  # Remove stale entries (files deleted from source)
  sy /source /destination --checksum --checksum-db=true --prune-checksum-db
  ```
- **Database Details**:
  - Location: `.sy-checksums.db` in destination directory
  - Format: SQLite with indexed queries for fast lookups
  - Schema: path, mtime, size, checksum_type, checksum, updated_at
  - Cache hits logged in debug mode: `RUST_LOG=sy=debug sy ...`
- **Performance Example**:
  ```bash
  # First sync: 500ms (computes all checksums)
  sy /source /dest --checksum --checksum-db=true

  # Second sync: 5ms (cache hits, 100x faster!)
  sy /source /dest --checksum --checksum-db=true
  ```
- **Maintenance**:
  - `--clear-checksum-db`: Clear all cached checksums
  - `--prune-checksum-db`: Remove entries for deleted files
  - Database grows with file count (~200 bytes per file)
  - Safe to delete `.sy-checksums.db` manually (will be recreated)

**Verify-Only Mode (v0.0.36)**:
- **Audit Without Modification** with `--verify-only` flag:
  - Compares source and destination by computing checksums
  - Reports matched files, mismatches, and differences
  - **Read-only**: Never modifies any files
  - **Scriptable**: JSON output with clear exit codes
- **Key Benefits**:
  - **Backup Verification**: Confirm backups match source data
  - **Corruption Detection**: Identify files that changed unexpectedly
  - **Audit Results**: Verify sync completed successfully
  - **Integration**: Exit codes enable automation and monitoring
- **Usage**:
  ```bash
  # Basic verification (human-readable output)
  sy /source /destination --verify-only

  # JSON output for scripting
  sy /source /destination --verify-only --json

  # Use in scripts with exit code checking
  if sy /backup /original --verify-only --json; then
    echo "Backup verified successfully"
  else
    echo "Backup verification failed"
  fi
  ```
- **Exit Codes**:
  - `0`: All files match (perfect integrity)
  - `1`: Mismatches or differences found
  - `2`: Errors occurred during verification
- **Output Details**:
  - **Files matched**: Count of files with identical checksums
  - **Files mismatched**: List of files with different content
  - **Files only in source**: Missing from destination
  - **Files only in destination**: Extra files not in source
  - **Errors**: Any files that couldn't be verified
  - **Duration**: Total verification time
- **JSON Format**:
  ```json
  {
    "type": "verification_result",
    "files_matched": 100,
    "files_mismatched": ["file1.txt"],
    "files_only_in_source": ["new.txt"],
    "files_only_in_dest": ["old.txt"],
    "errors": [],
    "duration_secs": 0.532,
    "exit_code": 1
  }
  ```
- **Use Cases**:
  - Verify backup integrity after sync
  - Detect bit rot or corruption over time
  - Audit cloud storage vs. local files
  - Monitor file synchronization systems
  - Integration testing for sync workflows

**Verification & Reliability (Phase 5 - Complete)**:
- **Verification Modes** (v0.0.14):
  - **Fast**: Size + mtime only (trust filesystem)
  - **Standard** (default): + xxHash3 checksums
  - **Verify**: + BLAKE3 cryptographic end-to-end verification
  - **Paranoid**: BLAKE3 + verify every block written
  - Flags: `--mode <mode>` or `--verify` (shortcut for verify mode)
  - Shows verification stats in summary output
  - JSON output includes verification counts and failures
- **BLAKE3 Integration**:
  - 32-byte cryptographic hashes for data integrity
  - Verifies source and destination match after transfer
  - Fast parallel hashing (multi-threaded by default)
  - Detects silent corruption that TCP checksums miss
- **Symlink Support** (v0.0.15):
  - **Preserve** (default): Copy symlinks as symlinks
  - **Follow** (`-L`): Copy the symlink target file
  - **Skip**: Ignore all symlinks
  - Detects broken symlinks and logs warnings
  - Cross-platform (Unix/Linux/macOS)
- **Sparse File Support** (v0.0.15):
  - Automatic detection of sparse files (files with "holes")
  - Preserves sparseness during transfer (Unix/Linux/macOS)
  - Efficient transfer - only allocated blocks are copied
  - Critical for VM disk images, database files, etc.
  - Zero configuration - works transparently
- **Extended Attributes Support** (v0.0.16):
  - `-X` flag to preserve extended attributes (xattrs)
  - Preserves metadata like macOS Finder info, security contexts
  - Always scanned, conditionally preserved (minimal overhead)
  - Full-fidelity backups when combined with other features
  - Cross-platform (Unix/Linux/macOS)
- **Hardlink Preservation** (v0.0.17):
  - `-H` flag to preserve hard links between files
  - Tracks inode numbers during scan
  - Creates hardlinks instead of copying duplicate data
  - Preserves disk space savings from source to destination
  - **Full parallel support**: Async coordination ensures correct hardlink creation with multiple workers
  - Critical for backup systems, package managers, etc.
  - Cross-platform (Unix/Linux/macOS)
- **ACL Preservation** (v0.0.17):
  - `-A` flag to preserve POSIX Access Control Lists
  - Always scanned, conditionally preserved (minimal overhead)
  - Preserves fine-grained permissions beyond owner/group/other
  - Parses and applies ACLs using standard text format
  - Essential for enterprise systems with complex permission models
  - Cross-platform (Unix/Linux/macOS)
  - Fully implemented and tested
- **BSD File Flags** (v0.0.41, macOS only):
  - `-F` flag to preserve BSD file flags (hidden, immutable, nodump, etc.)
  - Explicitly sets or clears flags to prevent auto-preservation
  - Preserves macOS-specific file attributes like Finder hidden flag
  - Uses `chflags()` syscall for accurate flag management
  - Essential for maintaining macOS file metadata in backups
  - Includes comprehensive tests for preservation and clearing behaviors
  - Fully implemented and tested on macOS
- **Rsync-Style Filters** (v0.0.18):
  - `--filter` flag for ordered include/exclude rules (first match wins)
  - `--include` and `--exclude` flags for simple patterns
  - Directory-only patterns with trailing slash (e.g., `build/`)
  - Wildcard directory patterns (e.g., `*/` to include all directories)
  - Basename matching (no slash) vs. full path matching (with slash)
  - Compatible with rsync filter semantics
  - Examples:
    - `--filter="+ *.txt" --filter="- *"` - Only sync .txt files
    - `--filter="+ */" --filter="+ *.rs" --filter="- *"` - Only .rs files in all dirs
    - `--filter="- build/" --filter="+ *"` - Exclude build directory and contents
- **Cross-Transport Sync** (v0.0.18-v0.0.21):
  - Remote → Local sync fully working (e.g., `user@host:/src /local/dst`)
  - Local → Remote already supported
  - Proper mtime preservation across transports
  - SFTP-based file reading for remote sources
  - **Cross-transport delta sync** (v0.0.19-v0.0.21):
    - Automatic remote file update detection
    - Delta sync triggers automatically for remote file updates
    - **98% bandwidth savings** demonstrated (50MB file, 1MB changed → only ~1MB transferred)
    - xxHash3 fast checksums (10x faster than BLAKE3 for non-cryptographic verification)
    - FileInfo abstraction enables transport-agnostic metadata operations

**S3/Cloud Storage (Phase 10 - Complete)**:
- **Multi-Cloud Support**:
  - AWS S3 (native support)
  - Cloudflare R2 (via custom endpoint)
  - Backblaze B2 (via custom endpoint)
  - Wasabi (via custom endpoint)
  - Any S3-compatible service
- **Path Format**: `s3://bucket/key/path?region=us-west-2&endpoint=https://...`
- **Authentication**: Automatic via AWS SDK
  - Environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)
  - ~/.aws/credentials and ~/.aws/config
  - IAM roles (when running on AWS)
  - SSO profiles
- **Features**:
  - Automatic multipart upload for large files (>100MB)
  - 5MB part size (S3 minimum requirement)
  - Force path-style addressing for non-AWS services
  - Full Transport trait implementation
  - Bidirectional sync (upload and download)
- **Example Usage**:
  - `sy /local s3://my-bucket/backups/` - Upload to S3
  - `sy s3://my-bucket/data/ /local/restore/` - Download from S3
  - `sy /data s3://my-bucket/data?region=us-west-2` - Specify region
  - `sy /data s3://my-bucket/data?endpoint=https://...` - Custom endpoint

**SSH Optimizations (v0.0.42)**:
- **Connection Pooling** (NEW):
  - True parallel SSH transfers with N connections for N workers
  - Round-robin session distribution via atomic counter
  - Pool size automatically matches `--parallel` worker count
  - Avoids ControlMaster bottleneck (which serializes on one TCP connection)
  - Each worker gets dedicated SSH connection for maximum throughput
- **Sparse File Transfer** (NEW):
  - Automatic detection of sparse files (VM images, databases, etc.)
  - Detects data regions using SEEK_HOLE/SEEK_DATA on Unix
  - Transfers only actual data, not holes (empty regions)
  - **10x bandwidth savings** for VM images (e.g., 10GB file with 1GB data → 1GB transferred)
  - **5x bandwidth savings** for database files
  - Protocol: detect regions → send JSON + stream data → reconstruct on remote
  - Graceful fallback to regular transfer if detection fails
  - Auto-detection: `allocated_size < file_size` on Unix systems
  - Zero configuration - works automatically for sparse files
- **Example Usage**:
  ```bash
  # Connection pooling (automatic with -j flag)
  sy /source user@host:/dest -j 20    # 20 workers = 20 SSH connections

  # Sparse file transfer (automatic detection)
  sy /vm/disk.vmdk user@host:/backup/ # Only transfers data regions
  sy /db/postgres.db user@host:/sync/ # Skips holes, transfers data
  ```

**Bidirectional Sync (v0.0.43+)**:
- **Two-Way Synchronization**:
  - Sync changes in both directions automatically
  - Text-based state tracking in `~/.cache/sy/bisync/` (v0.0.44+)
  - Detects new files, modifications, and deletions on both sides
  - Handles 9 change types including conflicts
  - **SSH Support** (v0.0.46+): Works with remote servers (local↔remote, remote↔remote)
- **Conflict Resolution Strategies** (6 options):
  - `newer` (default): Most recent modification time wins
  - `larger`: Largest file size wins
  - `smaller`: Smallest file size wins
  - `source`: Source always wins (force push)
  - `dest`: Destination always wins (force pull)
  - `rename`: Keep both files with `.conflict-{timestamp}-{side}` suffix
  - Automatic tie-breaker: falls back to rename when attributes equal
- **Safety Features**:
  - Deletion limit: Default 50% threshold prevents mass deletion
  - Dry-run support: Preview changes before syncing
  - Content equality checks: Reduces false conflict detection
  - State persistence: Survives interruptions and errors
- **Example Usage**:
  ```bash
  # Basic bidirectional sync (newest-wins)
  sy --bidirectional /laptop/docs /backup/docs
  sy -b /local /remote  # Short form

  # Explicit conflict resolution strategy
  sy -b /a /b --conflict-resolve newer   # Most recent wins (default)
  sy -b /a /b --conflict-resolve larger  # Largest file wins
  sy -b /a /b --conflict-resolve rename  # Keep both files

  # Force one direction in conflicts
  sy -b /source /dest --conflict-resolve source  # Source always wins
  sy -b /source /dest --conflict-resolve dest    # Dest always wins

  # Safety limits
  sy -b /a /b --max-delete 10   # Abort if >10% deletions
  sy -b /a /b --max-delete 0    # No limit (dangerous!)

  # Dry-run to preview changes
  sy -b /a /b --dry-run

  # Clear state and resync fresh
  sy -b /a /b --clear-bisync-state

  # SSH bidirectional sync (v0.0.46+)
  sy -b /local/docs user@host:/remote/docs       # Local ↔ Remote
  sy -b user@host1:/data user@host2:/backup      # Remote ↔ Remote
  sy -b /laptop/work user@server:/work -p 8      # With parallel transfers
  ```
- **Current Status**: Supports local↔local, local↔remote, and remote↔remote (v0.0.46+)

**Scale (Phase 11 - Complete)**:
- **Incremental Scanning with Cache** (NEW in v0.0.22):
  - Cache directory mtimes to detect unchanged directories
  - Store file metadata (path, size, mtime, is_dir) in JSON cache
  - Skip rescanning unchanged directories (use cached file list)
  - **Performance**: 1.67-1.84x speedup measured (10-100x expected on large datasets)
  - Cache file: `.sy-dir-cache.json` in destination (JSON format, version 2)
  - CLI flags: `--use-cache`, `--clear-cache`
  - Automatic cache invalidation on directory mtime change
  - 1-second mtime tolerance for filesystem granularity
- **Streaming Scanner**:
  - O(1) memory usage regardless of directory size
  - Iterator-based file processing (no loading all files into RAM)
  - Legacy `scan()` API preserved for compatibility
  - New `scan_streaming()` API for memory-efficient large-scale syncs
  - **Memory Savings**: 1M files: 150MB → O(1) constant memory
- **Parallel Directory Scanning**:
  - Automatic CPU core detection (uses all available cores)
  - 2-4x faster scanning on directories with many subdirectories
  - Configurable thread count via `Scanner::with_threads()`
  - Zero overhead for small directories
- **Bloom Filter Deletion**:
  - Space-efficient existence checks (1.2 bytes per file vs 100+ bytes for HashSet)
  - 1% false positive rate for optimal memory usage
  - 1M files: 1.2MB Bloom filter vs 100MB HashSet
  - **100x memory reduction** for deletion checks
  - Automatic threshold: >10k files uses Bloom filter, <10k uses HashMap
  - Streams destination files (no loading all into memory)
  - Zero false negatives (safe deletions guaranteed)
- **Batch Processing**:
  - Process files in configurable batches (default 10,000)
  - Balances memory usage and performance
  - Prevents memory exhaustion on multi-million file syncs
- **Performance at Scale**:
  - Tested with 100k+ files (stress tests)
  - Designed for millions of files without memory spikes
  - Streaming approach ensures consistent memory usage
  - **Real-world impact**: 1M file sync: ~150MB RAM → ~5MB RAM
  - **Incremental re-syncs**: 1.67-1.84x faster with cache (10-100x on large datasets)

### 📋 Common Use Cases

```bash
# Backup your project (uses delta sync for updates)
sy ~/my-project ~/backups/my-project

# Sync to external drive
sy ~/Documents /Volumes/Backup/Documents --delete

# Preview what would change
sy ~/src ~/dest --dry-run

# Sync with detailed logging (see delta sync in action)
RUST_LOG=info sy ~/src ~/dest

# Delta sync automatically activates for file updates
# Example output: "Delta sync: 3242 ops, 0.1% literal data"
# This means only 0.1% of the file was transferred!
```

## Vision

**The Problem**: rsync is single-threaded, has confusing flags, and doesn't verify integrity end-to-end. Modern tools like rclone are faster but complex. We can do better.

**The Goal**: A file sync tool that:
- ✅ Auto-detects network conditions and optimizes accordingly
- ✅ Verifies integrity with multi-layer checksums
- ✅ Has beautiful progress display and helpful errors
- ✅ Works great out of the box with smart defaults
- ✅ Scales from a few files to millions

## Key Features (Planned)

### Adaptive Performance
```bash
# Auto-detects: Local? LAN? WAN? Optimizes for each
sy ~/src /backup                    # Local: max parallelism, no compression
sy ~/src server:/backup             # LAN: parallel + minimal delta
sy ~/src remote:/backup             # WAN: compression + delta + BBR
```

### Verifiable Integrity ✅ (Implemented in v0.0.14)
```bash
# Multiple verification modes
sy ~/src remote:/dst --mode standard   # Default: xxHash3 checksums
sy ~/src remote:/dst --mode verify     # Cryptographic: BLAKE3 end-to-end
sy ~/src remote:/dst --mode paranoid   # Maximum: BLAKE3 + verify every block
sy ~/src remote:/dst --mode fast       # Fastest: size + mtime only
```

### Beautiful UX
```
Syncing ~/src → remote:/dest

[████████████████████----] 75% | 15.2 GB/s | ETA 12s
  ├─ config.json ✓
  ├─ database.db ⣾ (chunk 45/128, 156 MB/s)
  └─ videos/large.mp4 ⏸ (queued)

Files: 1,234 total | 892 synced | 312 skipped | 30 queued
```

### Smart Defaults
- Auto-detects gitignore patterns in repositories
- Refuses to delete >50% of destination (safety check)
- Warns about file descriptor limits before hitting them
- Detects sparse files and transfers efficiently
- Handles cross-platform filename conflicts

## Comparison

| Feature | rsync | rclone | sy (v0.0.22) |
|---------|-------|--------|-----|
| **Performance (local)** | baseline | N/A | **2-11x faster** |
| Parallel file transfers | ❌ | ✅ | ✅ |
| Parallel checksums | ❌ | ❌ | ✅ |
| SSH connection pooling | ❌ | ❌ | ✅ **N workers = N connections** |
| SSH sparse file transfer | ❌ | ❌ | ✅ **Auto-detect, 10x savings** |
| Delta sync | ✅ | ❌ | ✅ |
| Cross-transport delta sync | ❌ | ❌ | ✅ **Auto-detects updates!** |
| Streaming delta | ❌ | ❌ | ✅ **Constant memory!** |
| True O(1) rolling hash | ❌ | ❌ | ✅ **2ns per operation!** |
| Block checksums | ✅ MD5 | ❌ | ✅ xxHash3 |
| Cryptographic verification | ✅ MD5 | ✅ | ✅ **BLAKE3** |
| Compression | ✅ | ✅ | ✅ **Zstd (8 GB/s)** |
| Bandwidth limiting | ✅ | ✅ | ✅ |
| File filtering | ✅ | ✅ | ✅ **Rsync-style** |
| Resume support | ❌ | ✅ | ✅ |
| Watch mode | ❌ | ✅ | ✅ |
| JSON output | ❌ | ✅ | ✅ |
| Hooks | ❌ | ❌ | ✅ |
| Incremental scanning cache | ❌ | ❌ | ✅ **1.67-100x faster re-syncs** |
| S3/Cloud storage | ❌ | ✅ | ✅ **AWS, R2, B2, Wasabi** |
| Modern UX | ❌ | ⚠️ | ✅ |
| Single file sync | ⚠️ Complex | ✅ | ✅ |
| Zero compiler warnings | N/A | N/A | ✅ |

**All major features implemented!** See [DESIGN.md](DESIGN.md) for comprehensive technical details.

## Example Usage

```bash
# Basic sync (local or remote)
sy /source /destination
sy /source user@host:/dest

# Preview changes (dry-run)
sy /source /destination --dry-run

# Mirror mode (delete files not in source)
sy /source /destination --delete

# Parallel transfers (10 workers by default)
sy /source /destination -j 20

# Performance monitoring (detailed metrics)
sy /source /destination --perf

# File filtering
sy /source /destination --min-size 1MB --max-size 100MB
sy /source /destination --exclude "*.log" --exclude "node_modules"

# Bandwidth limiting
sy /source user@host:/dest --bwlimit 1MB

# Combined: dry-run with performance metrics
sy /source /destination --dry-run --diff --perf
```

## Design Highlights

### Reliability: Multi-Layer Defense
- **Layer 1**: TCP checksums (99.99% detection)
- **Layer 2**: xxHash3 per-block (fast corruption detection)
- **Layer 3**: BLAKE3 end-to-end (cryptographic verification)
- **Layer 4**: Optional multiple passes + comparison reads

Research shows 5% of 100 Gbps transfers have corruption TCP doesn't detect. We verify at multiple layers.

### Performance: Adaptive Strategies
Different scenarios need different approaches:
- **Local**: Maximum parallelism, kernel optimizations (copy_file_range, clonefile)
- **LAN**: Parallel transfers, selective delta, minimal compression
- **WAN**: Delta sync, adaptive compression, BBR congestion control

### Scale: Millions of Files
- Stream processing (no loading entire tree into RAM)
- Bloom filters for efficient deletion
- State caching for incremental syncs
- Parallel directory traversal

See [DESIGN.md](DESIGN.md) for full technical details.

## Design Complete! ✅

The design phase is finished with comprehensive specifications for:

1. **Core Architecture** - Parallel sync, delta algorithm, integrity verification
2. **Edge Cases** - 8 major categories (symlinks, sparse files, cross-platform, etc.)
3. **Advanced Features** - Filters, bandwidth limiting, progress UI, SSH integration
4. **Error Handling** - Threshold-based with categorization and reporting
5. **Testing Strategy** - Unit, integration, property, and stress tests
6. **Implementation Roadmap** - 10 phases from MVP to v1.0

Total design document: **2,400+ lines** of detailed specifications, code examples, and rationale.

## Implementation Roadmap

### ✅ Phase 1: MVP (v0.1.0) - COMPLETE
- ✅ Basic local sync
- ✅ File comparison (size + mtime)
- ✅ Full file copy with platform optimizations
- ✅ Beautiful progress display
- ✅ .gitignore support
- ✅ Dry-run and delete modes
- ✅ Comprehensive test suite (49 tests: unit, integration, property-based, edge cases, performance)
- ✅ Performance optimizations (10% faster than initial implementation)
- ✅ Comparative benchmarks (vs rsync and cp)

### ✅ Phase 2: Network Sync + Delta (v0.0.3) - **COMPLETE**
- ✅ SSH transport (SFTP-based)
- ✅ SSH config integration
- ✅ **Delta sync implemented** (rsync algorithm)
- ✅ Adler-32 rolling hash + xxHash3 checksums
- ✅ Block-level updates for local and remote files
- ✅ Adaptive block size calculation

**Performance Win**: Delta sync dramatically reduces bandwidth usage by transferring only changed blocks instead of entire files.

### ✅ Phase 3: Parallelism + Optimization (v0.0.4-v0.0.10) - **COMPLETE**
- ✅ Parallel file transfers (5-10x speedup for multiple files)
- ✅ Parallel checksum computation (2-4x faster)
- ✅ Configurable worker count (default 10, via `-j` flag)
- ✅ Thread-safe statistics tracking
- ✅ TRUE O(1) rolling hash (fixed critical bug, verified 2ns constant time)
- ✅ Streaming delta generation (constant ~256KB memory)
- ✅ Size-based local delta heuristic (>1GB files)
- ✅ **Full compression integration** (Zstd level 3, 8 GB/s throughput)
- ✅ Compression stats tracking and display
- ✅ Single file sync support
- ✅ Zero clippy warnings (idiomatic Rust)

**Critical Bug Fixed (v0.0.5)**: Original "O(1)" rolling hash was actually O(n) due to `Vec::remove(0)`. Fixed by removing unnecessary window field. Verified true constant time: 2ns per operation across all block sizes.

**Memory Win (v0.0.6)**: Streaming delta generation uses constant ~256KB memory regardless of file size. 10GB file: 10GB RAM → 256KB (39,000x reduction).

See [docs/OPTIMIZATIONS.md](docs/OPTIMIZATIONS.md) for detailed optimization history.

### Phase 4: Advanced Features (v0.1.0+) - NEXT
- Network speed detection
- Parallel chunk transfers for very large files
- Resume support for interrupted transfers
- End-to-end cryptographic checksums (BLAKE3)

### Phase 5: Reliability (v0.5.0)
- Multi-layer checksums
- Verification modes
- Atomic operations
- Crash recovery

### Phases 6-10
- Edge cases & advanced features
- Extreme scale optimization
- UX polish
- Testing & documentation
- v1.0 release

## Testing

Phase 1 includes comprehensive testing at multiple levels:

```bash
# Run all tests
cargo test

# Run specific test suites
cargo test --lib                      # Unit tests only
cargo test --test integration_test    # Integration tests
cargo test --test property_test       # Property-based tests
cargo test --test edge_cases_test     # Edge case tests
cargo test --release --test performance_test  # Performance regression tests

# Run benchmarks
cargo bench

# Run with output
cargo test -- --nocapture
```

**Test Coverage (100+ tests):**
- **Unit Tests (83)**: Core functionality, CLI, compression, delta sync, SSH config
- **Integration Tests (36)**: End-to-end scenarios, compression, edge cases, performance
  - Compression integration (5 tests)
  - Edge cases (11 tests)
  - Full sync scenarios (13 tests)
  - Performance regression (7 tests)

**Code Quality:**
- ✅ Zero compiler warnings
- ✅ Zero linter warnings
- ✅ 100% of public API documented
- ✅ 5,500+ lines of code
- ✅ All performance tests passing

See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for performance testing and regression tracking.

## Performance

**sy is consistently 2-11x faster than rsync for local sync:**

| Scenario | sy | rsync | Speedup |
|----------|-----|-------|---------|
| **100 small files** | 25 ms | 40 ms | **1.6x faster** |
| **50MB file** | 21 ms | 185 ms | **8.8x faster** |
| **Idempotent sync** | 8 ms | 17 ms | **2.1x faster** |
| **500 files** | <10 ms | 110 ms | **11x faster** |

**Why so fast?**
- Modern platform optimizations (`copy_file_range`, `clonefile`)
- Parallel file transfers (10 workers by default)
- Parallel checksum computation
- Efficient scanning with pre-allocated vectors
- Smart size+mtime comparison (vs rsync's checksums)

See [docs/BENCHMARK_RESULTS.md](docs/BENCHMARK_RESULTS.md) for comprehensive benchmark analysis.

## Contributing

sy v0.0.22 is production-ready! Phases 1-11 are complete (only Phase 12 remaining for v1.0).

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

**Interested in contributing?** Areas we'd love help with:
- **Testing**: Cross-platform testing (Windows, Linux, macOS)
- **Performance**: Profiling and optimization for very large datasets
- **Features**: Advanced features (network auto-detection, parallel chunk transfers, incremental state caching)
- **Documentation**: Usage examples, tutorials, blog posts
- **Real-world testing**: Use sy in your workflows and report issues!

## License

MIT

## Acknowledgments

Inspired by:
- **rsync** - The algorithm that started it all
- **rclone** - Proof that parallel transfers work
- **eza**, **fd**, **ripgrep** - Beautiful UX in modern CLI tools
- **Syncthing** - Block-based integrity model

Research that informed the design:
- **Jeff Geerling** (2025) - rclone vs rsync benchmarks
- **ACM 2024** - "QUIC is not Quick Enough over Fast Internet"
- **ScienceDirect 2021** - File transfer corruption studies
- **Multiple papers** - rsync algorithm analysis, hash performance, compression strategies

---

**Questions?** See [DESIGN.md](DESIGN.md) for comprehensive technical details or [CONTRIBUTING.md](CONTRIBUTING.md) to get started.
