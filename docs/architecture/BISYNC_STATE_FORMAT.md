# Bisync State File Format

## Overview

sy bisync uses a text-based file format for storing synchronization state, inspired by rclone bisync but optimized for sy's needs.

## Format Specification

### File Location
```
~/.cache/sy/bisync/<hash>.lst
```

Where `<hash>` is a deterministic hash of the source and destination paths.

### Header Format
```
# sy bisync v1
# sync_pair: <source_path> <-> <dest_path>
# last_sync: <ISO8601 timestamp>
```

### Entry Format
Each line represents one file's state:
```
<side> <mtime_ns> <size> <checksum> <path>
```

Fields:
- **side**: `source` or `dest`
- **mtime_ns**: Modification time in nanoseconds since UNIX epoch (i64)
- **size**: File size in bytes (u64)
- **checksum**: xxHash3 checksum (hex string) or `-` if not computed
- **path**: Relative path, quoted if contains spaces/special chars

### Example
```
# sy bisync v1
# sync_pair: /home/user/docs <-> /backup/docs
# last_sync: 2025-10-24T09:30:15.123456789Z
source 1729761015123456789 4096 abc123def456 file.txt
dest 1729761015123456789 4096 abc123def456 file.txt
source 1729761020000000000 2048 - "file with spaces.txt"
dest 1729761018000000000 2048 - "file with spaces.txt"
```

## Advantages Over SQLite

1. **Simplicity**: Plain text, no SQL, no schema migrations
2. **Debuggability**: `cat ~/.cache/sy/bisync/*.lst` shows full state
3. **Fewer dependencies**: No rusqlite + libsqlite3-sys
4. **Atomic writes**: Write to `.tmp` + rename for consistency
5. **Proven approach**: rclone bisync uses similar format successfully
6. **Human-readable**: Easy to inspect and understand state

## Format Version

The format version is `v1`. Future versions will increment this number and maintain backward compatibility where possible.

## Implementation Notes

- Files are sorted by path for consistent ordering
- Blank lines and `#` comments are ignored when parsing
- Paths are quoted using standard escaping (backslash for quotes)
- Format is UTF-8 encoded
- File writes are atomic (write temp + rename)
