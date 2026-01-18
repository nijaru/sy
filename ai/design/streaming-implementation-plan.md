# Streaming Protocol v2 Implementation Plan

**Purpose:** Step-by-step guide for implementing the streaming protocol. Designed to be followed by any AI agent or developer without requiring deep context.

**Branch:** `feature/streaming-protocol-v2`

**Status:** Phase 1 complete. Phases 2-7 remaining.

---

## Prerequisites

Before starting any phase:

```bash
git checkout feature/streaming-protocol-v2
cargo check  # Must pass
cargo test streaming  # Must pass
```

---

## Phase 2: Generator

**Goal:** Create the Generator task that scans source files and sends metadata to the Sender.

### File to Create: `src/streaming/generator.rs`

```rust
//! Generator task for streaming sync.
//!
//! Scans source directory and streams file metadata to Sender.
//! Receives destination state during Initial Exchange.

use crate::streaming::channel::{
    DeltaInfo, DestFileState, DestIndex, FileJob, FileJobSender, GeneratorMessage,
    DELTA_MIN_SIZE,
};
use crate::streaming::protocol::{BlockChecksum, DestFileEntry, DestFileFlags};
use crate::sync::scanner::{ScanEntry, ScanOptions, Scanner};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
```

### Step 2.1: Define Generator struct

```rust
/// Generator configuration
pub struct GeneratorConfig {
    /// Root path to scan
    pub root: PathBuf,
    /// Whether to include hidden files
    pub include_hidden: bool,
    /// Whether to follow symlinks
    pub follow_symlinks: bool,
    /// Whether --delete is enabled
    pub delete_enabled: bool,
}

/// Generator state
pub struct Generator {
    config: GeneratorConfig,
    dest_index: DestIndex,
    seen_inodes: HashMap<u64, Arc<PathBuf>>,  // For hard link detection
}
```

### Step 2.2: Implement Initial Exchange receiver

The Generator must first receive DEST_FILE_ENTRY messages to build the dest index:

```rust
impl Generator {
    pub fn new(config: GeneratorConfig) -> Self {
        Self {
            config,
            dest_index: DestIndex::new(),
            seen_inodes: HashMap::new(),
        }
    }

    /// Process a DEST_FILE_ENTRY received during Initial Exchange.
    /// Call this for each entry before starting the scan.
    pub fn add_dest_entry(&mut self, entry: DestFileEntry) {
        let delta_info = if entry.flags.contains(DestFileFlags::HAS_CHECKSUMS) {
            Some(DeltaInfo {
                block_size: entry.block_size,
                checksums: entry.checksums,
            })
        } else {
            None
        };

        self.dest_index.insert(
            entry.path,
            DestFileState {
                size: entry.size,
                mtime: entry.mtime,
                mode: entry.mode,
                is_dir: entry.flags.contains(DestFileFlags::DIR),
                delta_info,
            },
        );
    }

    /// Called after all DEST_FILE_ENTRY received (after DEST_FILE_END).
    pub fn dest_count(&self) -> usize {
        self.dest_index.len()
    }
}
```

### Step 2.3: Implement the scan loop

