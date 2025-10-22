# macOS-Specific File Synchronization Features

**Research Date**: 2025-10-22
**Target Versions**: macOS 12-15 (Monterey through Sequoia)
**Platform**: Apple Silicon (M1-M4) and Intel x86_64
**Status**: Implementation plan for v0.0.41+

## Executive Summary

sy already supports most macOS-specific features through its extended attributes implementation (`-X` flag). The highest-value addition is **BSD file flags** (hidden, immutable, nodump, etc.), which are not currently preserved even with full archive mode.

## Feature Analysis

### 1. BSD File Flags ⭐ HIGH PRIORITY

**Status**: ❌ NOT IMPLEMENTED
**Priority**: HIGH (commonly requested, missing feature)
**Effort**: Medium (200 lines, 1-2 days)

**Common BSD Flags**:

| Flag | Purpose | Permission Required |
|------|---------|---------------------|
| **hidden** | Hide from Finder GUI | Owner |
| **uchg** (user immutable) | User can't modify | Owner |
| **schg** (system immutable) | Root can't modify | Super-user |
| **nodump** | Skip in backups | Owner |
| **archived** | Backup marker | Super-user |
| **uappnd** (user append-only) | Only append | Owner |
| **sappnd** (system append-only) | Only append | Super-user |
| **opaque** | Opaque in union mounts | Owner |

**Why Important**:
1. Hidden files - Users expect hidden files to stay hidden after sync
2. Immutable files - Important for system integrity
3. Backup exclusion - nodump flag is standard for backup tools
4. Archive mode - sy's `-a` should preserve all file state including flags

**Implementation Approach**:
```rust
#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;

// 1. Add to FileEntry struct (src/sync/mod.rs)
pub struct FileEntry {
    // ... existing fields ...
    #[cfg(target_os = "macos")]
    pub bsd_flags: Option<u32>,
}

// 2. Capture during scan (src/sync/scanner.rs)
#[cfg(target_os = "macos")]
fn get_bsd_flags(metadata: &Metadata) -> Option<u32> {
    Some(metadata.flags())
}

// 3. Apply during transfer (src/transport/local.rs)
#[cfg(target_os = "macos")]
fn set_bsd_flags(path: &Path, flags: u32) -> Result<()> {
    use libc::chflags;
    use std::ffi::CString;

    let c_path = CString::new(path.to_str().unwrap())?;
    let result = unsafe { chflags(c_path.as_ptr(), flags as _) };

    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}
```

**CLI Integration**:
- Add `--preserve-flags` / `-F` flag
- Include in `-a` archive mode (full fidelity)
- Add `--force-change` flag to handle immutable files

**Testing Requirements**:
- Test hidden flag preservation
- Test immutable flag handling
- Test nodump flag
- Verify super-user flags fail gracefully when not root

**rsync Comparison**:
- rsync uses `--fileflags` flag (FreeBSD/macOS)
- rsync has `--force-change` to handle immutable files
- sy should match rsync behavior for compatibility

### 2. Finder Tags and Colors ✅ FULLY SUPPORTED

**Status**: ✅ Already working with `-X` flag
**Priority**: LOW (enhancement only)
**Effort**: Low (30-50 lines for verbose display)

**Technical Details**:
- Storage: `com.apple.metadata:_kMDItemUserTags` (primary)
- Also: `com.apple.FinderInfo` (legacy, 32 bytes exactly)
- Format: XML plist with array of strings
- Each tag: `"TagName\nColorNumber"` where color is 0-7
- Colors: 0=None, 1=Gray, 2=Green, 3=Purple, 4=Blue, 5=Yellow, 6=Red, 7=Orange

**Current Behavior**:
- Both xattrs preserved transparently with `-X` flag
- No parsing or special handling needed
- Works perfectly as-is

**Optional Enhancement** (verbose mode):
```rust
// Decode Finder tags for display
fn parse_finder_tags(xattr_value: &[u8]) -> Result<Vec<(String, u8)>> {
    // Parse plist XML to extract tag names and colors
    let plist = plist::from_bytes(xattr_value)?;
    // Return Vec<(tag_name, color_number)>
}
```

**Recommendation**: Document that it works, optionally add verbose display later.

### 3. Resource Forks ✅ FULLY SUPPORTED

