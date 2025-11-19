# Release Plan: v0.0.61

**Theme**: Scale, Cloud, and Polish
**Status**: Active Development

## Goals
1. **Scale**: Prove and optimize performance for 100,000+ file repositories.
2. **Cloud**: Stabilize S3 support for production use.
3. **UX**: Zero-setup remote sync (Auto-deploy) + Flexible builds (Optional features).

## Scope

| Feature | Status | Priority | Notes |
|---------|--------|----------|-------|
| **Auto-Deploy `sy-remote`** | ✅ Done | P0 | Merged in main. Zero config for remote servers. |
| **Optional SSH** | ✅ Done | P1 | Merged in main. Smaller binaries, fewer deps. |
| **Massive Scale Profiling** | 🔄 Todo | P1 | Target: 100k files. Optimize scan/schedule. |
| **S3 Stability** | 🔄 Todo | P1 | Validate R2/B2. Add integration tests. |
| **Watch Mode Polish** | 🔄 Todo | P2 | Make `notify` optional. Local-only watch. |

## Non-Goals
- **russh Migration**: Postponed indefinitely due to complexity.
- **Windows Support**: Still best-effort/experimental.

## Timeline
- **Phase 1**: Profiling & Benchmarks (Current)
- **Phase 2**: S3 Hardening
- **Phase 3**: Feature cleanup (Watch mode)
- **Release**: v0.0.61
