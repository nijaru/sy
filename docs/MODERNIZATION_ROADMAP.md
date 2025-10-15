# Modernization Roadmap - sy v0.1.0 → v1.0

**Status**: In Progress - Phases 9-11 Complete! (2025-10-15)
**Current Version**: v0.0.22 (pending release)
**Goal**: Make sy a complete modern rsync replacement for 90%+ of use cases

---

## Executive Summary

**sy is already production-ready for developers** (2-11x faster than rsync), and Phases 4-5 have addressed major modern CLI gaps:

### ✅ Phase 4 Complete (v0.0.11-v0.0.13)
1. ✅ **JSON output** - Machine-readable NDJSON format (v0.0.11)
2. ✅ **Config profiles** - Reusable configurations (v0.0.11)
3. ✅ **Watch mode** - Continuous sync (v0.0.12)
4. ✅ **Resume support** - Automatic recovery from interrupts (v0.0.13)

### ✅ Phase 5 Core Complete (v0.0.14-v0.0.16)
1. ✅ **Verification modes** - fast/standard/verify/paranoid (v0.0.14)
2. ✅ **BLAKE3 end-to-end** - Cryptographic integrity verification (v0.0.14)
3. ✅ **State hardening** - Auto-delete corrupted state files (v0.0.14)
4. ✅ **Symlink support** - Preserve/follow/skip modes (v0.0.15)
5. ✅ **Sparse file support** - Detection and preservation (v0.0.15)
6. ✅ **Extended attributes** - -X flag for full-fidelity backups (v0.0.16)

### ✅ Phase 6 Complete (v0.0.17)
1. ✅ **Hardlink preservation** - -H flag preserves hard links (v0.0.17)
2. ✅ **ACL preservation** - -A flag preserves POSIX ACLs (v0.0.17)

### ✅ Phase 7 Complete (v0.0.18)
1. ✅ **Rsync-style filters** - Full filter rule support with include/exclude (v0.0.18)
2. ✅ **Remote → Local sync** - Bidirectional sync fully working (v0.0.18)
3. ✅ **Archive mode** - -a flag with full rsync compatibility (v0.0.18)

### ✅ Phase 8 Complete (v0.0.19-v0.0.21)
1. ✅ **Cross-transport delta sync** - Automatic remote file update detection (v0.0.19)
2. ✅ **xxHash3 fast verification** - 10x faster than BLAKE3 (v0.0.21)
3. ✅ **FileInfo abstraction** - Transport-agnostic metadata (v0.0.20)
4. ✅ **98% bandwidth savings** - Delta sync working for remote updates (v0.0.21)

### ✅ Phase 9 Complete (v0.0.22)
1. ✅ **Hooks system** - Pre/post sync extensibility with environment variables
2. ✅ **Ignore templates** - Built-in patterns for node, rust, python, etc.
3. ✅ **Enhanced dry-run** - Improved output clarity and formatting

### ✅ Phase 10 Complete (v0.0.22)
1. ✅ **S3 transport** - Full AWS S3, Cloudflare R2, Backblaze B2, Wasabi support
2. ✅ **Multipart uploads** - Efficient handling of large files (>100MB)
3. ✅ **Custom endpoints** - Support for S3-compatible services
4. ✅ **Path parsing** - `s3://bucket/path?region=...&endpoint=...` syntax

### ✅ Phase 11 Complete (v0.0.22)
1. ✅ **Incremental scanning** - Cache-based skip logic (1.67-1.84x faster re-syncs)
2. ✅ **Memory-efficient deletion** - Streaming with Bloom filters for >10k files
3. ✅ **State caching** - Directory mtime + file metadata cache
4. ⏳ **Deduplication** - Content-addressed storage (DEFERRED to v1.1+)

### Critical Gaps Remaining for v1.0
1. ~~**Symlinks**~~ ✅ **DONE** (v0.0.15)
2. ~~**Sparse files**~~ ✅ **DONE** (v0.0.15)
3. ~~**Extended attributes**~~ ✅ **DONE** (v0.0.16)
4. ~~**Hardlinks**~~ ✅ **DONE** (v0.0.17)
5. ~~**ACLs**~~ ✅ **DONE** (v0.0.17)
6. ~~**Filters**~~ ✅ **DONE** (v0.0.18)
7. ~~**Cross-transport delta**~~ ✅ **DONE** (v0.0.19-v0.0.21)
8. ~~**Hooks**~~ ✅ **DONE** (v0.0.22)
9. ~~**Cloud backends**~~ ✅ **DONE** (v0.0.22)
10. ~~**State caching**~~ ✅ **DONE** (v0.0.22)