```rust
impl Generator {
    /// Run the generator, scanning source and sending to channel.
    /// Returns (total_files, total_bytes).
    pub async fn run(mut self, tx: FileJobSender) -> Result<(u64, u64)> {
        let scanner = Scanner::new(ScanOptions {
            include_hidden: self.config.include_hidden,
            follow_symlinks: self.config.follow_symlinks,
            ..Default::default()
        });

        let mut total_files = 0u64;
        let mut total_bytes = 0u64;

        // Scan and stream entries
        let entries = scanner.scan(&self.config.root).await?;

        for entry in entries {
            let rel_path = entry.path.strip_prefix(&self.config.root)
                .unwrap_or(&entry.path)
                .to_path_buf();
            let rel_path_str = rel_path.to_string_lossy().to_string();

            // Remove from dest_index (remaining entries are deletes)
            self.dest_index.remove(&rel_path_str);

            let msg = if entry.is_dir {
                GeneratorMessage::Mkdir {
                    path: Arc::new(rel_path),
                    mode: entry.mode,
                }
            } else if entry.is_symlink {
                GeneratorMessage::Symlink {
                    path: Arc::new(rel_path),
                    target: entry.symlink_target.unwrap_or_default(),
                }
            } else {
                // Check for hard link
                let link_target = if entry.nlink > 1 {
                    if let Some(existing) = self.seen_inodes.get(&entry.inode) {
                        Some(existing.clone())
                    } else {
                        self.seen_inodes.insert(entry.inode, Arc::new(rel_path.clone()));
                        None
                    }
                } else {
                    None
                };

                // Determine if delta is needed
                let (need_delta, checksums) = self.check_delta(&rel_path_str, entry.size);

                total_files += 1;
                total_bytes += entry.size;

                GeneratorMessage::File(FileJob {
                    path: Arc::new(rel_path),
                    size: entry.size,
                    mtime: entry.mtime,
                    mode: entry.mode,
                    inode: entry.inode,
                    need_delta,
                    checksums,
                })
            };

            tx.send(msg).await?;
        }

        // Send FILE_END
        tx.send(GeneratorMessage::FileEnd {
            total_files,
            total_bytes,
        }).await?;

        // Send deletes if enabled
        if self.config.delete_enabled {
            let mut delete_count = 0u64;
            for (path, state) in self.dest_index.remaining_paths() {
                tx.send(GeneratorMessage::Delete {
                    path: Arc::new(PathBuf::from(path)),
                    is_dir: state.is_dir,
                }).await?;
                delete_count += 1;
            }
            tx.send(GeneratorMessage::DeleteEnd { count: delete_count }).await?;
        }

        Ok((total_files, total_bytes))
    }

    fn check_delta(&self, path: &str, size: u64) -> (bool, Option<DeltaInfo>) {
        if size < DELTA_MIN_SIZE {
            return (false, None);
        }

        if let Some(dest_state) = self.dest_index.get(path) {
            if let Some(ref delta_info) = dest_state.delta_info {
                return (true, Some(delta_info.clone()));
            }
        }

        (false, None)
    }
}
```

### Step 2.4: Update mod.rs

Add to `src/streaming/mod.rs`:

```rust
pub mod generator;
pub use generator::{Generator, GeneratorConfig};
```

### Step 2.5: Write tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

    #[tokio::test]
    async fn test_generator_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: false,
        };

        let (tx, mut rx) = crate::streaming::channel::file_job_channel();
        let gen = Generator::new(config);

        tokio::spawn(async move {
            gen.run(tx).await.unwrap();
        });

        // Should receive FileEnd with 0 files
        match rx.recv().await {
            Some(GeneratorMessage::FileEnd { total_files, .. }) => {
                assert_eq!(total_files, 0);
            }
            other => panic!("Expected FileEnd, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_generator_with_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file1.txt"), "hello").unwrap();
        fs::write(tmp.path().join("file2.txt"), "world").unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();

        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: false,
        };

        let (tx, mut rx) = crate::streaming::channel::file_job_channel();
        let gen = Generator::new(config);

        tokio::spawn(async move {
            gen.run(tx).await.unwrap();
        });

        let mut file_count = 0;
        let mut dir_count = 0;

        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::File(_) => file_count += 1,
                GeneratorMessage::Mkdir { .. } => dir_count += 1,
                GeneratorMessage::FileEnd { total_files, .. } => {
                    assert_eq!(total_files, 2);
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(file_count, 2);
        assert_eq!(dir_count, 1);
    }

    #[tokio::test]
    async fn test_generator_delete_detection() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("keep.txt"), "keep").unwrap();

        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: true,
        };

        let (tx, mut rx) = crate::streaming::channel::file_job_channel();
        let mut gen = Generator::new(config);

        // Simulate dest has extra file
        gen.add_dest_entry(DestFileEntry {
            path: "delete_me.txt".to_string(),
            size: 100,
            mtime: 0,
            mode: 0o644,
            flags: DestFileFlags::empty(),
            block_size: 0,
            checksums: vec![],
        });

        tokio::spawn(async move {
            gen.run(tx).await.unwrap();
        });

        let mut got_delete = false;
        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::Delete { path, .. } => {
                    if path.to_string_lossy() == "delete_me.txt" {
                        got_delete = true;
                    }
                }
                GeneratorMessage::DeleteEnd { .. } => break,
                _ => {}
            }
        }

        assert!(got_delete, "Should have received delete for delete_me.txt");
    }
}
```

### Verification

```bash
cargo check
cargo test streaming::generator
cargo clippy -- -D warnings
```

---

## Phase 3: Sender

**Goal:** Create the Sender task that reads files and computes deltas.

### File to Create: `src/streaming/sender.rs`

### Step 3.1: Define Sender struct

```rust
//! Sender task for streaming sync.
//!
//! Receives FileJobs from Generator, reads file content,
//! computes deltas when possible, and sends Data chunks.

