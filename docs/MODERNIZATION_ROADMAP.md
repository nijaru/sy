# Modernization Roadmap - sy v0.1.0 → v1.0

**Status**: In Progress - Phase 5 Started! (2025-10-07)
**Current Version**: v0.0.14-dev
**Goal**: Make sy a complete modern rsync replacement for 90%+ of use cases

---

## Executive Summary

**sy is already production-ready for developers** (2-11x faster than rsync), and Phase 4 has addressed major modern CLI gaps:

### ✅ Phase 4 Complete (v0.0.11-v0.0.13)
1. ✅ **JSON output** - Machine-readable NDJSON format (v0.0.11)
2. ✅ **Config profiles** - Reusable configurations (v0.0.11)
3. ✅ **Watch mode** - Continuous sync (v0.0.12)
4. ✅ **Resume support** - Automatic recovery from interrupts (v0.0.13)

### Critical Gaps Remaining for v1.0
1. **Symlinks** - Very common in real-world usage (HIGH)
2. **Sparse files** - VM images, databases (MEDIUM)
3. **Extended attributes** - Full backup fidelity (MEDIUM)
4. **Hooks** - Pre/post sync extensibility (LOW)
5. **Cloud backends** - S3/R2/Backblaze (rclone territory) (DEFER)

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

4. ⏳ **Advanced crash recovery** (Deferred to v0.5.0+)
   - Transaction log for file-level rollback
   - Per-operation tracking (start/write/commit/complete)
   - Automatic recovery on restart
   - **Note**: Current resume support already handles interrupted syncs well

5. ⏳ **Atomic operations** (Deferred to v0.5.0+)
   - Already implemented (write to temp, rename)
   - `--no-atomic` flag for special filesystems
   - **Note**: Current implementation is atomic by default

**Deliverable**: ✅ Verifiable integrity for critical data (core features complete)

---

### Phase 6 (v0.3.0) - Filesystem Features
**Goal**: Handle common filesystem edge cases

**Timeline**: 3 weeks

**Features**:
1. **Symlinks**
   - Detect and handle symbolic links
   - Modes: `--copy-links`, `--skip-links`, `--links` (preserve)
   - Safety: Detect and prevent symlink loops
   - Cross-platform: Handle Windows junctions/symlinks

2. **Hardlinks**
   - Detect hardlinks via inode tracking
   - Preserve hardlink relationships
   - `--hard-links` flag to enable
   - Tests: Verify inode counts match

3. **Sparse files**
   - Detect sparse files (SEEK_DATA/SEEK_HOLE on Linux/macOS)
   - Transfer only allocated blocks
   - Preserve sparseness in destination
   - Example: 10GB VM image with 2GB used → transfer 2GB

4. **Extended attributes & ACLs**
   - `-X` flag to preserve xattrs
   - `-A` flag to preserve ACLs (requires root)
   - Platform-specific: macOS resource forks, Windows alternate data streams
   - Tests: Verify xattrs/ACLs preserved

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

## Feature Comparison: sy v1.0 vs Competition

| Feature | rsync | rclone | sy v1.0 (planned) |
|---------|-------|--------|-------------------|
| **Performance (local)** | baseline | N/A | **2-11x faster** ✅ |
| **Performance (network)** | baseline | 4x with --transfers | **5-10x faster** ✅ |
| **Delta sync** | ✅ | ❌ | ✅ |
| **Parallel files** | ❌ | ✅ | ✅ |
| **Parallel chunks** | ❌ | ✅ | ✅ (Phase 8) |
| **Resume** | ✅ | ✅ | ✅ (Phase 4) |
| **Symlinks** | ✅ | ✅ | ✅ (Phase 6) |
| **Sparse files** | ✅ | ❌ | ✅ (Phase 6) |
| **ACLs/xattrs** | ✅ | ❌ | ✅ (Phase 6) |
| **Cloud storage** | ❌ | ✅ | ✅ (Phase 8) |
| **Watch mode** | ❌ | ❌ | ✅ (Phase 4) |
| **JSON output** | ❌ | ✅ | ✅ (Phase 4) |
| **Config profiles** | ❌ | ✅ | ✅ (Phase 4) |
| **Verification** | checksum | hash | **Multi-layer** ✅ (Phase 5) |
| **Beautiful output** | ❌ | ⚠️ | ✅ |

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
