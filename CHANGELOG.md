# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-06-18

### Highlights

- **New architecture**: SyncSession + TaskExecutor replace monolithic SyncEngine
- **1.25-11.5x faster** than rsync across all scenarios
- **SSH competitive**: 10% faster than rsync for incremental sync
- **Full rsync compatibility**: 85 flags, all implemented (no stubs)
- **Critical SSH fixes**: --exclude/--include/--filter and --dry-run now work correctly

### Added

- Full rsync-compatible CLI (85 flags): --backup, --partial, --remove-source-files, --itemize-changes, --compress-level, --timeout, --bwlimit, --max-delete, --keep-dirlinks, --archive, --gitignore, and many more
- Streaming protocol edge case tests (25 tests)
- SSH integration tests (14 tests, were stubs)
- Backup failure path tests
- Auto-generated man page from CLI definition

### Fixed

- **Critical: --exclude/--include/--filter silently ignored in SSH mode**
- **Critical: --dry-run creating files on remote in SSH mode**
- Duplicate stats/mode/error output
- Symlink overwrite bug
- Filter ordering
- Delta sync strategy detection

### Changed

- Man page auto-generated from CLI (was outdated 0.0.22)
- .pi/, .tasks/, .mailmap removed from git tracking
- Co-authored-by tags stripped from history
- README rewritten

### Removed

- --rsh flag (deferred to v0.5, too complex)

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