use crate::streaming::channel::{
    DataChunk, DeltaInfo, FileJob, FileJobReceiver, GeneratorMessage,
    DATA_CHUNK_SIZE,
};
use crate::streaming::protocol::{Data, DataEnd, DataFlags};
use crate::delta::generator::DeltaGenerator;
use anyhow::Result;
use bytes::Bytes;
use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};

pub struct SenderConfig {
    /// Root path for reading files
    pub root: PathBuf,
    /// Whether to compress data
    pub compress: bool,
}

pub struct Sender {
    config: SenderConfig,
}
```

### Step 3.2: Implement file reading and chunking

```rust
impl Sender {
    pub fn new(config: SenderConfig) -> Self {
        Self { config }
    }

    /// Run the sender, processing FileJobs and outputting Data messages.
    /// Returns encoded Data messages via callback.
    pub async fn run<F>(
        self,
        mut rx: FileJobReceiver,
        mut on_data: F,
    ) -> Result<()>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::File(job) => {
                    self.process_file(job, &mut on_data).await?;
                }
                GeneratorMessage::Mkdir { path, mode } => {
                    // Forward mkdir - encode and send
                    let msg = crate::streaming::protocol::Mkdir {
                        path: path.to_string_lossy().to_string(),
                        mode,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::Symlink { path, target } => {
                    let msg = crate::streaming::protocol::Symlink {
                        path: path.to_string_lossy().to_string(),
                        target,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::Delete { path, is_dir } => {
                    let msg = crate::streaming::protocol::Delete {
                        path: path.to_string_lossy().to_string(),
                        is_dir,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::FileEnd { total_files, total_bytes } => {
                    let msg = crate::streaming::protocol::FileEnd {
                        total_files,
                        total_bytes,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::DeleteEnd { count } => {
                    let msg = crate::streaming::protocol::DeleteEnd { count };
                    on_data(msg.encode())?;
                }
            }
        }
        Ok(())
    }

    async fn process_file<F>(&self, job: FileJob, on_data: &mut F) -> Result<()>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        let path_str = job.path.to_string_lossy().to_string();
        let full_path = self.config.root.join(job.path.as_ref());

        // Send FILE_ENTRY first
        let entry = crate::streaming::protocol::FileEntry {
            path: path_str.clone(),
            size: job.size,
            mtime: job.mtime,
            mode: job.mode,
            inode: job.inode,
            flags: crate::streaming::protocol::FileFlags::empty(),
            symlink_target: None,
            link_target: None,
        };
        on_data(entry.encode())?;

        // Read and send data chunks
        if job.need_delta && job.checksums.is_some() {
            self.send_delta(&full_path, &path_str, job.checksums.unwrap(), on_data).await?;
        } else {
            self.send_full(&full_path, &path_str, on_data).await?;
        }

        // Send DATA_END
        let end = DataEnd {
            path: path_str,
            status: DataEnd::STATUS_OK,
        };
        on_data(end.encode())?;

        Ok(())
    }

    async fn send_full<F>(&self, path: &Path, path_str: &str, on_data: &mut F) -> Result<()>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        let file = File::open(path).await?;
        let mut reader = BufReader::new(file);
        let mut offset = 0u64;
        let mut buf = vec![0u8; DATA_CHUNK_SIZE];

        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }

            let mut flags = DataFlags::empty();
            if self.config.compress {
                flags |= DataFlags::COMPRESSED;
            }

            let data = Data {
                path: path_str.to_string(),
                offset,
                flags,
                data: Bytes::copy_from_slice(&buf[..n]),
            };
            on_data(data.encode())?;

            offset += n as u64;
        }

        Ok(())
    }

    async fn send_delta<F>(
        &self,
        path: &Path,
        path_str: &str,
        delta_info: DeltaInfo,
        on_data: &mut F,
    ) -> Result<()>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        // Use existing delta generator
        // This integrates with src/delta/generator.rs
        let delta_gen = DeltaGenerator::new(delta_info.block_size as usize);

        // Build checksum lookup
        let checksums: Vec<_> = delta_info.checksums.iter()
            .map(|c| crate::delta::checksum::BlockChecksum {
                offset: c.offset,
                weak: c.weak,
                strong: c.strong,
            })
            .collect();

        let file = File::open(path).await?;
        let mut reader = BufReader::new(file);

        // Generate delta ops
        let ops = delta_gen.generate(&mut reader, &checksums).await?;

        // Encode delta ops into DATA message
        let mut flags = DataFlags::DELTA;
        if self.config.compress {
            flags |= DataFlags::COMPRESSED;
        }

        // Serialize delta ops
        let mut delta_bytes = Vec::new();
        for op in ops {
            match op {
                crate::delta::generator::DeltaOp::Copy { offset, size } => {
                    delta_bytes.push(0x00);
                    delta_bytes.extend_from_slice(&offset.to_be_bytes());
                    delta_bytes.extend_from_slice(&size.to_be_bytes());
                }
                crate::delta::generator::DeltaOp::Insert(data) => {
                    delta_bytes.push(0x01);
                    delta_bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
                    delta_bytes.extend_from_slice(&data);
                }
            }
        }

        let data = Data {
            path: path_str.to_string(),
            offset: 0,
            flags,
            data: Bytes::from(delta_bytes),
        };
        on_data(data.encode())?;

        Ok(())
    }
}
```

### Step 3.3: Update mod.rs

```rust
pub mod sender;
pub use sender::{Sender, SenderConfig};
```

### Step 3.4: Write tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

    #[tokio::test]
    async fn test_sender_simple_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let config = SenderConfig {
            root: tmp.path().to_path_buf(),
            compress: false,
        };

        let (tx, rx) = crate::streaming::channel::file_job_channel();
        let sender = Sender::new(config);

        // Send a file job
        tx.send(GeneratorMessage::File(FileJob {
            path: Arc::new(PathBuf::from("test.txt")),
            size: 11,
            mtime: 0,
            mode: 0o644,
            inode: 0,
            need_delta: false,
            checksums: None,
        })).await.unwrap();

        tx.send(GeneratorMessage::FileEnd {
            total_files: 1,
            total_bytes: 11,
        }).await.unwrap();

        drop(tx);

        let mut messages = Vec::new();
        sender.run(rx, |bytes| {
            messages.push(bytes);
            Ok(())
        }).await.unwrap();

        // Should have: FileEntry, Data, DataEnd, FileEnd
        assert!(messages.len() >= 4);
    }
}
```

