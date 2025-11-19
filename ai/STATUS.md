# Status

## Current State
- Version: v0.0.60 (released) + 2 commits on main
- **Next Release Goal**: v0.0.61 (Performance & Stability)
- Test Coverage: **465 tests passing** ✅
- Feature Flags:
  - SSH: Optional (enabled by default)
  - Watch: Optional (disabled by default)
  - ACL: Optional (Linux requires libacl-dev, macOS works natively)

## v0.0.61 Release Plan (Active)

**Theme**: Scale & Stability
**Target**: Production-ready for massive directories and cloud storage.

1.  **Massive Scale Optimization** 🚀
    - **Goal**: Handle 100k+ files seamlessly.
    - **Status**: ✅ Implemented Streaming Sync (75% memory reduction: 530MB → 133MB)
    - **Tasks**: 
      - ✅ Profile memory/CPU (Done)
      - ✅ Implement `scan_streaming` (Done)
      - ✅ Implement streaming sync pipeline (`Scan -> Plan -> Execute`) (Done)

2.  **Object Store Stability (S3)** ☁️
    - **Goal**: Move from "Experimental" to "Stable".
    - **Status**: ✅ Hardening complete
    - **Tasks**: 
      - ✅ Integration tests (AWS/R2/B2)
      - ✅ Documentation & Auth patterns
      - ✅ Removed "experimental" warning

3.  **Watch Mode Polish** 👀
    - **Goal**: Reliable continuous sync.
    - **Status**: ✅ Feature flag added & robust handling implemented
    - **Tasks**: 
      - ✅ Gate `notify` behind `watch` feature
      - ✅ Decouple from SSH (enforce local source)
      - ✅ Robust error handling (auto-sync on watch errors)

4.  **Already Completed (in main)**:
    - ✅ Auto-deploy `sy-remote` (Zero-setup)
    - ✅ Optional SSH feature flag

## Blocked / Shelved
- **russh Migration**: Blocked by SSH agent auth complexity (requires ~300 LOC custom protocol). Sticking with `libssh2` for now.

## Recent Releases

### v0.0.60
- Critical memory bug fixes (streaming checksums)
- Optional ACL feature
- CI/CD infrastructure

---

## Next Up
See `ai/TODO.md` for detailed task breakdown.
