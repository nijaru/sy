# Modernization Roadmap - sy v0.1.0 → v1.0

**Status**: Planning (2025-10-06)
**Current Version**: v0.0.10
**Goal**: Make sy a complete modern rsync replacement for 90%+ of use cases

---

## Executive Summary

**sy is already production-ready for developers** (2-11x faster than rsync), but has critical gaps preventing it from being a complete replacement:

### Critical Gaps for v1.0
1. **Resume support** - Can't recover from interrupted transfers (BLOCKER)
2. **Symlinks** - Very common in real-world usage (HIGH)
3. **Sparse files** - VM images, databases (MEDIUM)
4. **Extended attributes** - Full backup fidelity (MEDIUM)

### Modern Features Missing
1. **Watch mode** - Continuous sync (modern dev workflow)
2. **JSON output** - Scriptability (standard in modern tools)
3. **Config profiles** - Reusable configurations
4. **Hooks** - Pre/post sync extensibility
5. **Cloud backends** - S3/R2/Backblaze (rclone territory)

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
❌ **JSON output** - Missing
❌ **Watch mode** - Missing
❌ **Config files** - Missing (designed but not implemented)
❌ **Extensibility** - Missing (no hooks)

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

### Phase 4 (v0.1.0) - Critical Reliability 🔴 **PRIORITY**
**Goal**: Make sy safe for production use

**Timeline**: 2-3 weeks

**Features**:
1. **Resume support** (CRITICAL)
   - State file: `.sy-state.json` in destination
   - Checkpoint every N files
   - Resume from last checkpoint on interruption
   - Verify partial files with checksums
   - Tests: Interrupt at 25%, 50%, 75% and resume

2. **Watch mode** (MODERN DEV WORKFLOW)
   - `sy /src /dst --watch` - Continuous sync
   - File system events (notify-rs)
   - Debouncing (500ms default, configurable)
   - Graceful shutdown on Ctrl+C
   - Tests: Create/modify/delete detection

3. **JSON output** (SCRIPTABILITY)
   - `sy /src /dst --json` - Machine-readable
   - One JSON object per line (NDJSON)
   - Schema: `{"type": "create|update|delete", "path": "...", "size": 123, ...}`
   - Tests: Parse and validate JSON

4. **Config profiles** (DESIGNED BUT NOT IMPLEMENTED)
   - `~/.config/sy/config.toml`
   - Named profiles: `sy --profile deploy-prod`
   - Overridable with CLI flags
   - Example:
     ```toml
     [profiles.deploy-prod]
     source = "./dist"
     destination = "user@prod:/var/www"
     exclude = ["*.map", "*.log"]
     delete = true
     ```

**Deliverable**: sy is safe and convenient for daily use

---

### Phase 5 (v0.2.0) - Verification & Reliability
**Goal**: Multi-layer integrity verification

**Timeline**: 2 weeks

**Features**:
1. **BLAKE3 end-to-end verification**
   - Optional cryptographic checksums (slower but verifiable)
   - `--verify` flag enables BLAKE3
   - Per-file verification after transfer
   - Paranoid mode: verify every block

2. **Verification modes**
   - `--mode fast` - Size + mtime only (current default)
   - `--mode standard` - + xxHash3 checksums
   - `--mode verify` - + BLAKE3 end-to-end
   - `--mode paranoid` - BLAKE3 + verify on read/write

3. **Crash recovery**
   - Transaction log for multi-file operations
   - Rollback incomplete operations
   - Detect corrupted state files
   - Self-healing: re-verify checksums

4. **Atomic operations**
   - Already implemented (write to temp, rename)
   - Document and test thoroughly
   - Add `--no-atomic` for filesystems without rename

**Deliverable**: Verifiable integrity for critical data

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

**Estimated**: 15.5-18.5 weeks (~4 months)

- Phase 4: 2-3 weeks (Critical)
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

1. **Review this plan** - Is the prioritization correct?
2. **Phase 4 implementation** - Start with resume support
3. **Community feedback** - Post roadmap, get input on priorities
4. **Beta testing** - Find early adopters after Phase 4

---

**Last Updated**: 2025-10-06
**Status**: Planning complete, ready to begin Phase 4
**Current Version**: v0.0.10 (Phases 1-3 complete)