### Verification

```bash
cargo check
cargo test streaming::sender
cargo clippy -- -D warnings
```

---

## Phase 4: Receiver

**Goal:** Create the Receiver task that writes files from incoming Data messages.

### File to Create: `src/streaming/receiver.rs`

### Step 4.1: Define Receiver struct

```rust
//! Receiver task for streaming sync.
//!
//! Receives Data messages and writes files to disk.
//! Handles Initial Exchange by sending DEST_FILE_ENTRY.

use crate::streaming::protocol::{
    Data, DataEnd, DataFlags, Delete, DestFileEntry, DestFileEnd, DestFileFlags,
    FileEntry, FileEnd, MessageType, Mkdir, Symlink,
};
use crate::streaming::channel::{SyncStats, DELTA_MIN_SIZE};
use crate::delta::applier::DeltaApplier;
use anyhow::Result;
use bytes::Bytes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;

pub struct ReceiverConfig {
    /// Root path for writing files
    pub root: PathBuf,
    /// Block size for checksums
    pub block_size: u32,
}

pub struct Receiver {
    config: ReceiverConfig,
    pending_files: HashMap<String, PendingFile>,
    stats: SyncStats,
}

struct PendingFile {
    entry: FileEntry,
    file: Option<File>,
    bytes_written: u64,
}
```

### Step 4.2: Implement Initial Exchange sender