---

## Analysis: What Makes a "Modern" Tool in 2024+

Looking at successful modern CLI tools:

| Tool | What Made It "Modern" |
|------|----------------------|
| **ripgrep** | Speed + smart defaults + JSON output |
| **fd** | Speed + gitignore awareness + colors |
| **eza** | Speed + icons + git integration |
| **bat** | Syntax highlighting + git diff + themes |
| **delta** | Better git diffs + side-by-side + syntax |
| **zoxide** | Frecency algorithm + learning |
| **starship** | Fast + config-driven + multi-shell |

### Common Patterns
✅ **Speed** - sy wins (2-11x faster)
✅ **Beautiful output** - sy has colors + progress
✅ **Smart defaults** - sy has gitignore awareness
✅ **JSON output** - Implemented (v0.0.11)
✅ **Watch mode** - Implemented (v0.0.12)
✅ **Config files** - Implemented (v0.0.11)
❌ **Extensibility** - Missing (no hooks yet)

---

## Protocol Analysis

### Should we add newer protocols?

**QUIC / HTTP/3**: ❌ **NO**
- Research (ACM 2024): 45% SLOWER on fast networks (>600 Mbps)
- Only beneficial for high-latency/high-loss scenarios
- SSH with BBR already optimized for packet loss (2-25x better than CUBIC)
- **Decision**: Keep SSH with TCP BBR, document QUIC decision in DESIGN.md

**WebRTC / P2P**: ❌ **NO** (for now)
- Niche use case (sync between browsers?)
- Adds massive complexity (STUN/TURN servers, NAT traversal)
- **Decision**: Defer to Phase 8+ if users request

**HTTP/2 for cloud backends**: ✅ **YES** (Phase 7+)
- Standard for S3/R2/Backblaze APIs
- Multiplexing helps with many small files
- **Decision**: Implement when adding cloud storage support

**BitTorrent-style multi-source**: 🤔 **MAYBE** (Phase 9+)
- Could sync from multiple sources simultaneously
- Useful for CDN-style distribution
- Complex, niche use case
- **Decision**: Research if users request

### Conclusion on Protocols
**Keep it simple**: SSH (current) + HTTP/2 for cloud (Phase 7+). No QUIC.

---

## Revised Phase Plan

### Phase 4 (v0.0.11-v0.0.13) - Critical Reliability ✅ **COMPLETE**
**Goal**: Make sy safe for production use

**Status**: Complete (2025-10-06)

**Features**:
1. ✅ **Resume support** (v0.0.13)
   - State file: `.sy-state.json` in destination
   - Flag compatibility checking on resume
   - Resume from last checkpoint on interruption
   - Filter completed files from sync tasks
   - Automatic cleanup on successful completion
   - **Note**: Periodic checkpointing deferred to Phase 5

2. ✅ **Watch mode** (v0.0.12)
   - `sy /src /dst --watch` - Continuous sync
   - File system events (notify 6.0)
   - Debouncing (500ms default)
   - Graceful shutdown on Ctrl+C (tokio::signal)
   - Event filtering (Create/Modify/Remove only)

3. ✅ **JSON output** (v0.0.11)
   - `sy /src /dst --json` - Machine-readable
   - One JSON object per line (NDJSON)
   - Schema: `{"type": "start|create|update|skip|delete|summary", ...}`
   - Auto-suppresses logging output

4. ✅ **Config profiles** (v0.0.11)
   - `~/.config/sy/config.toml` (XDG-compliant)
   - Named profiles: `sy --profile deploy-prod`
   - CLI args override profile settings
   - `--list-profiles` and `--show-profile` flags
   - Example:
     ```toml
     [profiles.deploy-prod]
     source = "./dist"
     destination = "user@prod:/var/www"
     exclude = ["*.map", "*.log"]
     delete = true
     ```

**Deliverable**: ✅ sy is safe and convenient for daily use (all 111 tests passing)

---

### Phase 5 (v0.0.14-v0.2.0) - Verification & Reliability 🔨 **IN PROGRESS**
**Goal**: Multi-layer integrity verification

**Timeline**: 2 weeks (started 2025-10-07)