**Status**: ✅ Already working with `-X` flag
**Priority**: LOW (rarely relevant in 2025)
**Effort**: Low (10-20 lines for detection warning)

**Technical Details**:
- Storage: Extended attribute `com.apple.ResourceFork`
- Access: `filename/..namedfork/rsrc` (legacy path)
- Modern usage: Minimal (legacy from Classic Mac OS)

**Still Used By**:
- Some older applications (pre-2010)
- Icon files (.icns archives)
- Some plists with embedded resources
- Rarely: custom metadata in creative apps

**Not Used By**:
- Modern macOS apps (use bundles instead)
- Command-line tools
- Most user documents (2024+ files)

**Current Behavior**:
- Resource fork preserved as `com.apple.ResourceFork` xattr
- Works transparently with `-X` flag
- No special fork-aware code needed

**Optional Enhancement** (verbose mode):
```rust
#[cfg(target_os = "macos")]
fn has_resource_fork(path: &Path) -> bool {
    xattr::list(path)
        .ok()
        .map(|attrs| attrs.any(|a| a == "com.apple.ResourceFork"))
        .unwrap_or(false)
}
```

**Recommendation**: Document that it works, optionally warn in verbose mode.

### 4. Extended Attributes ✅ FULLY SUPPORTED

**Status**: ✅ Already working with `-X` flag
**Priority**: MEDIUM (enhancement - selective preservation)
**Effort**: Low (20-30 lines)

**Common Extended Attributes**:

| Attribute | Purpose | Should Preserve? |
|-----------|---------|------------------|
| **com.apple.FinderInfo** | Finder metadata (32 bytes) | ✅ YES |
| **com.apple.ResourceFork** | Resource fork data | ✅ YES |
| **com.apple.metadata:_kMDItemUserTags** | Finder tags/colors | ✅ YES |
| **com.apple.metadata:_kMDItemWhereFroms** | Download source URL | ⚠️ MAYBE |
| **com.apple.quarantine** | Gatekeeper security flag | ⚠️ CAREFUL |
| **com.apple.provenance** | File tracking/lineage | ℹ️ AUTO-GENERATED |
| **com.apple.rootless** | SIP protection marker | ❌ NO (system) |

**Current Behavior**:
- All xattrs preserved with `-X` flag (no filtering)
- Quarantine flag copied as-is (may cause "downloaded from internet" warnings)
- Auto-generated attrs copied (unnecessary but harmless)

**Optional Enhancement** (`--no-quarantine` flag):
```rust
#[cfg(target_os = "macos")]
fn should_preserve_xattr(name: &str, strip_quarantine: bool) -> bool {
    match name {
        "com.apple.quarantine" => !strip_quarantine,
        "com.apple.provenance" => false, // Auto-generated
        "com.apple.rootless" => false,   // System-protected
        _ => true,
    }
}
```

**Recommendation**: Add `--no-quarantine` flag for convenience (v0.0.42+).

### 5. iCloud Drive Placeholders ⚠️ NOT SUPPORTED

**Status**: ⚠️ Edge case, complex
**Priority**: LOW (niche use case)
**Effort**: High (200+ lines, macOS APIs)

**Technical Details**:
- Files can be in three states:
  1. **Downloaded** (local) - sy works fine
  2. **In-cloud only** (.icloud placeholder files) - sy copies placeholder, not content
  3. **Evicted** (recently downloaded but evictable) - sy works fine

**Placeholder Format**:
- Filename: `originalname.icloud`
- Content: Small XML plist with download metadata
- Size: ~1-2KB (not actual file size)

**Challenges**:
- Reading placeholder triggers download (slow, may fail)
- Requires macOS File Provider APIs
- Complex error handling for network failures
- May download GBs unexpectedly

**Current Behavior**:
- sy copies .icloud placeholder files as-is (wrong)
- No special detection or handling
- Users must download iCloud files before sync

**Recommendation**: Document limitation, don't implement (users can use iCloud sync or download first).

### 6. File Provider Extensions ⚠️ NOT SUPPORTED

**Status**: ⚠️ Complex, low priority
**Priority**: LOW (edge case)
**Effort**: High (complex detection)

