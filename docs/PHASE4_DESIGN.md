# Phase 4 Design - Resume Support, Watch Mode, JSON Output, Config Profiles

**Status**: ✅ Complete (2025-10-06)
**Versions**: v0.0.11 (JSON + Config), v0.0.12 (Watch), v0.0.13 (Resume)
**Duration**: Completed in 1 day

---

## 1. Resume Support

### Goal
Enable sy to resume interrupted sync operations without re-transferring completed files.

### Requirements
- Save sync state periodically during operation
- Detect interrupted syncs on next run
- Resume from last checkpoint
- Verify partial files before resuming
- Clean up state file on successful completion

### State File Schema

**Location**: `{destination}/.sy-state.json`

**Format**:
```json
{
  "version": 1,
  "source": "/absolute/path/to/source",
  "destination": "/absolute/path/to/destination",
  "started_at": "2025-10-06T12:34:56Z",
  "checkpoint_at": "2025-10-06T12:35:30Z",
  "flags": {
    "dry_run": false,
    "delete": true,
    "exclude": ["*.log", "*.tmp"],
    "min_size": 1024,
    "max_size": null
  },
  "completed_files": [
    {
      "relative_path": "file1.txt",
      "action": "create",
      "size": 1234,
      "checksum": "xxhash3:abc123...",
      "completed_at": "2025-10-06T12:35:10Z"
    },
    {
      "relative_path": "file2.txt",
      "action": "update",
      "size": 5678,
      "checksum": "xxhash3:def456...",
      "completed_at": "2025-10-06T12:35:25Z"
    }
  ],
  "total_files": 100,
  "total_bytes_transferred": 123456789
}
```

### Implementation Strategy

#### 1. State File Management

**Module**: `src/sync/resume.rs`

```rust
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct ResumeState {
    version: u32,
    source: PathBuf,
    destination: PathBuf,
    started_at: String,  // ISO 8601
    checkpoint_at: String,
    flags: SyncFlags,
    completed_files: Vec<CompletedFile>,
    total_files: usize,
    total_bytes_transferred: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncFlags {
    dry_run: bool,
    delete: bool,
    exclude: Vec<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompletedFile {
    relative_path: PathBuf,
    action: String,  // "create", "update", "delete"
    size: u64,
    checksum: String,  // "xxhash3:..." format
    completed_at: String,
}

impl ResumeState {
    pub fn load(destination: &Path) -> Result<Option<Self>>;
    pub fn save(&self, destination: &Path) -> Result<()>;
    pub fn delete(destination: &Path) -> Result<()>;
    pub fn is_compatible_with(&self, current_flags: &SyncFlags) -> bool;
    pub fn verify_file(&self, file: &Path) -> Result<bool>;
}
```

#### 2. Checkpointing Strategy

**When to checkpoint**:
- Every N files (default: 10)
- Every M bytes transferred (default: 100 MB)
- Every T seconds (default: 30 seconds)
- Configurable via flags: `--checkpoint-files N --checkpoint-bytes M --checkpoint-interval T`

**Checkpoint process**:
1. Lock state file (advisory lock)
2. Update `checkpoint_at` timestamp
3. Append newly completed files
4. Update `total_bytes_transferred`
5. Write atomically (write to temp, rename)
6. Release lock

#### 3. Resume Logic

**On sync start**:
```rust
async fn sync_with_resume(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
    // 1. Check for existing state file
    let resume_state = ResumeState::load(destination)?;

    // 2. If found, validate compatibility
    if let Some(state) = resume_state {
        if !state.is_compatible_with(&current_flags) {
            // Warn user: flags changed, cannot resume
            tracing::warn!("Resume state incompatible (flags changed), starting fresh");
            ResumeState::delete(destination)?;
        } else {
            // Ask user if they want to resume
            if self.should_resume(&state)? {
                return self.resume_sync(state, source, destination).await;
            } else {
                // User declined, start fresh
                ResumeState::delete(destination)?;
            }
        }
    }

    // 3. No resume state or user declined: start fresh
    self.sync_fresh(source, destination).await
}

fn should_resume(&self, state: &ResumeState) -> Result<bool> {
    // In interactive mode: prompt user
    // In non-interactive mode (CI, cron): auto-resume if --resume flag set
    // Otherwise: auto-resume by default (fail-safe)
    Ok(true)  // For now, always resume
}
```

