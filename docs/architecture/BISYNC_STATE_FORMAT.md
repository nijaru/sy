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
# sy bisync v2
# sync_pair: <source_path> <-> <dest_path>
# last_sync: <ISO8601 timestamp>
```

### Entry Format (v2)
Each line represents one file's state:
```
<side> <mtime_ns> <size> <checksum> <last_sync_ns> "<escaped_path>"
```

Fields:
- **side**: `source` or `dest`
- **mtime_ns**: File modification time in nanoseconds since UNIX epoch (i64)
- **size**: File size in bytes (u64)
- **checksum**: xxHash3 checksum (hex string) or `-` if not computed
- **last_sync_ns**: Last sync time in nanoseconds since UNIX epoch (i64)
- **escaped_path**: Quoted and escaped path (handles special characters)

### Escape Sequences
All paths are quoted and special characters are escaped:
- `\\` → Backslash
- `\"` → Quote
- `\n` → Newline
- `\t` → Tab
- `\r` → Carriage return

### Example
```
# sy bisync v2
# sync_pair: /home/user/docs <-> /backup/docs
# last_sync: 2025-10-26T09:30:15.123456789Z
source 1729761015123456789 4096 abc123def456 1729761100000000000 "file.txt"
dest 1729761015123456789 4096 abc123def456 1729761100000000000 "file.txt"
source 1729761020000000000 2048 - 1729761100000000000 "file with spaces.txt"
dest 1729761018000000000 2048 - 1729761100000000000 "file\"with\"quotes.txt"
source 1729761025000000000 1024 - 1729761100000000000 "file\nwith\nnewlines.txt"
```

### Backward Compatibility (v1)
v2 format readers can load v1 files (5 fields instead of 6):
```
<side> <mtime_ns> <size> <checksum> <path>
```

When loading v1 format, `last_sync_ns` defaults to `mtime_ns`.

## Advantages Over SQLite

1. **Simplicity**: Plain text, no SQL, no schema migrations
2. **Debuggability**: `cat ~/.cache/sy/bisync/*.lst` shows full state
3. **Fewer dependencies**: No additional database dependencies
4. **Atomic writes**: Write to `.tmp` + rename for consistency
5. **Proven approach**: rclone bisync uses similar format successfully
6. **Human-readable**: Easy to inspect and understand state
7. **Robust escaping**: Handles all special characters correctly

## Format Version

Current format version is **v2** (introduced in sy v0.0.45).

Changes from v1:
- Added `last_sync_ns` field (separate from `mtime_ns`)
- All paths now properly escaped and quoted
- Robust error handling on parse failures
- Backward compatible with v1 files

## Error Handling

v2 format includes proper error handling:
- Parse errors are reported with line numbers
- Malformed data returns errors (no silent corruption)
- Invalid timestamps/checksums are detected and reported

## Implementation Notes

- Files are sorted by path for consistent ordering
- Blank lines and `#` comments are ignored when parsing
- Paths are quoted using standard escaping (backslash for quotes)
- Format is UTF-8 encoded
- File writes are atomic (write temp + rename)
