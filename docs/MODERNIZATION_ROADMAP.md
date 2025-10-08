# Modernization Roadmap - sy v0.1.0 → v1.0

**Status**: In Progress - Phase 6 Core Complete! (2025-10-08)
**Current Version**: v0.0.17
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

### ✅ Phase 6 Core Complete (v0.0.17)
1. ✅ **Hardlink preservation** - -H flag preserves hard links (v0.0.17)

### Critical Gaps Remaining for v1.0
1. ~~**Symlinks**~~ ✅ **DONE** (v0.0.15)
2. ~~**Sparse files**~~ ✅ **DONE** (v0.0.15)
3. ~~**Extended attributes**~~ ✅ **DONE** (v0.0.16)
4. ~~**Hardlinks**~~ ✅ **DONE** (v0.0.17)
5. **ACLs** - Advanced access control lists (MEDIUM)
6. **Hooks** - Pre/post sync extensibility (LOW)
7. **Cloud backends** - S3/R2/Backblaze (rclone territory) (DEFER)

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

### Phase 6 (v0.3.0) - Advanced Filesystem Features
**Goal**: Handle remaining filesystem edge cases for full-fidelity backups

**Timeline**: 2 weeks

**Features**:
1. **Hardlinks**
   - Detect hardlinks via inode tracking
   - Preserve hardlink relationships
   - `--hard-links` flag to enable
   - Tests: Verify inode counts match

2. **Extended attributes & ACLs**
   - `-X` flag to preserve xattrs
   - `-A` flag to preserve ACLs (requires root)
   - Platform-specific: macOS resource forks, Windows alternate data streams
   - Tests: Verify xattrs/ACLs preserved

**Note**: Symlinks and sparse files moved to Phase 5 (completed in v0.0.15)

**Deliverable**: Full-fidelity backups (like rsync -a)

---

### Phase 7 (v0.4.0) - Developer Experience
**Goal**: Extensibility & improved workflows

**Timeline**: 1.5 weeks

**Features**:
1. **Hooks** (pre/post sync)
   - `~/.config/sy/hooks/pre-sync.sh` - Run before sync
   - `~/.config/sy/hooks/post-sync.sh` - Run after sync
   - Environment variables: `$SY_SOURCE`, `$SY_DEST`, `$SY_FILES_TRANSFERRED`
   - Example: Git commit after sync, notification on completion

2. **Ignore templates**
   - `--ignore-template node` → Loads .gitignore + node_modules patterns
   - Built-in templates: node, rust, python, docker, mac, windows
   - Stored in `~/.config/sy/templates/`
   - Users can add custom templates

3. **Improved dry-run**
   - Colored diff-style output (like git diff)
   - `--dry-run --diff` shows content changes
   - Side-by-side comparison for modified files
   - Summary: bytes added/removed/changed

**Deliverable**: Great developer experience

**Note**: TUI mode deferred to Phase 9+ (optional feature, not critical for v1.0)

---

### Phase 8 (v0.5.0) - Cloud Era
**Goal**: Support cloud storage backends

**Timeline**: 3-4 weeks

**Features**:
1. **S3-compatible backends**
   - `sy /local s3://bucket/path`
   - Support: AWS S3, Cloudflare R2, Backblaze B2, Wasabi
   - S3 API via rusoto or aws-sdk-rust
   - Multipart upload for large files

2. **Object storage optimization**
   - Different strategy for blob stores (no mtime, use ETags)
   - Batch API calls (list 1000 objects at once)
   - Parallel uploads (already have infrastructure)
   - Compression before upload (already implemented)

3. **Cloud-specific features**
   - Storage class selection (Standard, IA, Glacier)
   - Server-side encryption (SSE-S3, SSE-KMS)
   - Lifecycle policies (delete after N days)
   - Cost estimation (estimate transfer + storage costs)

4. **Container awareness**
   - Auto-detect Docker volumes (`/var/lib/docker/volumes/...`)
   - Efficient sync of container layers
   - Integration with Docker/Podman APIs
   - Tests: Sync to/from running containers

