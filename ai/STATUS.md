# Status

## Current State

| Metric | Value | Updated |
|--------|-------|---------|
| Version | v0.1.2 | 2025-11-27 |
| Tests | 620+ passing | 2025-11-27 |
| Build | 🟢 PASSING | 2025-11-27 |

## What Worked

- Bidirectional server mode (74f7c35): Push + pull over SSH using binary protocol
- Delta sync optimization: 2x faster than rsync for partial updates
- Protocol fix (66d05d5): Always send MKDIR_BATCH even if empty
- Adaptive block sizes: 2KB→64KB based on file size

## What Didn't Work

- Initial pull mode protocol: Client expected MKDIR_BATCH but server only sent it when directories existed → Fixed by always sending MKDIR_BATCH

## Active Work

v0.1.2 released. Next: Server mode Phase 5 (progress reporting, hardlinks, xattrs)

Tasks: `bd ready` or `bd list`

## Blockers

None currently.

## Recent Releases

### v0.1.2 (2025-11-27)
- Bidirectional server mode (push + pull)
- Delta sync 2x faster than rsync
- Removed ~300 lines dead bulk_transfer code

### v0.1.1 (2025-11-26)
- Batch destination scanning (~1000x fewer SSH round-trips)
- Planning phase: 90 min → 30 sec for 531K files

### v0.1.0 (2025-11-25)
- Breaking: rsync-compatible defaults
- New flags: `--gitignore`, `--exclude-vcs`, `-u/--update`

## Feature Flags

| Flag | Default | Notes |
|------|---------|-------|
| SSH | Enabled | ssh2 (libssh2) |
| Watch | Disabled | File watching |
| ACL | Disabled | Linux: libacl-dev |
| S3 | Disabled | Experimental |