#### 4. File Verification

**Before resuming**, verify each completed file:
```rust
fn verify_completed_file(&self, file: &CompletedFile, destination: &Path) -> Result<bool> {
    let full_path = destination.join(&file.relative_path);

    // Check file exists
    if !full_path.exists() {
        return Ok(false);  // File was deleted, re-sync needed
    }

    // Check size matches
    let metadata = std::fs::metadata(&full_path)?;
    if metadata.len() != file.size {
        return Ok(false);  // File modified, re-sync needed
    }

    // Optionally: verify checksum (can be slow)
    if self.verify_checksums {
        let actual_checksum = compute_xxhash3(&full_path)?;
        if actual_checksum != file.checksum {
            return Ok(false);  // Checksum mismatch, re-sync needed
        }
    }

    Ok(true)  // File verified, skip it
}
```

#### 5. Integration with SyncEngine

```rust
pub struct SyncEngine<T: Transport> {
    // ... existing fields ...
    resume: bool,                    // --resume flag
    checkpoint_files: usize,         // Default: 10
    checkpoint_bytes: u64,           // Default: 100 MB
    checkpoint_interval: Duration,   // Default: 30s
}

impl<T: Transport + 'static> SyncEngine<T> {
    pub async fn sync(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
        if self.resume {
            self.sync_with_resume(source, destination).await
        } else {
            self.sync_fresh(source, destination).await
        }
    }

    async fn sync_fresh(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
        // Current sync() implementation
        // Add checkpointing logic in the parallel execution loop
    }

    async fn resume_sync(&self, state: ResumeState, source: &Path, destination: &Path) -> Result<SyncStats> {
        // Verify all completed files
        let mut valid_completed = Vec::new();
        for file in &state.completed_files {
            if self.verify_completed_file(file, destination)? {
                valid_completed.push(file.relative_path.clone());
            }
        }

        // Scan source and filter out completed files
        let all_files = self.transport.scan(source).await?;
        let remaining_files: Vec<_> = all_files
            .into_iter()
            .filter(|f| !valid_completed.contains(&f.relative_path))
            .collect();

        tracing::info!("Resuming sync: {} files remaining", remaining_files.len());

        // Continue with normal sync logic for remaining files
        // ...
    }
}
```

### CLI Flags

```bash
# Enable resume support (default: auto-resume if state file found)
sy /src /dst --resume

# Disable resume (always start fresh)
sy /src /dst --no-resume

# Checkpoint configuration
sy /src /dst --checkpoint-files 50        # Checkpoint every 50 files
sy /src /dst --checkpoint-bytes 500MB     # Checkpoint every 500 MB
sy /src /dst --checkpoint-interval 60s    # Checkpoint every 60 seconds

# Verify checksums when resuming (slower but safer)
sy /src /dst --resume --verify-resume
```

### Error Handling

1. **State file corrupted**: Warn and start fresh
2. **Flags incompatible**: Warn and start fresh
3. **Completed files modified**: Re-sync those files
4. **Checkpoint write fails**: Continue sync, warn user
5. **Interrupted during checkpoint**: State file is atomic, safe

### Testing

1. **Basic resume**: Interrupt at 50%, verify resume completes
2. **Checkpoint frequency**: Verify checkpoints happen at intervals
3. **File verification**: Modify completed file, verify re-sync
4. **Flag compatibility**: Change exclude patterns, verify fresh start
5. **Corrupted state**: Corrupt JSON, verify graceful fallback
6. **Concurrent syncs**: Two syncs to same destination, verify locking

### Performance Impact

- **Checkpoint overhead**: ~10-50ms per checkpoint (JSON write)
- **Resume overhead**: ~1-5ms per file (hash lookup)
- **Memory overhead**: ~1KB per completed file in state
- **Disk overhead**: State file size = ~200 bytes per file