**Status**: Core verification features complete! Transaction log deferred to later.

**Features**:
1. ✅ **BLAKE3 end-to-end verification** (v0.0.14)
   - Optional cryptographic checksums (slower but verifiable)
   - `--verify` flag enables BLAKE3
   - Per-file verification after transfer
   - Paranoid mode: verify every block
   - Stats displayed in summary output
   - JSON output includes verification counts

2. ✅ **Verification modes** (v0.0.14)
   - `--mode fast` - Size + mtime only
   - `--mode standard` - + xxHash3 checksums (default)
   - `--mode verify` - + BLAKE3 end-to-end
   - `--mode paranoid` - BLAKE3 + verify on read/write
   - Verification runs automatically after transfers
   - Failures logged with clear warnings

3. ✅ **State file hardening** (v0.0.14)
   - Comprehensive integrity checks on resume state
   - Auto-delete corrupted state files
   - Version, path, timestamp, and count validation
   - 8 new tests for corruption scenarios
   - `--clean-state` flag to force fresh sync

4. ✅ **Symlink support** (v0.0.15)
   - Three modes: preserve (default), follow, skip
   - `--links` flag to set mode, `-L` shortcut for follow
   - Detects and warns on broken symlinks
   - Cross-platform (Unix/Linux/macOS)
   - 3 comprehensive tests

5. ✅ **Sparse file support** (v0.0.15)
   - Automatic detection using st_blocks metadata
   - Preserves sparseness during transfer
   - Uses std::fs::copy() for sparse files (preserves holes)
   - Critical for VM disk images, database files
   - Zero configuration - works transparently
   - 2 tests (detection and transfer)

6. ⏳ **Advanced crash recovery** (Deferred to v0.5.0+)
   - Transaction log for file-level rollback
   - Per-operation tracking (start/write/commit/complete)
   - Automatic recovery on restart
   - **Note**: Current resume support already handles interrupted syncs well

7. ⏳ **Atomic operations** (Deferred to v0.5.0+)
   - Already implemented (write to temp, rename)
   - `--no-atomic` flag for special filesystems
   - **Note**: Current implementation is atomic by default

**Deliverable**: ✅ Verifiable integrity + filesystem features (core features complete, including symlinks & sparse files)

---

### Phase 6 (v0.0.17) - Advanced Filesystem Features ✅ **COMPLETE**
**Goal**: Handle remaining filesystem edge cases for full-fidelity backups

**Status**: Complete (2025-10-08)

**Features**:
1. ✅ **Hardlinks** (v0.0.17)
   - Detect hardlinks via inode tracking (u64 key: device + inode)
   - Preserve hardlink relationships across transfers
   - `-H` / `--preserve-hardlinks` flag to enable
   - Full parallel support with async coordination
   - Tests: Verify inode counts match, multiple hardlinks preserved

2. ✅ **ACLs** (v0.0.17)
   - `-A` / `--preserve-acls` flag to preserve POSIX ACLs
   - Always scanned, conditionally preserved
   - Parse and apply ACLs using standard text format
   - Cross-platform (Unix/Linux/macOS)
   - Tests: Verify ACLs preserved correctly

**Note**: Extended attributes completed in Phase 5 (v0.0.16). Symlinks and sparse files also in Phase 5 (v0.0.15)

**Deliverable**: ✅ Full-fidelity backups (like rsync -aHAX)

---

### Phase 7 (v0.0.18) - Rsync Compatibility & Filters ✅ **COMPLETE**
**Goal**: Feature parity with rsync for power users

**Status**: Complete (2025-10-09)

**Features**:
1. ✅ **Rsync-style filters** (v0.0.18)
   - `--filter` flag for ordered include/exclude rules (first match wins)
   - `--include` and `--exclude` flags for simple patterns
   - Directory-only patterns with trailing slash (e.g., `build/`)
   - Wildcard directory patterns (e.g., `*/` to include all directories)
   - Basename matching (no slash) vs. full path matching (with slash)
   - Compatible with rsync filter semantics
   - Full filter engine with comprehensive tests

2. ✅ **Remote → Local sync** (v0.0.18)
   - Bidirectional sync fully working (e.g., `user@host:/src /local/dst`)
   - Proper mtime preservation across transports
   - SFTP-based file reading for remote sources
   - Cross-transport operations handle both directions

