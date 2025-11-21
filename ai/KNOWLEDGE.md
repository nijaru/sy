# Codebase Knowledge

**Purpose:** Permanent quirks, gotchas, and non-obvious behavior in the sy codebase

| Area | Knowledge | Why | References |
|------|-----------|-----|------------|
| **Hashing** | xxHash3 is NOT a rolling hash | Cannot replace Adler-32 in delta sync algorithm. Different purposes: xxHash3 for blocks, Adler-32 for rolling window | src/integrity/, DECISIONS.md (Hash Function Selection) |
| **Networking** | QUIC is slower on fast networks | 45% performance regression on >600 Mbps connections. Use TCP with BBR instead | RESEARCH.md (QUIC Network Performance) |
| **Compression** | Compression overhead on fast networks | CPU bottleneck on >4Gbps connections. Never compress local sync | src/compress/, DECISIONS.md (Compression Strategy) |
| **Filesystems** | COW and hard links interaction | Hard links MUST use in-place strategy. COW cloning breaks link semantics (nlink > 1) | src/fs_util.rs, DECISIONS.md (COW-Aware Sync) |
| **Sparse Files** | Filesystem-dependent support | Not all filesystems support SEEK_HOLE/SEEK_DATA. Tests verify correctness, log whether sparseness preserved | src/transport/local.rs, tests/delta_sync_test.rs |
| **Memory** | sy-remote protocol buffering | Requires buffering entire compressed payload. Files >256MB use SFTP instead (chunks internally) | src/compress/mod.rs, DECISIONS.md (Critical Bug Fixes) |
| **S3** | Multipart threshold | AWS S3 minimum part size is 5MB. Small files use simple put, large files use multipart upload | src/transport/s3.rs |
| **SSH** | russh migration blocker | SSH agent authentication requires ~300 LOC custom protocol implementation. Using ssh2 (libssh2) until resources allow | feature/russh-migration branch, DECISIONS.md |

---

**Note:**
- For temporary issues or current blockers → use STATUS.md
- For architectural decisions and rationale → use DECISIONS.md
- This file is for permanent, non-obvious behavior that won't change
