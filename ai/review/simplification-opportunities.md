# Code Simplification Opportunities for sy

**Date**: 2026-01-18

After analyzing the sy codebase systematically, here are the findings organized by priority:

## High Priority: Code Duplication

### 1. `format_bytes` Function Duplicated 3 Times

**Locations:**

- `src/error.rs:175`
- `src/resource.rs:170`
- `src/main.rs:980`

All three are identical implementations. **Recommendation:** Keep only `crate::error::format_bytes` and have other modules import from there.

### 2. Delta Generation Shared Logic

**File:** `src/delta/generator.rs`

Both `generate_delta` (non-streaming) and `generate_delta_streaming` share:

- Hash table construction for checksums
- Block comparison logic
- Literal buffer management
- Strong hash verification

**Recommendation:** Extract `build_checksum_map()` helper and shared verification logic.

### 3. Bisync State Updates Repetition

**File:** `src/bisync/engine.rs:397-480`

`CopyToSource` and `CopyToDest` handling creates identical `SyncState` structs for both sides. Could be consolidated into a helper function.

---

## Medium Priority: Overly Complex Functions

### 1. `SyncEngine::new` - 35 Parameters

**File:** `src/sync/mod.rs:125-212`

Uses `#[allow(clippy::too_many_arguments)]`.

**Recommendation:** Builder pattern would make this more maintainable:

```rust
SyncEngine::builder(transport)
    .dry_run(true)
    .delete(true)
    .max_concurrent(8)
    .build()
```

### 2. `LocalTransport::sync_file_with_delta` - 475 Lines

**File:** `src/transport/local.rs:392-868`

Single method handling: sparse detection, change ratio estimation, COW strategy, in-place strategy, block comparison, verification, and error handling.

**Recommendation:** Split into:

- `should_use_full_copy()` - size/threshold checks
- `delta_sync_cow()` - COW-specific logic
- `delta_sync_inplace()` - in-place strategy

---

## Low Priority: Dead Code Annotations

**102 `#[allow(dead_code)]` annotations** across 34 files. Most are:

- Public API not yet used externally (~40)
- Builder methods for future use (~15)
- Test helpers (~10)
- Future features (~20)

**Files with most annotations:**

- `sync/scale.rs` (7)
- `cli.rs` (6)
- `sync/scanner.rs` (6)
- `bisync/engine.rs` (5)

**Recommendation:** Audit each and either document purpose or remove truly unused code.

---

## Not Recommended for Change

1. **`Arc<PathBuf>` in FileEntry** - Intentional for memory efficiency
2. **Async/Blocking split** - Correct pattern for filesystem I/O
3. **Separate hash implementations** - Blake3 (crypto) vs xxHash3 (fast) serve different purposes
4. **Platform-specific `#[cfg]` blocks** - Clear for this codebase size

---

## Summary Table

| Category                   | Count | Priority | Effort |
| -------------------------- | ----- | -------- | ------ |
| `format_bytes` duplication | 3     | High     | 1h     |
| Complex constructor        | 2     | Medium   | 4h     |
| Complex delta function     | 1     | Medium   | 3h     |
| Dead code annotations      | 102   | Low      | 2h     |
