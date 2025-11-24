# Status

## Current State
- Version: v0.0.62 (released)
- **Next Release Goal**: v0.1.0 (Production Readiness)
- Test Coverage: **475 tests passing** ✅ (Cross-platform verified)
- **Current Build**: 🟢 PASSING

## Feature Flags
- SSH: Optional (enabled by default)
- Watch: Optional (disabled by default)
- ACL: Optional (Linux requires libacl-dev, macOS works natively)
- S3: Optional (disabled by default)

## Active Development (v0.1.0 Prep)
- **Refinement**: Completed major performance passes (Adler32, Parallel SSH, Adaptive Compression).
- **Quality**: Enforced stricter safety rules via Clippy.
- **Archive Mode (Issue #11)**: ✅ Fixed - Added `--no-gitignore` and `--include-vcs` flags for idiomatic control.

## Recent Releases

### v0.0.62 (Refinement)
- **Parallel Chunk Transfers**: Split huge files into concurrent 1MB chunks over SSH
- **Adaptive Compression**: Auto-disable compression on fast networks (>500Mbps)
- **Adler32 Optimization**: SIMD-accelerated checksums (7x faster)