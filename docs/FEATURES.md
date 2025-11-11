# sy Features

Comprehensive feature documentation for sy file synchronization tool.

## Table of Contents

- [Core Features](#core-features)
- [Performance](#performance)
- [Verification & Integrity](#verification--integrity)
- [Advanced Sync](#advanced-sync)
- [Network & Remote](#network--remote)
- [Developer Experience](#developer-experience)
- [Platform Support](#platform-support)

## Core Features

### Local Sync (Phase 1)

**Smart File Sync:**
- Compares size + modification time (1s tolerance)
- Git-aware: automatically respects `.gitignore` patterns
- Safe by default: preview changes with `--dry-run`
- Beautiful progress bars with indicatif
- Flexible logging: from quiet to trace level

**Edge Cases Handled:**
- Unicode filenames (Chinese, Japanese, Russian, Arabic, emoji)
- Deep directory nesting (50+ levels tested)
- Large files (10MB+ with efficient streaming)
- Many small files (1000+ files in milliseconds)
- Empty directories (preserved in sync)
- Zero-byte files
- Binary files
- Hidden files (Unix convention with `.` prefix)

**Single File Sync:**
```bash
sy /path/to/file.txt /dest/file.txt
```

### Delta Sync (Phase 2)

**Rsync Algorithm Implementation:**
- TRUE O(1) rolling hash (2ns per operation, verified constant time)
- Adler-32 weak hash + xxHash3 strong checksum
- Block-level updates: only transfers changed blocks
- Adaptive block size: automatically calculates optimal size (√filesize)
- Streaming implementation: constant ~256KB memory for any file size

**Performance:**
- 10GB file: 10GB RAM → 256KB (39,000x memory reduction)
- 98% bandwidth savings for small changes (50MB file, 1MB changed → ~1MB transferred)
- Automatic activation based on file size and transport type

**Activation Heuristics:**
- Remote operations: enabled for all SSH/SFTP transfers
- Local operations: enabled for large files (>1GB threshold)
- Progress visibility: shows compression ratio in real-time (e.g., "delta: 2.4% literal")

### Parallel Execution (Phase 3)

**Parallel File Transfers:**
- 5-10x faster for multiple files
- Default 10 workers, adjustable via `-j` flag
- Semaphore control prevents resource exhaustion
- Thread-safe statistics tracking with Arc<Mutex<>>
- Error handling: collects all errors, reports first failure

**Parallel Checksums:**
- 2-4x faster block checksumming
- Utilizes all CPU cores
- Zero overhead for small files

```bash
sy /source /destination -j 20  # Use 20 parallel workers
```

### Compression (Phase 3.5)

**Performance (benchmarked):**
- LZ4: 23 GB/s throughput
- Zstd: 8 GB/s throughput (level 3)

**Smart Detection (v0.0.37):**
- **Content sampling**: tests first 64KB with LZ4 (~3μs overhead)
- **10% threshold**: only compress if >10% savings (ratio <0.9)
- **Auto-detection**: catches compressed files without extensions
- **CLI control**: `--compression-detection` (auto|extension|always|never)
- **BorgBackup-inspired**: proven approach from production backup tool

**Smart Heuristics:**
- Local: never compress (disk I/O bottleneck)
- Network: content-based detection (auto mode)
- Skip: files <1MB, pre-compressed formats (jpg, mp4, zip, pdf, etc.)
- Skip: incompressible data detected via sampling

**Status:**
- 28 unit tests, 5 integration tests
- Proven 50x faster than originally assumed
- 2-5x reduction on text/code files

## Performance

### Performance Monitoring (v0.0.33)

Detailed metrics with `--perf` flag:

```bash
sy /source /destination --perf
```

**Output:**
- Total time broken down by phase (scanning, planning, transferring)
- Files processed (created, updated, deleted)
- Data transferred and read
- Average transfer speed and file processing rate
- Bandwidth utilization (if rate limit set)

**Example:**
```
Performance Summary:
  Total time:      0.52s
    Scanning:      0.14s (26.6%)
    Planning:      0.03s (5.1%)
    Transferring:  0.34s (64.4%)
  Files:           1000 processed
    Created:       850
    Updated:       150
  Data:            1.95 GB transferred, 1.95 GB read
  Speed:           858.75 MB/s avg
  Rate:            1923 files/sec
```

### File Size Filtering

```bash
sy /source /destination --min-size 1KB      # Skip files < 1KB
sy /source /destination --max-size 100MB    # Skip files > 100MB
sy /source /destination --min-size 1MB --max-size 50MB  # Only 1-50MB files
```

Supports: KB, MB, GB, TB units

### Bandwidth Limiting

```bash
sy /source /destination --bwlimit 1MB                  # Limit to 1 MB/s
sy /source user@host:/dest --bwlimit 500KB             # Limit remote sync to 500 KB/s
```

## Verification & Integrity

### Verification Modes (v0.0.14)

Four verification levels:

1. **Fast**: Size + mtime only (trust filesystem)
   ```bash
   sy /source /destination --mode fast
   ```

2. **Standard** (default): + xxHash3 checksums
   ```bash
   sy /source /destination --mode standard
   ```

3. **Verify**: + BLAKE3 cryptographic end-to-end verification
   ```bash
   sy /source /destination --mode verify
   # Or shortcut:
   sy /source /destination --verify
   ```

4. **Paranoid**: BLAKE3 + verify every block written
   ```bash
   sy /source /destination --mode paranoid
   ```

**BLAKE3 Integration:**
- 32-byte cryptographic hashes for data integrity
- Verifies source and destination match after transfer
- Fast parallel hashing (multi-threaded by default)
- Detects silent corruption that TCP checksums miss

### Pre-Transfer Checksums (v0.0.35)

Smart content comparison with `--checksum` / `-c` flag:

```bash
sy /source /destination --checksum
sy /source /destination -c  # Short form
```

**Benefits:**
- Computes checksums **before** transfer to detect identical files
- Uses xxHash3 (15 GB/s throughput) for fast comparison
- Skips transfer if checksums match, even if mtime differs
- Bandwidth savings: skip files where only mtime changed
- Bit rot detection: detect corruption when content changed but mtime unchanged
- Minimal overhead: ~5% on SSDs

### Checksum Database (v0.0.35)

Persistent checksum cache for 10-100x faster re-syncs:

```bash
# Enable checksum database (must use with --checksum)
sy /source /destination --checksum --checksum-db=true

# First sync: Computes and stores checksums (normal speed)
# Second sync: Instant checksum retrieval (10-100x faster!)

# Clear database before sync
sy /source /destination --checksum --checksum-db=true --clear-checksum-db

# Remove stale entries
sy /source /destination --checksum --checksum-db=true --prune-checksum-db
```

**Database Details:**
- Location: `.sy-checksums.db` in destination directory
- Format: SQLite with indexed queries
- Schema: path, mtime, size, checksum_type, checksum, updated_at
- Storage: ~200 bytes per file
- Cache invalidation: automatic on mtime or size change

### Verify-Only Mode (v0.0.36)

Audit without modification:

```bash
# Basic verification
sy /source /destination --verify-only

# JSON output for scripting
sy /source /destination --verify-only --json

# Use in scripts
if sy /backup /original --verify-only --json; then
  echo "Backup verified successfully"
fi
```

**Exit Codes:**
- `0`: All files match
- `1`: Mismatches or differences found
- `2`: Errors occurred

**Output:**
- Files matched: count of identical checksums
- Files mismatched: list with different content
- Files only in source: missing from destination
- Files only in destination: extra files
- Errors: files that couldn't be verified
- Duration: total verification time

### Error Reporting (v0.0.34)

Comprehensive error collection:
- All errors collected during parallel execution
- Sync continues for successful files (up to max_errors threshold)
- Users see ALL problems at once, not just first failure
- Color-coded output with file path and action context

## Advanced Sync

### Bidirectional Sync (v0.0.43+)

Two-way synchronization with conflict resolution:

```bash
# Basic bidirectional sync
sy --bidirectional /laptop/docs /backup/docs
sy -b /local /remote  # Short form

# Dry-run to preview changes
sy -b /a /b --dry-run
```

**Conflict Resolution Strategies (6 options):**

1. **newer** (default): Most recent modification time wins
   ```bash
   sy -b /a /b --conflict-resolve newer
   ```

2. **larger**: Largest file size wins
   ```bash
   sy -b /a /b --conflict-resolve larger
   ```

3. **smaller**: Smallest file size wins
   ```bash
   sy -b /a /b --conflict-resolve smaller
   ```

4. **source**: Source always wins (force push)
   ```bash
   sy -b /a /b --conflict-resolve source
   ```

5. **dest**: Destination always wins (force pull)
   ```bash
   sy -b /a /b --conflict-resolve dest
   ```

6. **rename**: Keep both files with `.conflict-{timestamp}-{side}` suffix
   ```bash
   sy -b /a /b --conflict-resolve rename
   ```

**Safety Features:**
- Deletion limit: default 50% threshold prevents mass deletion
- Content equality checks: reduces false conflict detection
- State persistence: survives interruptions and errors
- Conflict history logging (v0.0.46+): audit trail in `~/.cache/sy/bisync/<pair>.conflicts.log`
- Exclude patterns: respects `.gitignore` files

**State Tracking:**
- Text-based state in `~/.cache/sy/bisync/` (v0.0.44+)
- Detects new files, modifications, deletions on both sides
- Handles 9 change types including conflicts
- Clear state: `sy -b /a /b --clear-bisync-state`

### Rsync-Style Filters (v0.0.18)

Ordered include/exclude rules (first match wins):

```bash
# Include only .txt files
sy /source /destination --filter="+ *.txt" --filter="- *"

# Only .rs files in all directories
sy /source /destination --filter="+ */" --filter="+ *.rs" --filter="- *"

# Exclude build directory and contents
sy /source /destination --filter="- build/" --filter="+ *"

# Simple patterns
sy /source /destination --include "*.txt" --exclude "*"
sy /source /destination --exclude "*.log" --exclude "node_modules"
```

**Pattern Matching:**
- Directory-only patterns with trailing slash (e.g., `build/`)
- Wildcard directory patterns (e.g., `*/` for all directories)
- Basename matching (no slash) vs. full path matching (with slash)
- Compatible with rsync filter semantics

### Watch Mode (v0.0.12)

Continuous file monitoring for real-time sync:

```bash
sy /source /destination --watch
```

**Features:**
- 500ms debouncing to avoid excessive syncing
- Graceful Ctrl+C shutdown
- Cross-platform (Linux, macOS, Windows)

### Resume Support (v0.0.13)

Automatic recovery from interrupted syncs:

```bash
# Interrupt with Ctrl+C
sy /large /destination

# Re-run to resume from checkpoint
sy /large /destination
```

**Features:**
- State file: `.sy-state.json` in destination
- Flag compatibility checking
- Skips already-completed files on resume

**Network Interruption Recovery (v0.0.49):**

```bash
sy /local user@host:/remote --retry 5                  # Retry up to 5 times
sy /local user@host:/remote --retry 3 --retry-delay 2  # 3 retries with 2s initial delay
sy /local user@host:/remote --resume-only              # Only resume interrupted transfers
sy /local user@host:/remote --clear-resume-state       # Clear resume state
```

**Features:**
- Exponential backoff: 1s → 2s → 4s (max 30s delay)
- Resume state: stored in `~/.cache/sy/resume/`
- 1MB chunks with BLAKE3-based IDs

### JSON Output (v0.0.11)

Machine-readable NDJSON format:

```bash
sy /source /destination --json
sy /source /destination --json | jq
```

**Events:**
- start, create, update, skip, delete, summary
- Auto-suppresses logging in JSON mode
- Exit codes for scripting

### Config Profiles (v0.0.11)

Save common sync configurations:

```bash
# Use saved profile
sy --profile backup-home

# Show available profiles
sy --list-profiles

# Show profile details
sy --show-profile backup-home
```

**Config file:** `~/.config/sy/config.toml`
**Note:** CLI args override profile settings

## Network & Remote

### SSH Sync (Phase 2)

SFTP-based synchronization:

```bash
sy /local user@host:/remote
sy user@host:/remote /local
```

**Features:**
- SSH config integration (~/.ssh/config)
- Delta sync enabled for all remote operations
- Cross-transport delta sync (v0.0.19-v0.0.21)
- Automatic remote file update detection
- Proper mtime preservation

### SSH Optimizations (v0.0.42)

**Connection Pooling:**
- True parallel SSH transfers with N connections for N workers
- Round-robin session distribution via atomic counter
- Pool size automatically matches `--parallel` worker count
- Avoids ControlMaster bottleneck

```bash
sy /source user@host:/dest -j 20  # 20 workers = 20 SSH connections
```

**Sparse File Transfer:**
- Automatic detection of sparse files (VM images, databases, etc.)
- Detects data regions using SEEK_HOLE/SEEK_DATA on Unix
- Transfers only actual data, not holes
- 10x bandwidth savings for VM images
- 5x bandwidth savings for database files
- Graceful fallback if detection fails

```bash
sy /vm/disk.vmdk user@host:/backup/  # Only transfers data regions
```

**SSH Bidirectional Sync (v0.0.46+):**

```bash
sy -b /local/docs user@host:/remote/docs       # Local ↔ Remote
sy -b user@host1:/data user@host2:/backup      # Remote ↔ Remote
sy -b /laptop/work user@server:/work -p 8      # With parallel transfers
```

### S3/Cloud Storage (Phase 10) - **EXPERIMENTAL**

**Status:** Implemented but needs more testing. Use with caution for production data.

Multi-cloud support:

```bash
# AWS S3
sy /local s3://my-bucket/backups/

# Cloudflare R2 / Backblaze B2 / Wasabi
sy /local s3://my-bucket/data?endpoint=https://...

# Specify region
sy /data s3://my-bucket/data?region=us-west-2
```

**Supported Providers:**
- AWS S3 (native support)
- Cloudflare R2 (via custom endpoint)
- Backblaze B2 (via custom endpoint)
- Wasabi (via custom endpoint)
- Any S3-compatible service

**Authentication (automatic via AWS SDK):**
- Environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)
- ~/.aws/credentials and ~/.aws/config
- IAM roles (when running on AWS)
- SSO profiles

**Features:**
- Automatic multipart upload for large files (>100MB)
- 5MB part size (S3 minimum requirement)
- Force path-style addressing for non-AWS services
- Full Transport trait implementation
- Bidirectional sync (upload and download)

**Requirements:** Compile with `--features s3`

## Developer Experience

### Hooks (Phase 9)

Pre-sync and post-sync hook execution:

```bash
sy /source /destination                      # Runs hooks automatically
sy /source /destination --no-hooks           # Disable hooks
sy /source /destination --abort-on-hook-failure  # Abort if hooks fail
```

**Hook Discovery:**
- Auto-discovered from `~/.config/sy/hooks/`
- Supported: Unix (.sh/.bash/.zsh/.fish), Windows (.bat/.cmd/.ps1/.exe)

**Environment Variables:**
- SY_SOURCE, SY_DESTINATION
- SY_FILES_*, SY_BYTES_*
- SY_DRY_RUN, SY_DELETE

**Use Cases:**
- Notifications (desktop, Slack, email)
- Backups before sync
- Custom validation
- Integration with other tools

### Ignore Templates (Phase 9)

Sy-specific ignore patterns:

```bash
# Use global template
sy /rust-project /backup --ignore-template rust
sy /node-app /backup --ignore-template node

# Combine multiple templates
sy /project /backup --ignore-template rust --ignore-template node

# Project-specific .syignore
echo "build/" > /project/.syignore
sy /project /backup  # Auto-loaded
```

**Priority Order:**
1. CLI flags
2. .syignore
3. Templates
4. .gitignore

**Template Location:** `~/.config/sy/templates/{name}.syignore`
**Built-in Templates:** rust, node, python (see templates/ directory)

### Enhanced Dry-Run (Phase 9)

Detailed preview with byte impact:

```bash
sy /src /dst --dry-run --diff
sy /src /dst --dry-run --diff --delete
```

**Output:**
- Clear "Would create/update/delete" messaging
- File sizes for changed files
- Byte statistics summary (total bytes to add/change/delete)
- Color-coded (yellow for changes, red for deletions)

## Platform Support

### Symlink Handling (v0.0.15)

Three modes:

```bash
# Preserve symlinks as symlinks (default)
sy /source /destination --links preserve

# Follow symlinks and copy targets
sy /source /destination -L

# Skip all symlinks
sy /source /destination --links skip
```

**Features:**
- Detects broken symlinks and logs warnings
- Cross-platform (Unix/Linux/macOS)

### Sparse File Support (v0.0.15)

Automatic detection and preservation:

```bash
sy /source /destination  # Automatic
```

**Features:**
- Detects files with "holes" (sparse files)
- Preserves sparseness during transfer
- Efficient transfer: only allocated blocks copied
- Critical for VM disk images, database files
- Zero configuration: works transparently
- Cross-platform (Unix/Linux/macOS)

### Extended Attributes (v0.0.16)

Preserve xattrs:

```bash
sy /source /destination -X
```

**Preserves:**
- macOS Finder info
- Security contexts
- Custom metadata
- Always scanned, conditionally preserved
- Cross-platform (Unix/Linux/macOS)

### Hardlink Preservation (v0.0.17)

Preserve hard links between files:

```bash
sy /source /destination -H
sy /source /destination --preserve-hardlinks
```

**Features:**
- Tracks inode numbers during scan
- Creates hardlinks instead of copying duplicate data
- Preserves disk space savings
- Full parallel support with async coordination
- Cross-platform (Unix/Linux/macOS)

### ACL Preservation (v0.0.17)

Preserve POSIX Access Control Lists:

```bash
sy /source /destination -A
sy /source /destination --preserve-acls
```

**Features:**
- Fine-grained permissions beyond owner/group/other
- Parses and applies ACLs using standard text format
- Always scanned, conditionally preserved
- Essential for enterprise systems
- Cross-platform (Unix/Linux/macOS)

### BSD File Flags (v0.0.41, macOS only)

Preserve macOS file flags:

```bash
sy /source /destination -F
sy /source /destination --preserve-flags
```

**Preserves:**
- Hidden flag (Finder hidden)
- Immutable flag
- nodump flag
- Other BSD flags

**Features:**
- Uses chflags() syscall
- Explicitly sets or clears flags
- Essential for macOS backups

### Archive Mode (v0.0.18)

Equivalent to `-rlptgoD` (rsync compatibility):

```bash
sy /source /destination -a
sy /source /destination --archive

# Full-fidelity backup
sy /source /destination -a -X -A -H -F
```

**Includes:**
- Recursive
- Links
- Permissions
- Times
- Group
- Owner
- Devices

### Individual Metadata Flags

```bash
sy /source /destination -p  # Permissions only
sy /source /destination -t  # Modification times only
sy /source /destination -g  # Group (requires permissions)
sy /source /destination -o  # Owner (requires root)
sy /source /destination -D  # Device files (requires root)
sy /source /destination -ptg  # Combine flags
```

### File Comparison Modes (v0.0.18)

```bash
# Always compare checksums (ignore mtime)
sy /source /destination --ignore-times

# Only compare file size (skip mtime checks)
sy /source /destination --size-only

# Pre-transfer checksums: skip if content identical
sy /source /destination -c
sy /source /destination --checksum
```

### Deletion Safety (v0.0.18)

```bash
# Allow up to 75% of files to be deleted
sy /source /destination --delete --delete-threshold 75

# Skip safety checks (dangerous!)
sy /source /destination --delete --force-delete
```

**Default:** 50% threshold, prompts for confirmation if >1000 files

## Scale Features (Phase 11)

### Incremental Scanning with Cache (v0.0.22)

Cache directory mtimes for faster re-syncs:

```bash
sy /large-project /backup --use-cache  # Enable cache
sy /large-project /backup --use-cache  # 2nd run: 1.67-1.84x faster
sy /large-project /backup --clear-cache  # Clear cache
```

**Features:**
- Cache directory mtimes to detect unchanged directories
- Store file metadata in JSON cache
- Skip rescanning unchanged directories
- 1.67-1.84x speedup measured (10-100x expected on large datasets)
- Cache file: `.sy-dir-cache.json` in destination
- Automatic cache invalidation on directory mtime change
- 1-second mtime tolerance for filesystem granularity

### Streaming Scanner

O(1) memory usage:

```bash
sy /millions-of-files /destination
```

**Features:**
- Iterator-based file processing
- No loading all files into RAM
- 1M files: 150MB → O(1) constant memory
- Legacy `scan()` API preserved for compatibility
- New `scan_streaming()` API for large-scale syncs

### Parallel Directory Scanning

Automatic CPU core detection:

**Features:**
- Uses all available CPU cores
- 2-4x faster scanning on many subdirectories
- Configurable thread count via `Scanner::with_threads()`
- Zero overhead for small directories

### Bloom Filter Deletion

Space-efficient existence checks:

**Features:**
- 1.2 bytes per file vs 100+ bytes for HashSet
- 1% false positive rate for optimal memory usage
- 1M files: 1.2MB Bloom filter vs 100MB HashSet
- 100x memory reduction for deletion checks
- Automatic threshold: >10k files uses Bloom filter
- Zero false negatives (safe deletions guaranteed)

### Batch Processing

Process files in configurable batches:

**Features:**
- Default 10,000 files per batch
- Balances memory usage and performance
- Prevents memory exhaustion on multi-million file syncs
- Consistent memory usage regardless of dataset size

### Performance at Scale

**Tested:**
- 100k+ files (stress tests)
- Designed for millions of files without memory spikes
- 1M file sync: ~150MB RAM → ~5MB RAM
- Incremental re-syncs: 1.67-1.84x faster with cache

## Comparison with rsync

| Feature | rsync | sy |
|---------|-------|-----|
| **Performance (local)** | baseline | **2-11x faster** |
| Parallel file transfers | ❌ | ✅ |
| Parallel checksums | ❌ | ✅ |
| SSH connection pooling | ❌ | ✅ **N workers = N connections** |
| SSH sparse file transfer | ❌ | ✅ **Auto-detect, 10x savings** |
| Delta sync | ✅ | ✅ |
| Cross-transport delta sync | ❌ | ✅ **Auto-detects updates!** |
| Streaming delta | ❌ | ✅ **Constant memory!** |
| True O(1) rolling hash | ❌ | ✅ **2ns per operation!** |
| Block checksums | ✅ MD5 | ✅ xxHash3 |
| Cryptographic verification | ✅ MD5 | ✅ **BLAKE3** |
| Compression | ✅ | ✅ **Zstd (8 GB/s)** |
| Compression auto-detection | ❌ | ✅ **Content sampling** |
| Bandwidth limiting | ✅ | ✅ |
| File filtering | ✅ | ✅ **Rsync-style** |
| Resume support | ❌ | ✅ |
| Watch mode | ❌ | ✅ |
| JSON output | ❌ | ✅ |
| Hooks | ❌ | ✅ |
| Incremental scanning cache | ❌ | ✅ **1.67-100x faster re-syncs** |
| Checksum database | ❌ | ✅ **10-100x faster** |
| Verify-only mode | ❌ | ✅ **Audit integrity** |
| S3/Cloud storage | ❌ | ✅ **AWS, R2, B2, Wasabi (experimental)** |
| Bidirectional sync | ❌ | ✅ **6 strategies** |
| Conflict resolution | ❌ | ✅ **6 strategies** |
| Hardlink preservation | ✅ | ✅ **Parallel-safe** |
| ACL preservation | ✅ | ✅ |
| Extended attributes | ✅ | ✅ |
| Sparse files | ✅ | ✅ **SSH + local** |
| Modern UX | ❌ | ✅ |
| Single file sync | ⚠️ Complex | ✅ |
| Zero compiler warnings | N/A | ✅ |

## See Also

- [USAGE.md](USAGE.md) - Comprehensive usage examples
- [PERFORMANCE.md](PERFORMANCE.md) - Performance analysis and benchmarks
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) - Common issues and solutions
- [DESIGN.md](../DESIGN.md) - Technical design and architecture
