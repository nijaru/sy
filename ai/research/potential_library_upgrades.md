# Potential Library Upgrades

Date: 2025-11-10
Status: Research - For Future Consideration

## Current Stack Assessment

The current dependency choices are **excellent and modern**. No critical gaps or urgent needs.

## Optional Improvements (Low Priority)

### 1. Error Reporting Enhancement
**Current**: `anyhow` (v1.x)
**Alternative**: `color-eyre` (v0.6+)

**Benefits**:
- Better panic/error formatting with color
- Stack traces with source context
- Suggestions for error resolution
- Drop-in replacement for anyhow

**Tradeoffs**:
- Adds ~15 deps (miette, owo-colors, etc.)
- Slightly larger binary
- More opinionated error display

**Verdict**: Nice-to-have, not critical. Current anyhow works well.

---

### 2. Structured Logging Enhancement
**Current**: `tracing` + `tracing-subscriber`
**Addition**: `tracing-tree` (v0.4+)

**Benefits**:
- Tree-structured span visualization
- Better debugging for nested operations
- Clear parent/child relationships in logs

**Use Case**: Debugging complex sync operations with nested spans

**Verdict**: Optional debugging aid. Could add as dev-dependency.

---

### 3. Property-Based Testing
**Current**: Manual unit tests + integration tests
**Addition**: `proptest` (v1.5+)

**Benefits**:
- Automatic edge case discovery
- Generate random file sync scenarios
- Find bugs in corner cases
- Shrinking to minimal failing case

**Use Cases**:
- File path edge cases (unicode, special chars, length limits)
- Delta sync with random data patterns
- Concurrent operation race conditions
- Resume state corruption scenarios

**Example**:
```rust
proptest! {
    #[test]
    fn sync_is_idempotent(files in prop::collection::vec(arbitrary_file(), 1..100)) {
        // First sync
        sync(&files)?;
        let state1 = get_dest_state();

        // Second sync (should be no-op)
        sync(&files)?;
        let state2 = get_dest_state();

        assert_eq!(state1, state2);
    }
}
```

**Verdict**: Good for robustness, but requires investment. Current test coverage is strong.

---

## Not Recommended

### Async Runtime Alternatives
**Current**: `tokio` (v1.x)
**Alternatives**: `async-std`, `smol`

**Verdict**: **Stick with tokio**. Industry standard, best ecosystem, SSH libs require it.

---

### Compression Alternatives
**Current**: `zstd`, `lz4`
**Alternatives**: `brotli`, `snappy`

**Verdict**: **Keep current**. Zstd/LZ4 are optimal for file sync (speed + ratio balance).

---

## Implementation Priority (If Pursued)

1. **Lowest hanging fruit**: `tracing-tree` as dev-dependency (zero risk)
2. **Medium effort**: `color-eyre` (easy migration, user-facing benefit)
3. **High effort**: `proptest` (requires test rewrite, high payoff)

## Decision

**Recommendation**: Current stack is solid. Only consider these if:
- Debugging becomes harder (add tracing-tree)
- Error reports confuse users (add color-eyre)
- Finding edge case bugs (add proptest)

**Action**: Document here, revisit in 6 months or when need arises.
