# Release Plan: v0.1.0 (Production Readiness)

**Status**: Planning

## Goals
1. **Quality**: Zero Clippy warnings, comprehensive error handling
2. **Platform**: Windows support (sparse files, ACLs, path handling)
3. **Stability**: Production validation period

## Completed (v0.0.61-v0.0.62)
- ✅ Auto-deploy `sy-remote` (zero-config remote sync)
- ✅ Optional SSH feature (minimal dependencies)
- ✅ Massive scale optimization (streaming pipeline, 75% memory reduction)
- ✅ S3 stability hardening
- ✅ Watch mode (optional feature)
- ✅ Parallel chunk transfers over SSH
- ✅ Adaptive compression (auto-disable on fast networks)
- ✅ Adler32 optimizations (7x faster)

## Active Work (v0.1.0)

| Feature | Priority | Notes |
|---------|----------|-------|
| **Clippy Cleanup** | P0 | Address remaining unwrap/expect in non-critical paths |
| **Error Handling Audit** | P0 | Ensure all errors have actionable suggestions |
| **Windows Support** | P1 | Sparse files (DeviceIoControl), ACL mapping, path edge cases |

## Future (Post-v0.1.0)
- SIMD optimization (if bottlenecks reappear)
- russh migration (blocked on SSH agent complexity)
- S3 bidirectional sync