**Deliverable**: Compete with rclone for cloud use cases

---

### Phase 9 (v0.6.0) - Scale
**Goal**: Handle millions of files efficiently

**Timeline**: 2 weeks

**Features**:
1. **Incremental scanning**
   - Don't re-scan unchanged directories
   - Cache directory mtimes
   - Only scan changed subtrees
   - Bloom filter for quick "file exists" checks

2. **Memory-efficient deletion**
   - Stream deletion list instead of loading into memory
   - Batch delete operations (1000 at a time)
   - Progress tracking for large deletions

3. **Deduplication**
   - Content-addressed storage (like git)
   - `--dedup` flag to enable
   - Hash-based block deduplication
   - Useful for backups with many similar files

4. **State caching**
   - Cache file metadata between runs
   - `.sy-cache` directory in destination
   - SQLite database for metadata
   - Dramatically faster re-syncs (skip scanning)

**Deliverable**: Sync millions of files without issues

---

### Phase 10 (v1.0.0) - Production Release
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

## Feature Comparison: sy v0.0.17 vs Competition

| Feature | rsync | rclone | **sy v0.0.17** | sy v1.0 (planned) |
|---------|-------|--------|----------------|-------------------|
| **Performance (local)** | baseline | N/A | **2-11x faster** ✅ | **2-11x faster** ✅ |
| **Performance (network)** | baseline | 4x with --transfers | **5-10x faster** ✅ | **5-10x faster** ✅ |
| **Delta sync** | ✅ | ❌ | ✅ v0.0.7 | ✅ |
| **Parallel files** | ❌ | ✅ | ✅ v0.0.8 | ✅ |
| **Parallel chunks** | ❌ | ✅ | ❌ | ✅ (Phase 8) |
| **Resume** | ✅ | ✅ | ✅ v0.0.13 | ✅ |
| **Symlinks** | ✅ | ✅ | ✅ v0.0.15 | ✅ |
| **Sparse files** | ✅ | ❌ | ✅ v0.0.15 | ✅ |
| **Extended attributes** | ✅ | ❌ | ✅ v0.0.16 | ✅ |
| **Hardlinks** | ✅ | ❌ | **✅ v0.0.17** | ✅ |
| **ACLs** | ✅ | ❌ | ⏳ Next | ✅ (Phase 6) |
| **Cloud storage** | ❌ | ✅ | ❌ | ✅ (Phase 8) |
| **Watch mode** | ❌ | ❌ | ✅ v0.0.12 | ✅ |
| **JSON output** | ❌ | ✅ | ✅ v0.0.11 | ✅ |
| **Config profiles** | ❌ | ✅ | ✅ v0.0.11 | ✅ |
| **Verification** | checksum | hash | **Multi-layer** ✅ v0.0.14 | **Multi-layer** ✅ |
| **Beautiful output** | ❌ | ⚠️ | ✅ | ✅ |

**Key**: ✅ Done | ⏳ In progress | ❌ Not supported

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

**Estimated**: 13.5-16.5 weeks remaining (~3.5 months)

- ✅ Phase 4: Complete (v0.0.11-v0.0.13)
- Phase 5: 2 weeks
- Phase 6: 3 weeks
- Phase 7: 1.5 weeks (TUI deferred)
- Phase 8: 3-4 weeks
- Phase 9: 2 weeks
- Phase 10: 2 weeks

**Target Release**: Q1-Q2 2026

**Beta Program**: Start after Phase 6 (v0.3.0) with symlink support

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

1. ✅ **Phase 4 implementation** - Complete (v0.0.11-v0.0.13)
2. **Phase 5 planning** - Design verification & reliability features
3. **Community feedback** - Post roadmap, get input on priorities
4. **Beta testing** - Find early adopters for Phase 5

---

**Last Updated**: 2025-10-06
**Status**: Phase 4 complete, ready to begin Phase 5
**Current Version**: v0.0.13 (Phases 1-4 complete)