3. ✅ **Archive mode & metadata flags** (v0.0.18)
   - `-a` / `--archive` flag (equivalent to `-rlptgoD`)
   - Individual metadata preservation flags: `-p`, `-t`, `-g`, `-o`, `-D`
   - Full rsync compatibility for backup workflows
   - File comparison modes: `--ignore-times`, `--size-only`, `-c` / `--checksum`

**Deliverable**: ✅ Rsync feature parity for advanced users

**Note**: Developer experience features (hooks, ignore templates, improved dry-run) deferred to Phase 9+

---

### Phase 8 (v0.0.19-v0.0.21) - Cross-Transport Delta Sync ✅ **COMPLETE**
**Goal**: Delta sync working across all transport combinations

**Status**: Complete (2025-10-13)

**Features**:
1. ✅ **Automatic remote file update detection** (v0.0.19-v0.0.20)
   - FileInfo abstraction for transport-agnostic metadata
   - SshTransport.file_info() using SFTP stat()
   - Delegation through DualTransport and TransportRouter
   - Files correctly detected as "updated" vs "created"
   - Critical bug fix enabling automatic delta sync

2. ✅ **xxHash3 fast verification** (v0.0.21)
   - Complete ChecksumType::Fast implementation
   - ~10x faster than BLAKE3 for non-cryptographic checksums
   - New src/integrity/xxhash3.rs module
   - Full test coverage (12 integrity tests)
   - Streaming hash computation for large files

3. ✅ **Bandwidth limit parsing from profiles** (v0.0.21)
   - Exposed cli::parse_size() as public API
   - Profile bwlimit now parsed correctly
   - Fixed final TODO in codebase

**Impact**: **98% bandwidth savings** demonstrated (50MB file, 1MB changed → only ~1MB transferred)

**Deliverable**: ✅ Delta sync works seamlessly for all sync scenarios (local→remote, remote→local, remote→remote)

---

### Phase 9 (v0.0.22) - Developer Experience ✅ **COMPLETE**
**Goal**: Extensibility & improved workflows

**Status**: Complete (2025-10-15)

**Features**:
1. ✅ **Hooks** (pre/post sync)
   - `~/.config/sy/hooks/pre-sync.sh` - Run before sync
   - `~/.config/sy/hooks/post-sync.sh` - Run after sync
   - Environment variables: `$SY_SOURCE`, `$SY_DEST`, `$SY_FILES_TRANSFERRED`
   - CLI flags: `--no-hooks`, `--hook-timeout`
   - Example: Git commit after sync, notification on completion

2. ✅ **Ignore templates**
   - `--ignore-template node` → Loads built-in patterns
   - Built-in templates: node, rust, python, docker, mac, windows
   - `.syignore` file support for custom patterns
   - Users can add custom templates

3. ✅ **Improved dry-run**
   - Enhanced output clarity and formatting
   - Better action labeling (Create/Update/Delete/Skip)
   - File count and size summaries

**Deliverable**: ✅ Great developer experience

**Note**: TUI mode deferred to v1.1+ (optional feature, not critical for v1.0)

---

### Phase 10 (v0.0.22) - Cloud Era ✅ **COMPLETE**
**Goal**: Support cloud storage backends

**Status**: Complete (2025-10-15)

**Features**:
1. ✅ **S3-compatible backends**
   - `sy /local s3://bucket/path` syntax working
   - Support: AWS S3, Cloudflare R2, Backblaze B2, Wasabi
   - Full S3 API integration via aws-sdk-rust
   - Multipart upload for large files (>100MB, 5MB chunks)
   - Router integration for local→S3 and S3→local syncs

2. ✅ **Object storage optimization**
   - Pagination for listing (handles large buckets)
   - ETags and LastModified for file comparison
   - Streaming download/upload
   - Parallel transfers (already have infrastructure)
   - Compression support (already implemented)

3. ⏳ **Cloud-specific features** (Deferred to v1.1+)
   - Storage class selection (Standard, IA, Glacier)
   - Server-side encryption (SSE-S3, SSE-KMS)
   - Lifecycle policies (delete after N days)
   - Cost estimation (estimate transfer + storage costs)

**Deliverable**: ✅ Basic S3 support working, competes with rclone for common use cases

---

### Phase 11 (v0.0.22) - Scale ✅ **COMPLETE**
**Goal**: Handle millions of files efficiently

**Status**: Complete (2025-10-15)