```rust
impl Receiver {
    pub fn new(config: ReceiverConfig) -> Self {
        Self {
            config,
            pending_files: HashMap::new(),
            stats: SyncStats::new(),
        }
    }

    /// Scan destination and yield DEST_FILE_ENTRY messages for Initial Exchange.
    pub async fn scan_dest<F>(&self, mut on_entry: F) -> Result<(u64, u64)>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        let mut total_files = 0u64;
        let mut total_bytes = 0u64;

        let scanner = crate::sync::scanner::Scanner::new(Default::default());
        let entries = scanner.scan(&self.config.root).await?;

        for entry in entries {
            let rel_path = entry.path.strip_prefix(&self.config.root)
                .unwrap_or(&entry.path);
            let path_str = rel_path.to_string_lossy().to_string();

            let mut flags = DestFileFlags::empty();
            if entry.is_dir {
                flags |= DestFileFlags::DIR;
            }

            // Compute checksums for delta candidates
            let (block_size, checksums) = if !entry.is_dir
                && entry.size >= DELTA_MIN_SIZE
            {
                flags |= DestFileFlags::HAS_CHECKSUMS;
                let cs = self.compute_checksums(&entry.path).await?;
                (self.config.block_size, cs)
            } else {
                (0, vec![])
            };

            let dest_entry = DestFileEntry {
                path: path_str,
                size: entry.size,
                mtime: entry.mtime,
                mode: entry.mode,
                flags,
                block_size,
                checksums,
            };
            on_entry(dest_entry.encode())?;

            total_files += 1;
            total_bytes += entry.size;
        }

        // Send DEST_FILE_END
        let end = DestFileEnd {
            total_files,
            total_bytes,
        };
        on_entry(end.encode())?;

        Ok((total_files, total_bytes))
    }

    async fn compute_checksums(&self, path: &Path) -> Result<Vec<crate::streaming::protocol::BlockChecksum>> {
        use crate::delta::checksum::compute_block_checksums;

        let checksums = compute_block_checksums(path, self.config.block_size as usize).await?;

        Ok(checksums.into_iter().map(|c| {
            crate::streaming::protocol::BlockChecksum {
                offset: c.offset,
                weak: c.weak,
                strong: c.strong,
            }
        }).collect())
    }
}
```

### Step 4.3: Implement message handling

