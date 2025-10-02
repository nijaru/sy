# AI Development Context for sy

This file provides context for AI assistants (Claude, etc.) working on the `sy` project.

## Project Overview

**sy** is a modern file synchronization tool written in Rust - a reimagining of rsync with adaptive performance, verifiable integrity, and beautiful UX.

**Status**: Design phase complete (Oct 2025), implementation starting with Phase 1 (MVP)

## Key Documents

1. **DESIGN.md** (2,400 lines) - Comprehensive technical design
   - Read this FIRST for any architectural questions
   - Contains all design decisions with rationale
   - Includes code examples and implementation details

2. **README.md** - User-facing overview
3. **CONTRIBUTING.md** - Development guidelines

## Design Philosophy

### Core Principles

1. **Verifiable, Not "Perfect"**
   - 100% reliability is physically impossible
   - Multi-layer verification (TCP → xxHash3 → BLAKE3)
   - Research: 5% of 100 Gbps transfers have corruption TCP doesn't detect

2. **Adaptive, Not One-Size-Fits-All**
   - Different strategies for local/LAN/WAN
   - Auto-detect and optimize
   - Clear modes: `--mode auto|local|lan|wan|verify|paranoid`

3. **Transparent Tradeoffs**
   - Speed vs reliability choices are explicit
   - No hidden behavior
   - Helpful error messages with fixes

### Important Technical Decisions

#### Hash Functions (DESIGN.md:79-128)
- **Adler-32**: Rolling hash for rsync algorithm (NOT replaceable)
- **xxHash3**: Block checksums (fast, non-cryptographic)
- **BLAKE3**: End-to-end verification (cryptographic)

**CRITICAL**: xxHash3 is NOT a rolling hash and cannot replace Adler-32 in delta sync.

#### Transport Protocols (DESIGN.md:252-322)
- **QUIC**: 45% SLOWER on fast networks (>600 Mbps) - don't use for LAN
- **TCP with BBR**: 2-25x faster under packet loss vs CUBIC
- **SSH ControlMaster**: 2.5x throughput boost

Decision: Custom binary protocol over SSH > SFTP > local I/O

#### Compression Thresholds (DESIGN.md:143-181)
Updated for 2024+ hardware:
- **>500 MB/s (4Gbps)**: No compression (CPU bottleneck)
- **100-500 MB/s (1-4Gbps)**: LZ4 only (400-500 MB/s compress speed)
- **<100 MB/s**: Adaptive zstd
- **Local**: NEVER compress (disk I/O bottleneck)

## Implementation Roadmap

### Current Phase: Not Started
Design complete, ready to begin Phase 1

### Phase 1: MVP (v0.1.0)
**Goal**: Basic local sync working

Tasks:
- [ ] CLI argument parsing (clap)
- [ ] Local filesystem traversal (walkdir + ignore)
- [ ] File comparison (size + mtime)
- [ ] Full file copy (no delta yet)
- [ ] Basic progress display (indicatif)
- [ ] Unit tests

**Deliverable**: `sy /src /dst` works locally

### Future Phases (2-10)
See DESIGN.md:2198-2330 for complete roadmap

## Code Organization

```
sy/
├── src/
│   ├── main.rs                 # CLI entry point
│   ├── cli.rs                  # Argument parsing
│   ├── config.rs               # Config file parsing
│   ├── sync/                   # Sync orchestration
│   ├── integrity/              # Hash functions
│   ├── transport/              # SSH/SFTP/local
│   ├── compress/               # zstd/lz4
│   ├── filter/                 # gitignore/rsync patterns
│   ├── progress/               # Progress UI
│   ├── metadata/               # Permissions/xattrs
│   ├── error.rs                # Error types
│   ├── bandwidth.rs            # Rate limiting
│   └── ssh_config.rs           # SSH config parsing
```

## Important Edge Cases

### Edge Case Categories (DESIGN.md:548-1036)

1. **Symlinks/Hardlinks** - Multiple modes, inode tracking
2. **Large Scale** - Millions of files, streaming, Bloom filters
3. **Atomic Operations** - Write to .sy.tmp, fsync, rename
4. **Cross-Platform** - Windows reserved names, case sensitivity
5. **Deletion Safety** - Multi-layer protection, thresholds
6. **Metadata** - Permissions/xattrs/ACLs (requires root)
7. **Sparse Files** - VM images, SEEK_DATA/SEEK_HOLE
8. **TOCTOU** - Concurrent modification detection

### Common Pitfalls to Avoid

❌ **Don't** use xxHash3 for rolling hash (use Adler-32)
❌ **Don't** assume QUIC is faster (it's slower on fast networks)
❌ **Don't** compress on local or >4Gbps networks
❌ **Don't** load entire file tree into memory (stream it)
❌ **Don't** trust mtime alone (needs tolerance windows)

✅ **Do** use block-level checksums (not just file-level)
✅ **Do** handle sparse files specially (SEEK_DATA/SEEK_HOLE)
✅ **Do** atomic operations (write temp, verify, rename)
✅ **Do** check resources before starting (disk space, FDs)

## Testing Strategy

### Test Types (DESIGN.md:1700-1743)

1. **Unit Tests**: Hash correctness, compression selection, filter matching
2. **Integration Tests**: Full sync scenarios, resume, metadata preservation
3. **Property Tests**: Idempotence, compression roundtrip, filter ordering
4. **Stress Tests**: Millions of files, huge sparse files, deep hierarchies
5. **Benchmarks**: Hash speed, compression, parallel vs sequential

