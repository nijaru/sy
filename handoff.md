# Handoff — sy v0.4.0

**Date:** 2026-06-17
**Branch:** dev
**Commit:** 488bd47 (fix: wire delta sync strategy detection into SyncSession)

## Status

**What was achieved this session:**

1. **TaskExecutor refactor** — Enhanced TaskExecutor with all execution features (hardlink tracking, xattrs, dir permissions, --partial, --itemize, --remove-source, --stats, --backup). Removed ~200 lines of duplicated code from session.rs.

2. **SyncSession refactor** — SyncSession.direct_local() now delegates to TaskExecutor via execute_batch(). Single execution path for all file operations.

3. **Delta sync integration** — Wired change ratio detection and COW/in-place strategy logging into SyncSession. Fixed 6 pre-existing test failures.

4. **CLI fixes** — Added --force-delete flag, fixed test flag mismatches (--use-cache, --compress).

5. **Testing plan** — Created 5 sprints with 50 tasks for comprehensive testing.

**Current state:**
- 549 tests passing, 0 failures, 12 ignored
- Clippy clean, 0 warnings
- 85 CLI flags, all implemented
- No stubs, no TODOs in new code

## Context

### Key Files Changed

| File | Change |
|------|--------|
| src/sync/executor.rs | Enhanced TaskExecutor with ExecuteConfig, BackupConfig, hardlink tracking (Mutex), xattr preservation, dir permissions, --partial, --itemize, --remove-source, --stats, --backup. 21 tests. |
| src/sync/session.rs | Refactored direct_local() to use TaskExecutor. Added change ratio detection and COW/in-place strategy logging. Added delete threshold enforcement. |
| src/sync/mod.rs | Made itemize_string pub(crate). Single source of truth. |
| src/cli.rs | Added --force-delete flag. 85 flags total. |
| src/main.rs | Wired --force-delete to DeleteMode.force. |
| tests/integration_test.rs | Fixed --use-cache=true → --cache=true |
| tests/sync/advanced.rs | Fixed --compress → --compress=auto |
| ai/PLAN.md | Testing plan with 5 sprints, 50 tasks |
| ai/sprints/ | Sprint details for each testing area |

### Architecture

```
SyncSession (orchestrator)
  ├── Strategy selection (direct_local, streaming_push, streaming_pull)
  ├── Filter engine (exclude, include, patterns)
  ├── Delete threshold enforcement
  ├── Change ratio detection
  ├── COW/in-place strategy selection
  └── Delegates to TaskExecutor

TaskExecutor (worker)
  ├── ExecuteConfig (hardlink, xattrs, dir perms, partial, itemize, remove-source, stats)
  ├── BackupConfig (enabled, suffix, dir)
  ├── Hardlink tracking (Mutex<HashMap<u64, PathBuf>>)
  ├── execute_file() — single file copy with all features
  ├── execute_directory() — directory creation with permission preservation
  ├── execute_symlink() — symlink creation with update handling
  ├── execute_delete() — file deletion
  └── execute_batch() — parallel/sequential execution
```

### Key Decisions

- **TaskExecutor is the single execution path** — no duplication between session and executor
- **Backup failure is fatal** — user asked for backups, they should know if they fail
- **--force-delete bypasses safety threshold** — escape hatch for 100% deletion scenarios
- **Change ratio detection at info level** — users should see why delta sync was/wasn't used
- **COW/in-place strategy at info level** — users should see which strategy was selected

## Next Steps

**Immediate (Sprint 01):**
1. Read ai/PLAN.md and ai/sprints/01-executor-tests.md
2. Start with Task 1.1: Test execute_file with hardlink tracking
3. Run `cargo test sync::executor::tests` to verify

**After Sprint 01:**
1. Sprint 02: SyncSession direct unit tests
2. Sprint 03: CLI integration tests
3. Sprint 04: Edge cases and failure modes
4. Sprint 05: Performance benchmarks and profiling

## Environment

- **Branch:** dev (ahead of main)
- **Tag:** v0.4.0-pre-executor-refactor (at commit before executor refactor)
- **Tests:** 549 passing, 0 failures, 12 ignored
- **Clippy:** Clean, 0 warnings
- **CLI Flags:** 85 (all implemented)

## Tasks (tk ls)

| ID | Priority | Status | Title |
|----|----------|--------|-------|
| tk-aswh | p2 | open | Sprint 01: TaskExecutor direct unit tests |
| tk-akpn | p2 | open | Sprint 02: SyncSession direct unit tests |
| tk-150x | p2 | open | Sprint 03: CLI integration tests |
| tk-p3rc | p2 | open | Sprint 04: Edge cases and failure modes |
| tk-6k7l | p3 | open | Sprint 05: Performance benchmarks and profiling |
| tk-8v6z | p3 | active | Phase 5: Cleanup + delete old transport |
| tk-46zd | p2 | open | Close SSH incremental sync gap |
| tk-zh3p | p3 | open | Curate library API |
| tk-oahw | p3 | open | Benchmark against rsync |
| tk-n3si | p3 | open | Profile many files (10K+) |
| tk-qhjt | p3 | open | Profile large file sync (100MB+) |
| tk-r85d | p3 | open | Restructure test suite (28 files → ~8) |

## Build & Verify

```bash
cargo build                    # Build
cargo test                     # Test (549 pass, 0 fail, 12 ignored)
cargo clippy -- -D warnings    # Lint (0 warnings)
cargo fmt --check              # Format (no changes)
```