```rust
impl Receiver {
    /// Process an incoming message.
    pub async fn handle_message(&mut self, msg_type: MessageType, payload: Bytes) -> Result<()> {
        match msg_type {
            MessageType::FileEntry => {
                let entry = FileEntry::decode(payload)?;
                self.handle_file_entry(entry).await?;
            }
            MessageType::Data => {
                let data = Data::decode(payload)?;
                self.handle_data(data).await?;
            }
            MessageType::DataEnd => {
                let end = DataEnd::decode(payload)?;
                self.handle_data_end(end).await?;
            }
            MessageType::Mkdir => {
                let mkdir = Mkdir::decode(payload)?;
                self.handle_mkdir(mkdir).await?;
            }
            MessageType::Symlink => {
                let symlink = Symlink::decode(payload)?;
                self.handle_symlink(symlink).await?;
            }
            MessageType::Delete => {
                let delete = Delete::decode(payload)?;
                self.handle_delete(delete).await?;
            }
            MessageType::FileEnd => {
                let _end = FileEnd::decode(payload)?;
                // File list complete, data transfer continues
            }
            _ => {
                // Ignore unknown messages
            }
        }
        Ok(())
    }

    async fn handle_file_entry(&mut self, entry: FileEntry) -> Result<()> {
        let full_path = self.config.root.join(&entry.path);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Open file for writing
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&full_path)
            .await?;

        self.pending_files.insert(entry.path.clone(), PendingFile {
            entry,
            file: Some(file),
            bytes_written: 0,
        });

        Ok(())
    }

    async fn handle_data(&mut self, data: Data) -> Result<()> {
        let pending = self.pending_files.get_mut(&data.path)
            .ok_or_else(|| anyhow::anyhow!("No pending file for {}", data.path))?;

        if let Some(ref mut file) = pending.file {
            if data.flags.contains(DataFlags::DELTA) {
                // Apply delta
                self.apply_delta(file, &data.data).await?;
            } else {
                // Write raw data
                file.write_all(&data.data).await?;
            }
            pending.bytes_written += data.data.len() as u64;
        }

        Ok(())
    }

    async fn handle_data_end(&mut self, end: DataEnd) -> Result<()> {
        if let Some(mut pending) = self.pending_files.remove(&end.path) {
            if let Some(file) = pending.file.take() {
                file.sync_all().await?;
            }

            // Set permissions
            let full_path = self.config.root.join(&end.path);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(pending.entry.mode);
                fs::set_permissions(&full_path, perms).await?;
            }

            // Set mtime
            let mtime = filetime::FileTime::from_unix_time(pending.entry.mtime, 0);
            filetime::set_file_mtime(&full_path, mtime)?;

            if end.status == DataEnd::STATUS_OK {
                self.stats.files_ok += 1;
                self.stats.bytes_transferred += pending.bytes_written;
            } else {
                self.stats.files_err += 1;
            }
        }

        Ok(())
    }

    async fn handle_mkdir(&mut self, mkdir: Mkdir) -> Result<()> {
        let full_path = self.config.root.join(&mkdir.path);
        fs::create_dir_all(&full_path).await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mkdir.mode);
            fs::set_permissions(&full_path, perms).await?;
        }

        self.stats.dirs_created += 1;
        Ok(())
    }

    async fn handle_symlink(&mut self, symlink: Symlink) -> Result<()> {
        let full_path = self.config.root.join(&symlink.path);

        // Remove existing if any
        let _ = fs::remove_file(&full_path).await;

        #[cfg(unix)]
        tokio::fs::symlink(&symlink.target, &full_path).await?;

        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&symlink.target, &full_path)?;

        self.stats.symlinks_created += 1;
        Ok(())
    }

    async fn handle_delete(&mut self, delete: Delete) -> Result<()> {
        let full_path = self.config.root.join(&delete.path);

        if delete.is_dir {
            fs::remove_dir_all(&full_path).await?;
        } else {
            fs::remove_file(&full_path).await?;
        }

        self.stats.deleted += 1;
        Ok(())
    }

    async fn apply_delta(&self, file: &mut File, delta_data: &[u8]) -> Result<()> {
        // Parse and apply delta ops
        // Uses crate::delta::applier
        let applier = DeltaApplier::new();
        applier.apply(file, delta_data).await
    }

    pub fn stats(&self) -> &SyncStats {
        &self.stats
    }
}
```

### Step 4.4: Update mod.rs

```rust
pub mod receiver;
pub use receiver::{Receiver, ReceiverConfig};
```

### Verification

```bash
cargo check
cargo test streaming::receiver
cargo clippy -- -D warnings
```

---

## Phase 5: Integration

**Goal:** Wire up Generator, Sender, Receiver into a working pipeline.

### File to Create: `src/streaming/pipeline.rs`

### Step 5.1: Define Pipeline

