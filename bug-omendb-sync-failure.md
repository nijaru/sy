# Bug Report: I/O Error When Syncing omendb Directory

**Date**: 2025-10-31
**sy version**: v0.0.55
**Platform**: macOS (M3 Max) → Fedora (via Tailscale)

## Issue

`sy` fails with "No such file or directory" error when attempting to sync the omendb directory, even after target directory is created.

## Command

```bash
cd /Users/nick/github/omendb
sy omendb nick@fedora:/home/nick/github/omendb/omendb
```

## Error Output

```
Error: I/O error: No such file or directory (os error 2)

Caused by:
    No such file or directory (os error 2)

sy v0.0.55
Syncing omendb → nick@fedora:/home/nick/github/omendb/omendb
INFO SSH connection pool initialized with 10 connections
INFO Starting sync: omendb → /home/nick/github/omendb/omendb
INFO Found 435 items in source
```

## Reproduction Steps

1. Source directory: `/Users/nick/github/omendb/omendb` (exists, contains 435 items)
2. Target created: `ssh nick@fedora 'mkdir -p /home/nick/github/omendb/omendb'`
3. Run: `sy omendb nick@fedora:/home/nick/github/omendb/omendb`
4. Error occurs after finding source items but before syncing

## Attempted Workarounds

All failed with same error:
- Creating parent directory first: `mkdir -p /home/nick/github/omendb`
- Creating exact target directory: `mkdir -p /home/nick/github/omendb/omendb`
- Using parent directory as target: `sy omendb nick@fedora:/home/nick/github/omendb/`
- Removing old `omen` directory from target

## Environment

**Source:**
- Path: `/Users/nick/github/omendb/omendb`
- Items: 435 files/directories
- No broken symlinks detected

**Target:**
- Host: `nick@fedora` (Tailscale)
- Path: `/home/nick/github/omendb/omendb`
- Directory created before sync
- Old `omen` directory removed

**SSH:**
- Connection pool: 10 connections
- SSH works (manual commands succeed)

## Observations

1. Error occurs **after** "Found 435 items in source" message
2. No indication which specific file/directory causes the error
3. Same error regardless of target directory variations
4. `rsync` workaround was attempted (interrupted by user)

## Expected Behavior

Should sync all 435 items from source to target, creating/updating/deleting as needed.

## Actual Behavior

Fails with generic I/O error after finding source items but before transferring.

## Additional Context

This is a Rust project directory containing:
- Cargo workspace
- Multiple crates
- Git repository
- `.gitignore` files
- Build artifacts (target/)

May contain special files, symlinks, or permission issues that sy doesn't handle gracefully.

## Suggested Investigation

1. Add verbose error reporting showing which file/path causes the error
2. Check if issue is with source scanning vs target writing
3. Test handling of:
   - Git directories
   - Build artifacts
   - macOS extended attributes
   - Permission differences between macOS and Linux
