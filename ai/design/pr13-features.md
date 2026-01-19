# PR #13 Feature Extraction - Gemini Handoff

**Reference**: https://github.com/nijaru/sy/pull/13

---

## RULES (READ FIRST)

### Mandatory Before Each Task

1. Read `ai/STATUS.md`
2. Run `tk ls` to see current tasks
3. Run `tk start <id>` on the task you're working on

### Mandatory After Each Task

1. Run `cargo test` - must pass
2. Run `cargo clippy -- -D warnings` - must pass
3. Update `ai/STATUS.md` with what you did
4. Run `tk done <id>` to mark task complete
5. Commit your changes
6. **COMPACT** - tell user "ready to compact" and wait

### Do NOT

- Copy code from PR #13 - write fresh following our patterns
- Create new files without reading similar existing files first
- Skip tests or clippy
- Work on multiple tasks without compacting between them
- Modify `src/sync/server_mode.rs` - it uses new streaming protocol
- Add backwards compatibility shims or version suffixes
- Leave TODO comments in code

### Do

- Read existing code before writing new code
- Follow existing patterns exactly
- Keep changes minimal
- Test locally before committing
- Ask user if unclear

---

## TASK 1: GCS Transport

**Task ID**: `tk-dpw8`

### Prerequisites

```bash
tk start tk-dpw8
```

### Step-by-Step Checklist

- [ ] Read `src/transport/s3.rs` completely
- [ ] Read `src/transport/mod.rs` to see how S3 is exported
- [ ] Read `src/transport/router.rs` to see how `s3://` URLs are handled
- [ ] Create `src/transport/gcs.rs` following S3 pattern exactly

### Implementation

```rust
// src/transport/gcs.rs
// MUST follow this structure - do not deviate

use super::{FileInfo, TransferResult, Transport};
use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, ScanOptions};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub struct GcsTransport {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl GcsTransport {
    pub async fn new(
        bucket: String,
        prefix: String,
        project: Option<String>,
    ) -> Result<Self> {
        let mut builder = GoogleCloudStorageBuilder::new()
            .with_bucket_name(&bucket);

        if let Some(p) = project {
            builder = builder.with_project(&p);
        }

        let store = builder.build().map_err(|e| {
            SyncError::Transport(format!("Failed to create GCS client: {}", e))
        })?;

        Ok(Self {
            store: Arc::new(store),
            prefix,
        })
    }
}

// Copy ALL trait implementations from S3Transport
// Replace "S3" with "GCS" in error messages
// Do not change any logic
```

### Update Cargo.toml

Add feature flag (copy s3 pattern):

```toml
[features]
gcs = ["object_store"]  # object_store already has gcp feature enabled
```

### Update router.rs

Add `gs://` URL handling - copy `s3://` pattern exactly.

### Verification

```bash
cargo build --features gcs
cargo test --features gcs
cargo clippy --features gcs -- -D warnings
```

### Completion

```bash
tk done tk-dpw8
git add -A && git commit -m "feat: add GCS transport"
```

**STOP AND COMPACT BEFORE NEXT TASK**

---

## TASK 2: S3 Testing

**Task ID**: `tk-cb9z`

### Prerequisites

```bash
tk start tk-cb9z
```

### Step-by-Step Checklist

- [ ] Read `src/transport/s3.rs` completely
- [ ] Read `tests/` for any existing S3 tests
- [ ] Test with MinIO locally
- [ ] Document any fixes needed
- [ ] Fix issues found (if any)

### Local Testing with MinIO

```bash
# Terminal 1: Start MinIO
docker run -p 9000:9000 -p 9001:9001 \
  -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio server /data --console-address ":9001"

# Terminal 2: Create test bucket
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
aws --endpoint-url http://localhost:9000 s3 mb s3://test-bucket

# Terminal 3: Test sy
cargo build --features s3
./target/debug/sy /tmp/test-src s3://test-bucket/test-dst \
  --s3-endpoint http://localhost:9000
```

### What to Check

1. Does basic sync work?
2. Does custom endpoint work?
3. Are errors handled properly?
4. Does incremental sync work?

### If Issues Found

Fix them. Do not add workarounds. Do not add feature flags to disable broken code.

### Verification

```bash
cargo test --features s3
cargo clippy --features s3 -- -D warnings
```

### Completion

```bash
tk done tk-cb9z
git add -A && git commit -m "test: verify S3 transport with MinIO"
# or if fixes were needed:
git add -A && git commit -m "fix: S3 transport issues found during testing"
```

**STOP AND COMPACT BEFORE NEXT TASK**

---

## TASK 3: Python Bindings (LOW PRIORITY)

**Task ID**: `tk-dyjb`

**DO NOT START** until Tasks 1-2 complete and v0.3.0 is stable.

### Overview Only

- Separate `sypy/` crate in workspace
- Use maturin + pyo3
- Minimal API: `sync(src, dst, **options) -> SyncResult`
- Type stubs for IDE support

### When Ready

1. Read maturin docs
2. Read pyo3 docs
3. Look at similar projects (e.g., `ruff`, `polars` for patterns)
4. Create minimal binding first, expand later

---

## TASK 4: Daemon Mode (DEFERRED)

**Task ID**: `tk-bapt`

**DO NOT START** - deferred until users request it.

Streaming protocol already eliminates round-trip latency. SSH connection setup (~200-500ms) is the only remaining overhead. Not worth the complexity unless users complain.

---

## Summary Checklist

```
[ ] Task 1: GCS Transport (tk-dpw8)
    [ ] Read S3 code first
    [ ] Create gcs.rs following pattern
    [ ] Add feature flag
    [ ] Update router.rs
    [ ] cargo test + clippy pass
    [ ] Commit
    [ ] COMPACT

[ ] Task 2: S3 Testing (tk-cb9z)
    [ ] Read S3 code
    [ ] Test with MinIO
    [ ] Fix any issues
    [ ] cargo test + clippy pass
    [ ] Commit
    [ ] COMPACT

[ ] Task 3: Python Bindings (tk-dyjb) - AFTER v0.3.0
[ ] Task 4: Daemon Mode (tk-bapt) - DEFERRED
```

---

## If You Get Stuck

1. Stop
2. Update `ai/STATUS.md` with what's blocking you
3. Run `tk log <id> "blocked: <reason>"`
4. Tell user what's wrong
5. Compact and wait for guidance

Do not guess. Do not work around. Ask.