```rust
//! Streaming sync pipeline.
//!
//! Orchestrates Generator, Sender, and Receiver tasks.

use crate::streaming::{
    Generator, GeneratorConfig, Sender, SenderConfig, Receiver, ReceiverConfig,
    channel::{file_job_channel, SyncStats},
    protocol::{Hello, HelloFlags, read_frame, write_frame, MessageType},
};
use anyhow::Result;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub struct StreamingSync {
    pub local_root: PathBuf,
    pub remote_root: PathBuf,
    pub delete_enabled: bool,
    pub compress: bool,
}

impl StreamingSync {
    /// Run a push sync (local -> remote).
    pub async fn push<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> Result<SyncStats>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        // 1. Send HELLO
        let hello = Hello::new(HelloFlags::empty(), &self.remote_root.to_string_lossy());
        write_frame(writer, &hello.encode()).await?;
        writer.flush().await?;

        // 2. Receive HELLO response
        let (msg_type, payload) = read_frame(reader).await?;
        if msg_type != MessageType::Hello {
            anyhow::bail!("Expected Hello response");
        }
        let _server_hello = Hello::decode(payload)?;

        // 3. Receive DEST_FILE_ENTRY messages (Initial Exchange)
        let mut generator = Generator::new(GeneratorConfig {
            root: self.local_root.clone(),
            include_hidden: true,
            follow_symlinks: false,
            delete_enabled: self.delete_enabled,
        });

        loop {
            let (msg_type, payload) = read_frame(reader).await?;
            match msg_type {
                MessageType::DestFileEntry => {
                    let entry = crate::streaming::protocol::DestFileEntry::decode(payload)?;
                    generator.add_dest_entry(entry);
                }
                MessageType::DestFileEnd => {
                    break;
                }
                _ => {
                    anyhow::bail!("Unexpected message during Initial Exchange: {:?}", msg_type);
                }
            }
        }

        // 4. Run Generator and Sender
        let (tx, rx) = file_job_channel();

        let gen_handle = tokio::spawn(async move {
            generator.run(tx).await
        });

        let sender = Sender::new(SenderConfig {
            root: self.local_root.clone(),
            compress: self.compress,
        });

        sender.run(rx, |bytes| {
            // This is sync - need to use a channel or make async
            // For now, collect and write after
            Ok(())
        }).await?;

        let (total_files, total_bytes) = gen_handle.await??;

        Ok(SyncStats {
            files_ok: total_files,
            bytes_transferred: total_bytes,
            ..Default::default()
        })
    }

    /// Run a pull sync (remote -> local).
    pub async fn pull<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> Result<SyncStats>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        // 1. Send HELLO with PULL flag
        let hello = Hello::new(
            HelloFlags::PULL | if self.delete_enabled { HelloFlags::DELETE } else { HelloFlags::empty() },
            &self.remote_root.to_string_lossy(),
        );
        write_frame(writer, &hello.encode()).await?;
        writer.flush().await?;

        // 2. Receive HELLO response
        let (msg_type, payload) = read_frame(reader).await?;
        if msg_type != MessageType::Hello {
            anyhow::bail!("Expected Hello response");
        }
        let _server_hello = Hello::decode(payload)?;

        // 3. Send DEST_FILE_ENTRY messages (Initial Exchange)
        let receiver = Receiver::new(ReceiverConfig {
            root: self.local_root.clone(),
            block_size: 4096,
        });

        receiver.scan_dest(|bytes| {
            // Write to remote
            Ok(())
        }).await?;
        writer.flush().await?;

        // 4. Receive and process streaming messages
        let mut receiver = Receiver::new(ReceiverConfig {
            root: self.local_root.clone(),
            block_size: 4096,
        });

        loop {
            let (msg_type, payload) = read_frame(reader).await?;

            if msg_type == MessageType::Done {
                break;
            }

            receiver.handle_message(msg_type, payload).await?;
        }

        Ok(receiver.stats().clone())
    }
}
```

### Verification

```bash
cargo check
cargo test streaming::pipeline
cargo clippy -- -D warnings
```

---

## Phase 6: Server Integration

**Goal:** Add v2 protocol support to `sy --server`.

### File to Modify: `src/server/mod.rs`

### Step 6.1: Add version detection

At the top of `run_server()`, after reading HELLO:

```rust
// Check protocol version
if hello.version == 1 {
    // Use existing v1 handler
    return run_server_v1(hello, stdin, stdout).await;
} else if hello.version >= 2 {
    // Use new streaming handler
    return run_server_v2(hello, stdin, stdout).await;
} else {
    let err = ErrorMessage {
        code: 1,
        message: format!("Unsupported protocol version: {}", hello.version),
    };
    err.write(&mut stdout).await?;
    return Ok(());
}
```

### Step 6.2: Implement v2 server handler

