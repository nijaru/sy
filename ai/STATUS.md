# Status

## Current State
- Version: v0.0.62 (released)
- **Next Release Goal**: v0.1.0 (Production Readiness)
- Test Coverage: **475 tests passing** ✅ (Cross-platform verified)
- Feature Flags:
  - SSH: Optional (enabled by default)
  - Watch: Optional (disabled by default)
  - ACL: Optional (Linux requires libacl-dev, macOS works natively)
  - S3: Optional (disabled by default)

## Active Development (v0.1.0 Prep)
- **Refinement**: Completed major performance passes (Adler32, Parallel SSH, Adaptive Compression).
- **Quality**: Enforced stricter safety rules via Clippy.

## Recent Releases

### v0.0.62 (Refinement)
- **Parallel Chunk Transfers**: Split huge files into concurrent 1MB chunks over SSH
- **Adaptive Compression**: Disable compression on fast networks (>500Mbps)
- **Adler32 Speedup**: 7x faster static hash, 1.85x faster rolling hash
- **Safety**: Removed critical unwraps, added `clippy.toml`

### v0.0.61 (Scale & Stability)
- **Massive Scale**: Streaming sync pipeline (75% memory reduction)
- **S3 Stability**: Hardened transport, verified AWS/R2/B2 compatibility
- **Watch Mode**: Optional feature, robust error handling
- **Remote**: Auto-deploy `sy-remote` binary

---

## Next Up
See `ai/TODO.md` for detailed task breakdown.
