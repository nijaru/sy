# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-06-21

### Highlights

- **New architecture**: SyncSession + TaskExecutor replace monolithic SyncEngine
- **1.25-11.5x faster** than rsync across all scenarios
- **SSH competitive**: 10% faster than rsync for incremental sync
- **Critical SSH fixes**: --exclude/--include/--filter and --dry-run now work correctly
- **Data safety audit**: all destination writes use atomic temp+rename

### Added

- Full rsync-compatible CLI: --backup, --remove-source-files, --itemize-changes, --compress-level, --timeout, --bwlimit, --max-delete, --keep-dirlinks, --archive, --gitignore, --exclude-from, --include-from, and more
- Streaming protocol edge case tests (25 tests)
- SSH integration tests (19 tests)
- Backup failure path tests
- Auto-generated man page from CLI definition
- Feature status table in README (stable/local-only/experimental/not-implemented)
- Runtime warnings for flags not wired in SSH mode
- Protocol backward compatibility for new trailing fields (filter_patterns, files_scanned, max_delete)

### Fixed

- **Critical: --exclude/--include/--filter silently ignored in SSH pull mode**
- **Critical: --dry-run creating files on remote in SSH mode**
- **Critical: pull-mode scan options (respect_gitignore, exclude_vcs, dirs_only) not propagated**
- **ReceiveFile temp file leak on early error** (TempFileGuard)
- **SSH streaming files_scanned reported as 0** (Done message now carries scan count)
- Duplicate stats/mode/error output
- Symlink overwrite bug
- Filter ordering
- Delta sync strategy detection
- Lock file truncate(false) to preserve holder PID
- BSD flags JSON propagation
- setuid/setgid stripping in safe_received_mode()

### Changed

- Man page auto-generated from CLI (was outdated 0.0.22)
- .pi/, .tasks/, .mailmap removed from git tracking
- Co-authored-by tags stripped from history
- README rewritten with feature status table

### Known Limitations

- --bwlimit, --checksum, --update, --existing, --ignore-times, --ignore-existing, --verify: work for local sync, not wired in SSH streaming mode
- --partial, --stream, --retry: not implemented (hidden from --help)
- Sparse file handling not in streaming protocol
- Bisync: experimental, limited conflict resolution
- S3/GCS: code complete, not tested against real infrastructure

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