```rust
async fn run_server_v2(
    hello: Hello,
    mut stdin: impl AsyncRead + Unpin,
    mut stdout: impl AsyncWrite + Unpin,
) -> Result<()> {
    use crate::streaming::{Receiver, ReceiverConfig, protocol as v2};

    let root_path = expand_tilde(Path::new(&hello.root_path));

    // Send HELLO response
    let resp = v2::Hello::new(v2::HelloFlags::empty(), "");
    v2::write_frame(&mut stdout, &resp.encode()).await?;
    stdout.flush().await?;

    let is_pull = hello.flags & HELLO_FLAG_PULL != 0;

    if is_pull {
        // Client wants to pull - we are the source
        // Send DEST_FILE_ENTRY for our files (client needs checksums)
        // Then run Generator + Sender
        todo!("Implement pull mode")
    } else {
        // Client wants to push - we are the destination
        // Send DEST_FILE_ENTRY for our files
        // Then receive and write files

        let receiver = Receiver::new(ReceiverConfig {
            root: root_path.clone(),
            block_size: 4096,
        });

        // Send Initial Exchange
        receiver.scan_dest(|bytes| {
            // Use blocking write since we're in async context
            futures::executor::block_on(async {
                stdout.write_all(&bytes).await?;
                Ok::<_, anyhow::Error>(())
            })
        }).await?;
        stdout.flush().await?;

        // Receive streaming messages
        let mut receiver = Receiver::new(ReceiverConfig {
            root: root_path,
            block_size: 4096,
        });

        loop {
            let (msg_type, payload) = v2::read_frame(&mut stdin).await?;

            if msg_type == v2::MessageType::Done {
                break;
            }

            receiver.handle_message(msg_type, payload).await?;
        }

        // Send DONE
        let done = v2::Done {
            files_ok: receiver.stats().files_ok,
            files_err: receiver.stats().files_err,
            bytes: receiver.stats().bytes_transferred,
            duration_ms: 0,
        };
        v2::write_frame(&mut stdout, &done.encode()).await?;
        stdout.flush().await?;
    }

    Ok(())
}
```

### Verification

```bash
cargo check
cargo test server
cargo clippy -- -D warnings
```

---

## Phase 7: Client Integration

**Goal:** Add `--protocol-v2` flag and integrate with SSH transport.

### File to Modify: `src/cli.rs`

Add flag:

```rust
#[arg(long, hide = true)]
pub protocol_v2: bool,
```

### File to Modify: `src/transport/ssh.rs`

Add v2 protocol path in `sync_to_remote()` and `sync_from_remote()`:

```rust
if args.protocol_v2 {
    return self.sync_v2_push(args).await;
}
```

### Implementation

```rust
impl SshTransport {
    async fn sync_v2_push(&self, args: &SyncArgs) -> Result<SyncStats> {
        use crate::streaming::{StreamingSync, protocol::PROTOCOL_VERSION};

        let sync = StreamingSync {
            local_root: args.source.local_path().unwrap().to_path_buf(),
            remote_root: args.dest.path().to_path_buf(),
            delete_enabled: args.delete,
            compress: args.compress,
        };

        let (mut reader, mut writer) = self.open_channel().await?;
        sync.push(&mut reader, &mut writer).await
    }

    async fn sync_v2_pull(&self, args: &SyncArgs) -> Result<SyncStats> {
        use crate::streaming::StreamingSync;

        let sync = StreamingSync {
            local_root: args.dest.local_path().unwrap().to_path_buf(),
            remote_root: args.source.path().to_path_buf(),
            delete_enabled: args.delete,
            compress: args.compress,
        };

        let (mut reader, mut writer) = self.open_channel().await?;
        sync.pull(&mut reader, &mut writer).await
    }
}
```

### Verification

```bash
cargo build --release
# Manual test:
./target/release/sy --protocol-v2 /tmp/src user@host:/tmp/dest
```

---

## Testing Checklist

After each phase, run:

```bash
cargo check
cargo test
cargo clippy -- -D warnings
```

Before merging:

```bash
# Full test suite
cargo test

# SSH tests (if SSH agent available)
cargo test ssh -- --ignored

# Build release
cargo build --release

# Manual smoke test
./target/release/sy /tmp/test_src /tmp/test_dest
./target/release/sy --protocol-v2 /tmp/test_src /tmp/test_dest
```

---

## Notes for Implementer

1. **Don't skip tests** - Each phase has specific tests. Run them.

2. **Check existing code** - The codebase has:
   - `src/delta/generator.rs` - Delta computation
   - `src/delta/applier.rs` - Delta application
   - `src/sync/scanner.rs` - File scanning
   - `src/delta/checksum.rs` - Block checksums

3. **Error handling** - Use `anyhow::Result` for errors. Propagate with `?`.

4. **Commit after each phase** - Don't batch all phases into one commit.

5. **If stuck** - Read `ai/design/streaming-protocol-v0.3.0.md` for the full specification.

6. **Don't break existing v1** - The v1 protocol must continue to work. Version detection routes to v1 or v2.