**What They Are**:
- macOS API for cloud storage integration
- Dropbox, Google Drive, OneDrive use this
- Files appear local but may be virtual/on-demand

**Impact on sy**:
- Reading a file may trigger download (slow)
- Files may disappear between scan and transfer
- Need error handling for "file vanished" scenarios

**Current Behavior**:
- sy attempts to read virtual files (may trigger download)
- Standard error handling if file vanishes
- Works but may be slow for virtual files

**Recommendation**: Document behavior, add error handling for ENOENT during transfer.

## Priority Ranking

### HIGH Priority (v0.0.41)

1. **BSD File Flags** (`--preserve-flags`)
   - Value: High (commonly expected, part of full backup)
   - Cost: Medium (200 lines, 1-2 days)
   - Impact: Preserves hidden status, immutability, backup markers
   - Implementation: Add field to FileEntry, capture in scanner, restore in transport

### MEDIUM Priority (v0.0.42+)

2. **Quarantine Stripping** (`--no-quarantine`)
   - Value: Medium (security/convenience tradeoff)
   - Cost: Low (20-30 lines)
   - Impact: Avoid "downloaded from internet" warnings
   - Implementation: Filter xattr during copy

3. **Verbose Metadata Display**
   - Value: Low (UX improvement)
   - Cost: Low (50 lines total)
   - Impact: Show Finder tags, resource forks, flags in verbose mode
   - Implementation: Parse xattrs/flags for display

### LOW Priority (Defer or Document)

4. **iCloud Drive Handling** (`--icloud-download`)
   - Value: Low (niche use case)
   - Cost: High (200+ lines, macOS APIs)
   - Impact: Handle .icloud placeholders
   - Recommendation: Document limitation

5. **File Provider Awareness**
   - Value: Low (edge case)
   - Cost: Medium (detection, error handling)
   - Impact: Better handling of virtual files
   - Recommendation: Improve error messages

## Implementation Plan

### Phase 1: BSD File Flags (v0.0.41)

**Steps**:
1. Add `bsd_flags: Option<u32>` to FileEntry struct
2. Capture flags in scanner using `MetadataExt::flags()`
3. Add `--preserve-flags` / `-F` CLI flag
4. Include flags preservation in `-a` archive mode
5. Implement `set_bsd_flags()` in local transport
6. Handle immutable flags (--force-change logic):
   - Detect uchg/schg flags
   - Temporarily clear flags before modification
   - Restore flags after modification
7. Add tests:
   - Hidden flag preservation
   - Immutable file handling
   - nodump flag
   - Super-user flags (graceful failure)
8. Update documentation:
   - README: Document --preserve-flags
   - MACOS_SUPPORT.md: Add BSD flags section
   - TROUBLESHOOTING.md: Immutable file issues

**Estimated Time**: 1-2 days

### Phase 2: Quarantine Handling (v0.0.42)

**Steps**:
1. Add `--no-quarantine` flag
2. Filter `com.apple.quarantine` in xattr copy when flag set
3. Add test for quarantine stripping
4. Document in README

**Estimated Time**: 2-3 hours

### Phase 3: Verbose Enhancements (v0.0.43+)

**Steps**:
1. Parse and display Finder tags in verbose mode
2. Warn about resource forks in verbose mode
3. Display BSD flags in verbose mode
4. Validate FinderInfo size (warn if not 32 bytes)

**Estimated Time**: 4-6 hours

## Gotchas and Edge Cases

### 1. Immutable File Handling

**Issue**: Can't modify files with uchg/schg flags set
**Error**: `EPERM: Operation not permitted`
**Solution**:
```rust
// Temporarily clear immutable flags
let original_flags = get_flags(path)?;
if original_flags & (UF_IMMUTABLE | SF_IMMUTABLE) != 0 {
    set_flags(path, original_flags & !(UF_IMMUTABLE | SF_IMMUTABLE))?;
    // ... make modifications ...
    set_flags(path, original_flags)?; // Restore
}
```

### 2. FinderInfo Size Validation

**Issue**: FinderInfo MUST be exactly 32 bytes
**Error**: `xattr: [Errno 34] Result too large`
**Solution**: Validate before writing:
```rust
if attr_name == "com.apple.FinderInfo" && value.len() != 32 {
    tracing::warn!("Invalid FinderInfo size: {} bytes (expected 32)", value.len());
    continue; // Skip invalid FinderInfo
}
```