For 10,000 files:
- State file: ~2 MB
- Checkpoint cost: ~50ms every 10 files = 50 checkpoints = 2.5s total
- Resume cost: 10,000 files × 1ms = 10s verification

**Acceptable overhead** for reliability.

---

## 2. Watch Mode

### Goal
Continuously monitor source directory and sync changes in real-time.

### Requirements
- Detect file creation, modification, deletion
- Debounce rapid changes (avoid syncing every keystroke)
- Graceful shutdown on Ctrl+C
- Cross-platform (Linux, macOS, Windows)

### Implementation Strategy

**Dependency**: `notify = "6.0"` (cross-platform file watcher)

**Module**: `src/sync/watch.rs`

```rust
use notify::{Watcher, RecursiveMode, Event};
use std::time::Duration;

pub struct WatchMode<T: Transport> {
    engine: SyncEngine<T>,
    source: PathBuf,
    destination: PathBuf,
    debounce: Duration,  // Default: 500ms
}

impl<T: Transport + 'static> WatchMode<T> {
    pub async fn watch(&self) -> Result<()> {
        // Initial sync
        tracing::info!("Running initial sync...");
        self.engine.sync(&self.source, &self.destination).await?;

        // Set up file watcher
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(&self.source, RecursiveMode::Recursive)?;

        tracing::info!("Watching {} for changes (Ctrl+C to stop)", self.source.display());

        // Event loop with debouncing
        let mut pending_changes = Vec::new();
        let mut last_sync = Instant::now();

        loop {
            match rx.recv_timeout(self.debounce) {
                Ok(Ok(event)) => {
                    pending_changes.push(event);
                }
                Ok(Err(e)) => {
                    tracing::error!("Watch error: {}", e);
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Debounce timeout: sync if we have pending changes
                    if !pending_changes.is_empty() {
                        tracing::info!("Detected {} changes, syncing...", pending_changes.len());
                        self.engine.sync(&self.source, &self.destination).await?;
                        pending_changes.clear();
                        last_sync = Instant::now();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    break;  // Watcher dropped
                }
            }

            // Handle Ctrl+C
            if self.should_stop() {
                break;
            }
        }

        Ok(())
    }
}
```

### CLI Flags

```bash
# Enable watch mode
sy /src /dst --watch

# Configure debounce interval (default: 500ms)
sy /src /dst --watch --debounce 1s

# Watch mode with other flags
sy /src /dst --watch --delete --exclude "*.log"
```

### Testing

1. **Create file**: Touch file, verify sync
2. **Modify file**: Edit file, verify update
3. **Delete file**: Remove file, verify deletion (if --delete)
4. **Rapid changes**: Edit file 10x in 100ms, verify single sync
5. **Ctrl+C**: Verify graceful shutdown

---

## 3. JSON Output

### Goal
Provide machine-readable output for scripting and automation.

### Format

**NDJSON** (Newline-Delimited JSON): One JSON object per line

```json
{"type":"start","source":"/src","destination":"/dst","total_files":100}
{"type":"create","path":"file1.txt","size":1234,"bytes_transferred":1234}
{"type":"update","path":"file2.txt","size":5678,"bytes_transferred":234,"delta_used":true}
{"type":"skip","path":"file3.txt","reason":"up_to_date"}
{"type":"delete","path":"file4.txt"}
{"type":"error","path":"file5.txt","error":"Permission denied"}
{"type":"summary","files_created":50,"files_updated":20,"files_deleted":5,"bytes_transferred":123456,"duration_secs":12.5}
```

### Implementation

**Module**: `src/sync/output.rs`

```rust
use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncEvent {
    Start {
        source: PathBuf,
        destination: PathBuf,
        total_files: usize,
    },
    Create {
        path: PathBuf,
        size: u64,
        bytes_transferred: u64,
    },
    Update {
        path: PathBuf,
        size: u64,
        bytes_transferred: u64,
        delta_used: bool,
    },
    Skip {
        path: PathBuf,
        reason: String,
    },
    Delete {
        path: PathBuf,
    },
    Error {
        path: PathBuf,
        error: String,
    },
    Summary {
        files_created: usize,
        files_updated: usize,
        files_deleted: usize,
        bytes_transferred: u64,
        duration_secs: f64,
    },
}

impl SyncEvent {
    pub fn emit(&self) {
        // Only emit if --json flag is set
        if is_json_mode() {
            println!("{}", serde_json::to_string(self).unwrap());
        }
    }
}
```