**Features**:
1. ✅ **Incremental scanning**
   - Cache directory mtimes to detect unchanged directories
   - Store file metadata (path, size, mtime, is_dir) in JSON cache
   - Skip rescanning unchanged directories (use cached file list)
   - Cache version control with auto-invalidation
   - 1.67-1.84x speedup measured (10-100x expected on large datasets)
   - CLI flags: `--use-cache`, `--clear-cache`

2. ✅ **Memory-efficient deletion**
   - Stream deletion list instead of loading into memory
   - Bloom filter for >10k files (100x memory reduction vs HashMap)
   - Automatic threshold switching (10k files)
   - No false negatives, handles false positives correctly

3. ⏳ **Deduplication** (Deferred to v1.1+)
   - Content-addressed storage (like git)
   - `--dedup` flag to enable
   - Hash-based block deduplication
   - Useful for backups with many similar files

4. ✅ **State caching**
   - Cache file metadata between runs
   - `.sy-dir-cache.json` in destination directory
   - JSON format (human-readable, version 2)
   - Dramatically faster re-syncs (skip scanning unchanged dirs)

**Deliverable**: ✅ Sync millions of files efficiently with bounded memory

---

### Phase 12 (v1.0.0) - Production Release
**Goal**: Stable, polished, well-distributed

**Timeline**: 2 weeks

**Tasks**:
1. **Security audit**
   - Run cargo-audit in CI (already done)
   - Fuzz testing (cargo-fuzz)
   - Review unsafe code usage (already zero, keep it that way)
   - Document security model

2. **Performance profiling**
   - Flamegraphs for CPU hotspots
   - Memory profiling (heaptrack)
   - Benchmark suite vs rsync/rclone
   - Document performance characteristics

3. **CI/CD pipeline**
   - Already have GitHub Actions
   - Add: Release automation
   - Add: Changelog generation
   - Add: Version bumping

4. **Distribution**
   - ✅ crates.io (pending auth)
   - Homebrew formula (`brew install sy`)
   - Arch AUR package
   - Debian/Ubuntu PPA
   - Docker image (alpine-based)
   - Pre-built binaries (GitHub releases)

5. **Documentation**
   - Man pages (`man sy`)
   - Website (mdBook or Docusaurus)
   - Screencast/demo (asciinema)
   - Migration guide from rsync

**Deliverable**: sy v1.0 - Production-ready modern rsync 🚀

---

## Feature Comparison: sy v0.0.21 vs Competition

| Feature | rsync | rclone | **sy v0.0.21** | sy v1.0 (planned) |
|---------|-------|--------|----------------|-------------------|
| **Performance (local)** | baseline | N/A | **2-11x faster** ✅ | **2-11x faster** ✅ |
| **Performance (network)** | baseline | 4x with --transfers | **5-10x faster** ✅ | **5-10x faster** ✅ |
| **Delta sync** | ✅ | ❌ | ✅ v0.0.7 | ✅ |
| **Cross-transport delta** | ❌ | ❌ | **✅ v0.0.19-v0.0.21** | ✅ |
| **Parallel files** | ❌ | ✅ | ✅ v0.0.8 | ✅ |
| **Parallel chunks** | ❌ | ✅ | ❌ | ✅ (Phase 10) |
| **Resume** | ✅ | ✅ | ✅ v0.0.13 | ✅ |
| **Symlinks** | ✅ | ✅ | ✅ v0.0.15 | ✅ |
| **Sparse files** | ✅ | ❌ | ✅ v0.0.15 | ✅ |
| **Extended attributes** | ✅ | ❌ | ✅ v0.0.16 | ✅ |
| **Hardlinks** | ✅ | ❌ | ✅ v0.0.17 | ✅ |
| **ACLs** | ✅ | ❌ | **✅ v0.0.17** | ✅ |
| **Rsync-style filters** | ✅ | ⚠️ | **✅ v0.0.18** | ✅ |
| **Cloud storage** | ❌ | ✅ | ❌ | ✅ (Phase 10) |
| **Watch mode** | ❌ | ❌ | ✅ v0.0.12 | ✅ |
| **JSON output** | ❌ | ✅ | ✅ v0.0.11 | ✅ |
| **Config profiles** | ❌ | ✅ | ✅ v0.0.11 | ✅ |
| **Verification** | checksum | hash | **Multi-layer** ✅ v0.0.14 | **Multi-layer** ✅ |
| **Beautiful output** | ❌ | ⚠️ | ✅ | ✅ |
| **Hooks** | ❌ | ✅ | ⏳ Phase 9 | ✅ (Phase 9) |

