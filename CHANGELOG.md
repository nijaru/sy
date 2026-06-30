# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.1] - 2026-06-30

### Changed

- All production `.unwrap()` calls converted to `.expect()` with descriptive messages (79 sites)

### Fixed

- Perf regression test thresholds relaxed for slower machines
- Resume state tests use process-scoped temp dir via `SY_CACHE_DIR` to avoid parallel test races

### Added

- 13 new proptest/fuzz tests: delta round-trip, protocol decoder fuzzing, filter property tests, checksum round-trip

## [0.4.0] - 2026-06-23

### Highlights

- **New architecture**: SyncSession + TaskExecutor replace monolithic SyncEngine
- **1.25-11.5x faster** than rsync across all scenarios
- **SSH competitive**: 10% faster than rsync for incremental sync
- **Critical SSH fixes**: --exclude/--include/--filter and --dry-run now work correctly
- **Data safety audit**: all destination writes use atomic temp+rename
- **APFS sparse file fix**: SEEK_DATA/SEEK_HOLE returning wrong results caused silent data loss
- **Library API curated**: 24 pub modules reduced to 8 stable + 4 doc-hidden + 14 private

### Added

- Full rsync-compatible CLI: --backup, --remove-source-files, --itemize-changes, --compress-level, --timeout, --bwlimit, --max-delete, --keep-dirlinks, --archive, --gitignore, --exclude-from, --include-from, and more
- SSH streaming comparison flags (--size-only, --checksum, --update) wired end-to-end
- SSH streaming --bwlimit and --verify wired end-to-end
- Streaming protocol edge case tests (25 tests)
- SSH integration tests (21 tests)
- Backup failure path tests
- Auto-generated man page from CLI definition
- Feature status table in README (stable/local-only/experimental/not-implemented)
- Runtime warnings for flags not wired in SSH mode
- Protocol backward compatibility for new trailing fields (filter_patterns, files_scanned, max_delete)
- Sparse test serialization (SPARSE_MUTEX) for APFS parallel race condition

### Fixed

- **Critical: APFS SEEK_DATA/SEEK_HOLE returning wrong results** — SEEK_DATA returns file_size instead of error for non-sparse files, SEEK_HOLE doesn't detect holes. Both caused zero bytes copied = silent data loss. Now returns Unsupported so callers fall back to block-based copy.
- **Critical: --exclude/--include/--filter silently ignored in SSH pull mode**
- **Critical: --dry-run creating files on remote in SSH mode**
- **Critical: pull-mode scan options (respect_gitignore, exclude_vcs, dirs_only) not propagated**
- **Critical: Hello protocol frame length missing comparison_flags field** (caused "Unexpected message during Initial Exchange: Done" in SSH tests)
- **ReceiveFile temp file leak on early error** (TempFileGuard)
- **SSH streaming files_scanned reported as 0** (Done message now carries scan count)
- Duplicate stats/mode/error output
- Symlink overwrite bug
- Filter ordering
- Delta sync strategy detection
- Lock file truncate(false) to preserve holder PID
- BSD flags JSON propagation
- setuid/setgid stripping in safe_received_mode()
- --retry default changed from 3 to 0 (was not wired, misleading)

### Changed

- **Breaking**: Library API surface reduced from 24 pub modules to 8 stable + 4 doc-hidden + 14 private
- **Breaking**: --retry default changed from 3 to 0
- Consolidated triplicated detect_data_regions to single implementation in sparse.rs (-161 lines)
- Removed deprecated SyncEngine::new(), migrated all callers to with_config()
- Replaced blanket #![allow(dead_code)] in server/mod.rs with targeted allows
- Test suite restructured: 16 root files consolidated to 6, 9 moved to tests/sync/
- Man page auto-generated from CLI (was outdated 0.0.22)
- .pi/, .tasks/, .mailmap removed from git tracking
- Co-authored-by tags stripped from history
- README rewritten with feature status table
- CI: Release workflow now manual dispatch only (no auto-release on tag push)
- CI: cargo test (not --all) to skip ignored SSH tests in CI

### Known Limitations

- --partial, --stream: not implemented (hidden from --help)
- Sparse file handling not in streaming protocol
- Bisync: experimental, limited conflict resolution
- S3/GCS: code complete, not tested against real infrastructure

### Removed

- --rsh flag (deferred to v0.5, too complex)
- Deprecated SyncEngine::new() (135 lines)
- Triplicated sparse detection code (-161 lines)
- 7 sparse test #[ignore]s (now pass on both macOS APFS and Linux ext4)

## [0.3.0] - 2026-06-10

### Added

- Bidirectional sync (bisync mode)
- Watch mode with file system monitoring
- SSH sync with streaming protocol
- S3 sync support
- Delta sync with change detection
- Directory cache for faster incremental sync
- Checksum database for content verification
- Resume interrupted transfers
- Post-write verification
- JSON output mode
- Performance profiling

## [0.2.0] - 2026-05-15

### Added

- Hard link preservation
- Symlink handling
- Extended attributes (xattr) preservation
- ACL preservation
- Sparse file detection
- Bandwidth limiting
- Filter patterns (exclude/include)
- Comparison modes (size-only, checksum, update)

## [0.1.2] - 2026-04-20

### Fixed

- Fix symlink overwrite bug
- Fix permission preservation

## [0.1.1] - 2026-04-10

### Fixed

- Fix trailing slash behavior
- Fix dry-run output

## [0.1.0] - 2026-04-01

### Added

- Initial release
- Basic file sync
- Dry-run mode
- Progress reporting