### 3. System Integrity Protection (SIP)

**Issue**: Some attributes are protected by SIP
**Error**: `EPERM: Operation not permitted` on `com.apple.rootless`
**Solution**: Skip SIP-protected attributes:
```rust
match name {
    "com.apple.rootless" => {
        tracing::debug!("Skipping SIP-protected attribute: {}", name);
        continue;
    }
    _ => { /* preserve */ }
}
```

### 4. Super-User Flags Without Root

**Issue**: Can't set schg, archived, sappnd without root
**Error**: `EPERM: Operation not permitted`
**Solution**: Graceful degradation:
```rust
if let Err(e) = set_flags(path, flags) {
    if e.raw_os_error() == Some(libc::EPERM) {
        tracing::warn!("Cannot set system flags without root: {}", path.display());
        // Try again with only user flags
        let user_flags = flags & USER_FLAGS_MASK;
        set_flags(path, user_flags)?;
    } else {
        return Err(e.into());
    }
}
```

### 5. Quarantine Attribute Format

**Format**: `<flags>;<timestamp>;<agent>;<event UUID>`
**Example**: `0001;12345678;Safari;00000000-0000-0000-0000-000000000000`
**Consideration**: Preserving quarantine may be annoying (Gatekeeper warnings), but stripping it may be a security concern.
**Recommendation**: Preserve by default, add `--no-quarantine` opt-in flag.

## Testing Strategy

### Unit Tests

```rust
#[cfg(target_os = "macos")]
mod macos_tests {
    #[test]
    fn test_bsd_flags_capture() {
        // Create file with hidden flag
        // Verify scanner captures flag
    }

    #[test]
    fn test_bsd_flags_restoration() {
        // Sync file with flags
        // Verify flags preserved
    }

    #[test]
    fn test_immutable_file_handling() {
        // Create immutable file
        // Verify --force-change allows modification
    }

    #[test]
    fn test_quarantine_stripping() {
        // File with quarantine xattr
        // Sync with --no-quarantine
        // Verify quarantine removed
    }
}
```

### Integration Tests

```bash
# Test 1: Hidden flag preservation
chflags hidden source/file.txt
sy -F source dest
# Verify: dest/file.txt has hidden flag

# Test 2: Immutable file
chflags uchg source/file.txt
sy -F source dest
# Verify: dest/file.txt has uchg flag

# Test 3: Archive mode includes flags
sy -a source dest
# Verify: All flags preserved

# Test 4: Quarantine stripping
xattr -w com.apple.quarantine "0001;12345678;Safari;" source/file.txt
sy -X --no-quarantine source dest
# Verify: dest/file.txt has no quarantine xattr
```

## rsync Compatibility

sy should match rsync behavior for macOS-specific features:

| Feature | rsync Flag | sy Equivalent | Status |
|---------|------------|---------------|--------|
| Extended attributes | `-X` or `--xattrs` | `-X` or `--preserve-xattrs` | ✅ Implemented |
| ACLs | `-A` or `--acls` | `-A` or `--preserve-acls` | ✅ Implemented |
| BSD file flags | `--fileflags` | `-F` or `--preserve-flags` | ❌ To implement |
| Force change immutable | `--force-change` | `--force-change` | ❌ To implement |
| Archive mode (all) | `-a` (includes `-X` on macOS) | `-a` (should include `-F`) | ⚠️ Update needed |

## Conclusion

**sy's current macOS support is excellent** for extended attributes (Finder tags, resource forks, custom metadata). The primary gap is **BSD file flags**, which are:

1. Not currently preserved (even with `-a`)
2. Commonly expected by macOS users
3. Important for full-fidelity backups
4. Moderately easy to implement

**Recommended roadmap**:
- **v0.0.41**: BSD file flags (`--preserve-flags`, include in `-a`)
- **v0.0.42**: Quarantine stripping (`--no-quarantine`)
- **v0.0.43+**: Verbose metadata display (UX enhancement)
- **Future**: Document iCloud limitations (don't implement)

Implementation of BSD flags will make sy feature-complete for macOS-specific file sync requirements, matching or exceeding rsync's macOS support.