**Key**: ✅ Done | ⏳ Next | ❌ Not supported

---

## Decision Log

### ❌ Features We Won't Add (and Why)

1. **QUIC/HTTP/3 transport**
   - Reason: 45% slower than TCP on fast networks (ACM 2024)
   - Alternative: SSH with TCP BBR is already optimized

2. **Bidirectional sync** (Syncthing-style)
   - Reason: Adds massive complexity (conflict resolution, vector clocks)
   - Alternative: Defer to Phase 11+ if strongly requested
   - Note: Could add limited bidirectional (newest-wins) in Phase 8

3. **Built-in encryption at rest**
   - Reason: Better handled by filesystem (LUKS, FileVault)
   - Alternative: Document how to use with encrypted filesystems

4. **Plugin system**
   - Reason: Hooks (Phase 7) cover 90% of use cases
   - Alternative: Rust is not dynamic, plugins would require embedding interpreter

5. **GUI application**
   - Reason: sy is a CLI-first tool
   - Alternative: Third parties can build GUIs using `--json` output

### ⏸️ Features Deferred (Post-v1.0)

1. **TUI mode** (full-screen interface)
   - Reason: Not critical for v1.0, nice-to-have
   - Current output (colors + progress bars) is sufficient
   - Defer to Phase 9+ or v1.1+ if users request
   - Note: Would use ratatui if implemented

---

## Timeline to v1.0

**Estimated**: 2 weeks remaining (Phase 12 only!)

- ✅ Phase 4: Complete (v0.0.11-v0.0.13)
- ✅ Phase 5: Complete (v0.0.14-v0.0.16)
- ✅ Phase 6: Complete (v0.0.17)
- ✅ Phase 7: Complete (v0.0.18)
- ✅ Phase 8: Complete (v0.0.19-v0.0.21)
- ✅ Phase 9: Complete (v0.0.22) - Developer Experience
- ✅ Phase 10: Complete (v0.0.22) - Cloud Era (S3)
- ✅ Phase 11: Complete (v0.0.22) - Scale optimizations
- 🎯 Phase 12: 2 weeks (Production Release)

**Target Release**: Q4 2025 (November!)

**Current Status**: 11 of 12 phases complete! Ready for Phase 12 (Production Release Prep)

---

## Success Metrics for v1.0

1. **Performance**: Faster than rsync for 90%+ of use cases
2. **Reliability**: Resume works 100% of the time
3. **Compatibility**: Handles symlinks, sparse files, xattrs
4. **Adoption**: 1000+ GitHub stars, 500+ crates.io downloads/week
5. **Quality**: Zero CVEs, <5 open bugs, A+ code quality

---

## crates.io Token Permissions

**For initial publish** (sy doesn't exist yet):
- Need: `publish-new` permission OR full access token
- Get token: https://crates.io/settings/tokens
- Run: `cargo login <token>`

**For future updates** (after initial publish):
- Use scoped token: `publish-update` only (more secure)
- Separate tokens for CI vs manual (revoke CI token if leaked)

**Best practice**:
```bash
# Initial publish (one-time)
cargo login <full-access-token>
cargo publish

# Create scoped token for future
# At https://crates.io/settings/tokens:
# - Name: "sy updates"
# - Permissions: publish-update
# - Scopes: sy

# Use for all future publishes
cargo login <scoped-token>
```

---

## Next Steps

1. ✅ **Phases 4-11 implementation** - Complete (v0.0.11-v0.0.22)
2. 🎯 **Phase 12 execution** - Production release preparation
3. **Release v0.0.22** - Incremental scanning, S3 support, hooks
4. **Community feedback** - Post about v0.0.22 features
5. **Beta testing** - Find S3 users, test cache on large datasets

**Recommendations for Phase 12**:
- **Security audit** - Run cargo-audit, fuzz testing
- **Performance profiling** - Flamegraphs, memory analysis
- **Distribution** - Homebrew, AUR, crates.io
- **Documentation** - Man pages, website, migration guide

---

**Last Updated**: 2025-10-15
**Status**: Phases 9-11 complete, ready for Phase 12 (Production Release)
**Current Version**: v0.0.22 (pending release, 289 tests passing)