### Required Before Merging

```bash
cargo test                      # All tests pass
cargo clippy -- -D warnings     # No warnings
cargo fmt -- --check            # Formatted
cargo bench                     # Benchmarks (if perf change)
```

## Dependencies

### Core Crates (DESIGN.md:2095-2144)

**CLI & Config**:
- `clap` (derive + env features) - CLI parsing
- `toml` - Config files
- `serde` - Serialization

**Async**:
- `tokio` (full features) - Async runtime
- `futures` - Async utilities

**Hashing**:
- `xxhash-rust` - xxHash3
- `blake3` (rayon features) - BLAKE3

**Compression**:
- `zstd` - Zstandard
- `lz4-flex` - LZ4

**SSH/Network**:
- `russh` - SSH protocol
- `russh-sftp` - SFTP
- `ssh-config` - SSH config parsing

**Filesystem**:
- `walkdir` - Directory traversal
- `ignore` - Gitignore support
- `filetime` - Timestamp manipulation
- `xattr` - Extended attributes

**Progress & Logging**:
- `indicatif` - Progress bars
- `tracing` + `tracing-subscriber` - Structured logging

**Error Handling**:
- `anyhow` - CLI errors
- `thiserror` - Library errors

**Utilities**:
- `dashmap` - Concurrent HashMap
- `rayon` - Parallel iterators
- `once_cell` - Lazy statics

## Security Considerations (DESIGN.md:2148-2194)

### Threat Model

**In Scope**:
- Data integrity (checksums)
- MITM (SSH encryption)
- DoS (bandwidth limiting, resource checks)
- Path traversal (sanitize paths)
- Symlink attacks (configurable handling)

**Out of Scope (v1.0)**:
- Encryption at rest (use LUKS/dm-crypt)
- Authentication (delegated to SSH)
- Authorization (filesystem permissions)
- Side-channel attacks

### Safe Defaults

```rust
symlink_mode: SymlinkMode::IgnoreUnsafe,  // Don't follow outside tree
delete: false,                             // Never delete by default
verify_checksums: mode != Mode::Fast,      // Verify unless fast mode
```

## Common Development Scenarios

### Adding a New Feature

1. Check DESIGN.md to see if already specified
2. If not designed, discuss design first (don't jump to code)
3. Update DESIGN.md with rationale
4. Implement with tests
5. Update CONTRIBUTING.md if needed

### Performance Work

1. Benchmark BEFORE making changes
2. Profile with `cargo flamegraph` to find bottleneck
3. Make targeted change
4. Benchmark AFTER
5. Document improvement in commit message

### Fixing a Bug

1. Write failing test first
2. Fix bug
3. Verify test passes
4. Check if design assumptions were wrong
5. Update DESIGN.md if architectural issue

## AI Assistant Guidelines

### When Asked to Implement

1. **Read DESIGN.md section first** - Don't guess at architecture
2. **Follow the roadmap** - Don't jump ahead (we're on Phase 1)
3. **Write tests** - No implementation without tests
4. **Match code examples** - DESIGN.md has implementation patterns
5. **Ask if unclear** - Better to clarify than assume

### When Asked About Design

1. **Cite DESIGN.md line numbers** - E.g., "See DESIGN.md:252-322"
2. **Include rationale** - Not just "what" but "why"
3. **Reference research** - E.g., "QUIC 45% slower (ACM 2024)"

### When Suggesting Changes

1. **Explain tradeoffs** - Speed vs reliability, memory vs CPU, etc.
2. **Provide benchmarks** - Or explain how to benchmark
3. **Update DESIGN.md** - Design decisions should be documented

## Quick Reference

### Find Information About...

- **Hashing**: DESIGN.md:79-128
- **Transport**: DESIGN.md:252-322
- **Compression**: DESIGN.md:132-181
- **Filters**: DESIGN.md:1059-1129
- **Bandwidth**: DESIGN.md:1132-1197
- **Progress**: DESIGN.md:1201-1267
- **SSH**: DESIGN.md:1271-1361
- **Errors**: DESIGN.md:1365-1483
- **Logging**: DESIGN.md:1487-1549
- **Edge Cases**: DESIGN.md:548-1036
- **Roadmap**: DESIGN.md:2198-2330

### Research Papers Referenced

- Jeff Geerling (2025) - rclone vs rsync benchmarks
- ACM 2024 - "QUIC is not Quick Enough over Fast Internet"
- ScienceDirect 2021 - File transfer corruption studies
- Multiple - rsync algorithm, hash performance, compression

### Example Commands (Once Implemented)

```bash
sy ./src remote:/dest                    # Basic sync
sy ./src remote:/dest --dry-run          # Preview
sy ./src remote:/dest --mode lan         # LAN optimization
sy ./src remote:/dest --verify           # Cryptographic checksums
sy backup-home                           # Named config profile
```

## Notes for Future Sessions

- Design is **complete** - don't redesign without good reason
- Phase 1 is **next** - focus on MVP, not advanced features
- Research is **done** - 2024/2025 data already incorporated
- Tests are **required** - no merging without tests

---

**Last Updated**: October 2025 (Design phase complete)
**Current Phase**: Ready to begin Phase 1 (MVP)
**Total Design**: 2,400+ lines in DESIGN.md
