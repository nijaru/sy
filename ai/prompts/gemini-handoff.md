# Gemini Handoff: sy v0.2.1 Bug Fixes

You are implementing critical bug fixes for sy, a Rust file sync tool.

## Critical Rules

1. **DO NOT** touch, comment on, merge, or cherry-pick from PR #13
2. **DO NOT** create new workflows or CI files
3. **DO NOT** add new features - only fix the listed bugs
4. **DO** commit after each fix with the exact format shown
5. **DO** run verification after each fix before committing

## Setup

```bash
git checkout main
git pull origin main
```

## Tasks

Complete these in order. Each task has: location, problem, fix, verification.

---

### Task 1: Fix content_equal() Data Loss Bug

**Priority:** P1 - Critical
**File:** `src/bisync/classifier.rs`
**Lines:** 226-237

**Problem:** Files with same size but different content are assumed equal. This causes data loss.

**Current code (lines 225-237):**

```rust
/// Check if two files have equal content
fn content_equal(source: &FileEntry, dest: &FileEntry) -> Result<bool> {
    // Fast path: size mismatch
    if source.size != dest.size {
        return Ok(false);
    }

    // For now, assume equal if sizes match
    // In future: compare checksums if available
    // This is conservative (may miss some conflicts) but safe

    Ok(true)
}
```

**Replace entire function with:**

```rust
/// Check if two files have equal content
fn content_equal(source: &FileEntry, dest: &FileEntry) -> Result<bool> {
    if source.size != dest.size {
        return Ok(false);
    }
    // Same size - compare mtimes as tie-breaker
    Ok(source.modified == dest.modified)
}
```

**Verification:**

```bash
cargo build
cargo test bisync
cargo clippy -- -D warnings
```

**Commit:**

```bash
git add src/bisync/classifier.rs
git commit -m "fix: Compare mtime in content_equal() to prevent data loss

Previously assumed files were equal if sizes matched, which could
miss conflicts when both sides modify a file to the same byte count."
```

---

### Task 2: Fix Lock Panics in SSH Transport

**Priority:** P1 - Critical
**File:** `src/transport/ssh.rs`
**Lines:** 172, 225, 240, 248

**Problem:** Using `.expect()` on RwLock will panic if lock is poisoned, crashing mid-sync.

**Current code (4 locations):**

```rust
.read().expect("SSH connection pool lock poisoned during read")
.write().expect("SSH connection pool lock poisoned during write")
.read().expect("SSH connection pool lock poisoned")
.read().expect("SSH connection pool lock poisoned")
```

**Fix:** Replace each `.expect(...)` with:

```rust
.unwrap_or_else(|poisoned| {
    tracing::warn!("SSH connection pool lock poisoned, recovering");
    poisoned.into_inner()
})
```

**Verification:**

```bash
cargo build
cargo test transport::ssh
cargo clippy -- -D warnings
```

**Commit:**

```bash
git add src/transport/ssh.rs
git commit -m "fix: Recover from poisoned locks in SSH transport

Replace expect() with unwrap_or_else to recover from poisoned locks
instead of panicking. Prevents crash if a thread panics while holding lock."
```

---

### Task 3: Fix SystemTime Unwrap Panic

**Priority:** P1 - Important
**File:** `src/bisync/resolver.rs`
**Lines:** 231-237

**Problem:** `unwrap()` will panic if system clock is before Unix epoch.

**Current code (lines 230-237):**

```rust
/// Generate timestamp for conflict filename
fn generate_conflict_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    format!("{}", now.as_secs())
}
```

**Replace entire function with:**

```rust
/// Generate timestamp for conflict filename
fn generate_conflict_timestamp() -> String {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| format!("{}", d.as_secs()))
        .unwrap_or_else(|_| "0".to_string())
}
```

**Verification:**

```bash
cargo build
cargo test bisync
cargo clippy -- -D warnings
```

**Commit:**

```bash
git add src/bisync/resolver.rs
git commit -m "fix: Handle invalid system time in conflict timestamp

Use unwrap_or_else instead of unwrap to handle edge case where
system clock is incorrectly set before Unix epoch."
```

---

### Task 4: Remove Dead Retry Code

**Priority:** P1 - Important
**File:** `src/retry.rs`
**Lines:** 107-121

**Problem:** Creates an `_updated` error that is never used. The underscore prefix shows it's dead code.

**Current code (lines 107-121):**

```rust
                // Update attempts count for NetworkRetryable errors
                if let SyncError::NetworkRetryable {
                    message,
                    max_attempts,
                    ..
                } = e
                {
                    // Create new error with updated attempt count
                    let _updated = SyncError::NetworkRetryable {
                        message,
                        attempts: attempt,
                        max_attempts,
                    };
                    // Note: The error will be recreated on next iteration
                }
```

**Fix:** Delete these 15 lines entirely (107-121). The `_updated` variable is created but never used.

**Verification:**

```bash
cargo build
cargo test retry
cargo clippy -- -D warnings
```

**Commit:**

```bash
git add src/retry.rs
git commit -m "fix: Remove dead retry error update code

The _updated error was created but never used. Removing dead code
that served no purpose."
```

---

## Final Verification

After all tasks:

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

All must pass with zero errors and zero warnings.

## Push

```bash
git push origin main
```

## Summary

| Task | File          | Issue                                |
| ---- | ------------- | ------------------------------------ |
| 1    | classifier.rs | content_equal() size-only comparison |
| 2    | ssh.rs        | Lock expect() panics (4 locations)   |
| 3    | resolver.rs   | SystemTime unwrap panic              |
| 4    | retry.rs      | Dead code removal                    |

## If Something Goes Wrong

- **Build fails**: Check error message, ensure you replaced code exactly as shown
- **Tests fail**: Read test output, the fix might need adjustment
- **Clippy warns**: Address the warning before committing
- **Unsure**: Stop and ask for help rather than guessing

## What NOT To Do

- Do not touch PR #13 in any way
- Do not add new files unless required for the fix
- Do not refactor surrounding code
- Do not add new features
- Do not modify CI/workflows
- Do not add comments beyond what's necessary
- Do not run `git push --force`
- Do not continue if tests fail - fix the issue first
