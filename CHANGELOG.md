# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **SyncSession architecture**: New modular sync engine replacing monolithic SyncEngine
- **TaskExecutor**: Centralized task execution with backup, xattr, hardlink support
- **--max-delete**: Supports both percentage (`--max-delete=50%`) and absolute count (`--max-delete=1000`)
- **--keep-dirlinks (-K)**: Treat symlinked directories on receiver as directories
- **--force-delete**: Bypass deletion safety threshold
- **--itemize-changes**: rsync-style `YXcstpoguax` output for each file
- **--compress (-z)**: Now works as boolean flag (rsync compatible)
- **Edge case tests**: Special characters, empty files, long filenames, concurrent sync safety

### Fixed
- Duplicate stats output when using `--stats`
- Duplicate "Mode:" line in dry-run output
- Duplicate error message for deletion threshold
- `-z` flag now works as boolean (rsync compatible)
- Test flag mismatches (`--use-cache`, `--per-file-progress`, `-z`)

### Changed
- Architecture: SyncSession + TaskExecutor replace SyncEngine for local/SSH sync
- Benchmarks: 1.25-11.5x faster than rsync across all scenarios
- README rewritten with idiomatic structure

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

### Changed
- Improved error messages
- Better progress reporting
- Faster checksum calculation (xxHash3)

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

### Changed
- Improved performance for large directories
- Better memory usage

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