### CLI Flags

```bash
# Enable JSON output
sy /src /dst --json

# JSON output is automatically quiet (no colors, no progress bar)
```

### Testing

1. **Valid JSON**: Verify each line parses as JSON
2. **Schema**: Verify all fields present
3. **Events**: Create/update/delete all emit events
4. **Errors**: Verify error events emitted

---

## 4. Config Profiles

### Goal
Save common sync configurations for reuse.

### Config File Location

**Path**: `~/.config/sy/config.toml` (XDG Base Directory)

**Format**:
```toml
# Default settings (applied to all syncs)
[defaults]
parallel = 10
exclude = ["*.tmp", ".DS_Store"]

# Named profiles
[profiles.deploy-prod]
source = "~/projects/myapp/dist"
destination = "user@prod.example.com:/var/www/html"
delete = true
exclude = ["*.map", "*.log"]
bwlimit = "10MB"

[profiles.backup-home]
source = "~"
destination = "/mnt/backup/home"
delete = true
exclude = [".cache", "node_modules", "target"]
resume = true
```

### CLI Usage

```bash
# Use named profile
sy --profile deploy-prod

# Override profile settings
sy --profile deploy-prod --dry-run

# List available profiles
sy --list-profiles

# Show profile details
sy --show-profile deploy-prod
```

### Implementation

**Module**: `src/config.rs` (already exists, needs enhancement)

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct Config {
    defaults: Option<Defaults>,
    profiles: HashMap<String, Profile>,
}

#[derive(Debug, Deserialize)]
pub struct Defaults {
    parallel: Option<usize>,
    exclude: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Profile {
    source: Option<String>,
    destination: Option<String>,
    delete: Option<bool>,
    exclude: Option<Vec<String>>,
    bwlimit: Option<String>,
    resume: Option<bool>,
    min_size: Option<String>,
    max_size: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = dirs::config_dir()
            .ok_or_else(|| anyhow!("Cannot find config directory"))?
            .join("sy")
            .join("config.toml");

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&config_path)?;
        Ok(toml::from_str(&contents)?)
    }

    pub fn get_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn list_profiles(&self) -> Vec<&String> {
        self.profiles.keys().collect()
    }
}
```

### Testing

1. **Load config**: Verify TOML parsing
2. **Apply profile**: Verify flags merged correctly
3. **Override**: CLI flags override profile settings
4. **Missing profile**: Error message
5. **Invalid TOML**: Helpful error message

---

## Implementation Order

1. **Week 1**: Resume support (most critical)
   - State file schema
   - Checkpointing logic
   - Resume verification
   - Tests

2. **Week 2**: JSON output + Config profiles (easier features)
   - JSON event emission
   - Config file loading
   - Profile CLI flags
   - Tests

3. **Week 2-3**: Watch mode (requires testing)
   - File watcher integration
   - Debouncing logic
   - Graceful shutdown
   - Cross-platform testing

4. **Week 3**: Integration + documentation
   - End-to-end tests
   - Update README
   - Update CHANGELOG
   - Bump version to v0.1.0

---

## Success Criteria

- ✅ Resume works after interrupt at any point
- ✅ Watch mode detects all file changes
- ✅ JSON output is valid and complete
- ✅ Config profiles load and apply correctly
- ✅ All 100+ tests pass (including new Phase 4 tests)
- ✅ Documentation updated
- ✅ Zero regressions in existing features

---

**Last Updated**: 2025-10-06
**Status**: All Phase 4 features complete (v0.0.11-v0.0.13)
**Next**: Phase 5 - Verification & Reliability (see docs/MODERNIZATION_ROADMAP.md)
