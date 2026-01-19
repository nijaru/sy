//! Receiver task for streaming sync.
//!
//! Receives Data messages and writes files to disk.
//! Handles Initial Exchange by sending DEST_FILE_ENTRY.

use crate::streaming::channel::SyncStats;
use crate::streaming::channel::DELTA_MIN_SIZE;
use crate::streaming::protocol::{
    Data, DataEnd, DataFlags, Delete, DeleteEnd, DestFileEnd, DestFileEntry, DestFileFlags,
    FileEnd, FileEntry, MessageType, Mkdir, Symlink,
};
use crate::temp_file::TempFileGuard;
use anyhow::{Context, Result};
use bytes::{Buf, Bytes, BytesMut};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

/// Maximum size for delta copy operations (16MB)
const MAX_DELTA_COPY_SIZE: usize = 16 * 1024 * 1024;

/// Batch size for DEST_FILE_ENTRY messages (64KB)
/// Reduces syscalls by batching multiple encoded frames into single writes
const DEST_ENTRY_BATCH_SIZE: usize = 64 * 1024;

/// Validate that a relative path is safe and doesn't escape the root.
/// Returns the full path if valid.
fn validate_path(root: &Path, relative: &str) -> Result<PathBuf> {
    // Reject empty paths
    if relative.is_empty() {
        anyhow::bail!("Empty path not allowed");
    }

    // Reject absolute paths
    let rel_path = Path::new(relative);
    if rel_path.is_absolute() {
        anyhow::bail!("Absolute paths not allowed: {}", relative);
    }

    // Check for path traversal attempts
    for component in rel_path.components() {
        match component {
            Component::ParentDir => {
                anyhow::bail!("Path traversal not allowed: {}", relative);
            }
            Component::Prefix(_) => {
                anyhow::bail!("Windows prefix paths not allowed: {}", relative);
            }
            _ => {}
        }
    }

    // Build full path and verify it's under root
    let full = root.join(rel_path);

    // Normalize and check (handles edge cases like "foo/../bar")
    let normalized = normalize_path(&full);
    let root_normalized = normalize_path(root);

    if !normalized.starts_with(&root_normalized) {
        anyhow::bail!("Path escapes root directory: {}", relative);
    }

    Ok(full)
}

/// Normalize a path without requiring it to exist (unlike canonicalize)
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            c => normalized.push(c),
        }
    }
    normalized
}

/// Validate symlink target - must be relative and not escape root
fn validate_symlink_target(root: &Path, link_path: &Path, target: &str) -> Result<()> {
    let target_path = Path::new(target);

    // Absolute symlink targets are not allowed
    if target_path.is_absolute() {
        anyhow::bail!(
            "Absolute symlink targets not allowed: {} -> {}",
            link_path.display(),
            target
        );
    }

    // Resolve the target relative to the symlink's parent
    if let Some(link_parent) = link_path.parent() {
        let resolved = link_parent.join(target_path);
        let normalized = normalize_path(&resolved);
        let root_normalized = normalize_path(root);

        if !normalized.starts_with(&root_normalized) {
            anyhow::bail!(
                "Symlink target escapes root: {} -> {}",
                link_path.display(),
                target
            );
        }
    }

    Ok(())
}

/// Receiver configuration
pub struct ReceiverConfig {
    /// Root path for writing files
    pub root: PathBuf,
    /// Block size for checksums
    pub block_size: u32,
}

/// Receiver state
pub struct Receiver {
    config: ReceiverConfig,
    pending_files: HashMap<String, PendingFile>,
    stats: SyncStats,
}

struct PendingFile {
    entry: FileEntry,
    temp_path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
    guard: Option<TempFileGuard>,
}

impl Receiver {
    pub fn new(config: ReceiverConfig) -> Self {
        Self {
            config,
            pending_files: HashMap::new(),
            stats: SyncStats::new(),
        }
    }

    /// Scan destination and yield DEST_FILE_ENTRY messages for Initial Exchange.
    /// Messages are batched to reduce syscalls.
    pub async fn scan_dest<F>(&self, mut on_entry: F) -> Result<(u64, u64)>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        let mut total_files = 0u64;
        let mut total_bytes = 0u64;

        let scanner = crate::sync::scanner::Scanner::new(&self.config.root);
        // Use blocking scan in spawn_blocking
        let entries = tokio::task::spawn_blocking(move || scanner.scan()).await??;

        // Batch buffer for reducing syscalls
        let mut batch = BytesMut::with_capacity(DEST_ENTRY_BATCH_SIZE);

        for entry in entries {
            let rel_path = entry.relative_path.as_ref();
            let path_str = rel_path.to_string_lossy().to_string();

            // Skip root
            if path_str.is_empty() {
                continue;
            }

            let mut flags = DestFileFlags::empty();
            if entry.is_dir {
                flags |= DestFileFlags::DIR;
            }

            // Compute checksums for delta candidates
            let (block_size, checksums) = if !entry.is_dir && entry.size >= DELTA_MIN_SIZE {
                flags |= DestFileFlags::HAS_CHECKSUMS;
                let cs = self.compute_checksums(&entry.path).await?;
                (self.config.block_size, cs)
            } else {
                (0, vec![])
            };

            let mtime = entry
                .modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            // TODO: Scanner should provide mode. For now use 0.
            let mode = if entry.is_dir { 0o755 } else { 0o644 };

            let dest_entry = DestFileEntry {
                path: path_str,
                size: entry.size,
                mtime,
                mode,
                flags,
                block_size,
                checksums,
            };

            // Add to batch
            let encoded = dest_entry.encode();
            batch.extend_from_slice(&encoded);

            // Flush batch when threshold reached
            if batch.len() >= DEST_ENTRY_BATCH_SIZE {
                on_entry(batch.split().freeze())?;
            }

            total_files += 1;
            total_bytes += entry.size;
        }

        // Flush remaining entries
        if !batch.is_empty() {
            on_entry(batch.freeze())?;
        }

        // Send DEST_FILE_END (not batched - signals end of entries)
        let end = DestFileEnd {
            total_files,
            total_bytes,
        };
        on_entry(end.encode())?;

        Ok((total_files, total_bytes))
    }

    async fn compute_checksums(
        &self,
        path: &Path,
    ) -> Result<Vec<crate::streaming::protocol::BlockChecksum>> {
        let p = path.to_path_buf();
        let bs = self.config.block_size as usize;
        let checksums =
            tokio::task::spawn_blocking(move || crate::delta::checksum::compute_checksums(&p, bs))
                .await??;

        Ok(checksums
            .into_iter()
            .map(|c| crate::streaming::protocol::BlockChecksum {
                offset: c.offset,
                weak: c.weak,
                strong: c.strong,
            })
            .collect())
    }

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
            }
            MessageType::DeleteEnd => {
                let _end = DeleteEnd::decode(payload)?;
            }
            _ => {
                // Ignore unknown messages
            }
        }
        Ok(())
    }

    async fn handle_file_entry(&mut self, entry: FileEntry) -> Result<()> {
        let full_path = validate_path(&self.config.root, &entry.path)?;

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Create temp file
        let temp_path = full_path.with_extension("sy.tmp");
        let guard = TempFileGuard::new(&temp_path);

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .await?;

        self.pending_files.insert(
            entry.path.clone(),
            PendingFile {
                entry,
                temp_path,
                file: Some(file),
                bytes_written: 0,
                guard: Some(guard),
            },
        );

        Ok(())
    }

    async fn handle_data(&mut self, data: Data) -> Result<()> {
        let pending = self
            .pending_files
            .get_mut(&data.path)
            .ok_or_else(|| anyhow::anyhow!("No pending file for {}", data.path))?;

        if let Some(ref mut file) = pending.file {
            if data.flags.contains(DataFlags::DELTA) {
                // Apply delta
                Self::apply_delta_static(&self.config.root, file, &data.path, &data.data).await?;
            } else {
                // Write raw data at offset
                file.seek(SeekFrom::Start(data.offset)).await?;
                file.write_all(&data.data).await?;
            }
            pending.bytes_written += data.data.len() as u64;
        }

        Ok(())
    }

    async fn handle_data_end(&mut self, end: DataEnd) -> Result<()> {
        if let Some(mut pending) = self.pending_files.remove(&end.path) {
            if let Some(mut file) = pending.file.take() {
                file.flush().await?;
                file.sync_all().await?;
            }

            // Path was already validated in handle_file_entry
            let full_path = validate_path(&self.config.root, &end.path)?;

            if end.status == DataEnd::STATUS_OK {
                // Move temp file to final destination
                fs::rename(&pending.temp_path, &full_path).await?;

                // Defuse guard after successful rename
                if let Some(guard) = pending.guard.take() {
                    guard.defuse();
                }

                // Set permissions
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(pending.entry.mode);
                    if let Err(e) = fs::set_permissions(&full_path, perms).await {
                        tracing::warn!(
                            "Failed to set permissions on {}: {}",
                            full_path.display(),
                            e
                        );
                    }
                }

                // Set mtime
                let mtime = filetime::FileTime::from_unix_time(pending.entry.mtime, 0);
                let _ = tokio::task::spawn_blocking(move || {
                    filetime::set_file_mtime(&full_path, mtime)
                })
                .await?;

                self.stats.files_ok += 1;
                self.stats.bytes_transferred += pending.bytes_written;
            } else {
                self.stats.files_err += 1;
            }
        }

        Ok(())
    }

    async fn handle_mkdir(&mut self, mkdir: Mkdir) -> Result<()> {
        let full_path = validate_path(&self.config.root, &mkdir.path)?;
        fs::create_dir_all(&full_path).await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mkdir.mode);
            if let Err(e) = fs::set_permissions(&full_path, perms).await {
                tracing::warn!(
                    "Failed to set permissions on {}: {}",
                    full_path.display(),
                    e
                );
            }
        }

        self.stats.dirs_created += 1;
        Ok(())
    }

    async fn handle_symlink(&mut self, symlink: Symlink) -> Result<()> {
        let full_path = validate_path(&self.config.root, &symlink.path)?;

        // Validate symlink target
        validate_symlink_target(&self.config.root, &full_path, &symlink.target)?;

        // Remove existing if any
        let _ = fs::remove_file(&full_path).await;

        #[cfg(unix)]
        tokio::fs::symlink(&symlink.target, &full_path).await?;

        #[cfg(windows)]
        tokio::task::spawn_blocking({
            let target = symlink.target.clone();
            let path = full_path.clone();
            move || std::os::windows::fs::symlink_file(&target, &path)
        })
        .await??;

        self.stats.symlinks_created += 1;
        Ok(())
    }

    async fn handle_delete(&mut self, delete: Delete) -> Result<()> {
        let full_path = validate_path(&self.config.root, &delete.path)?;

        if delete.is_dir {
            let _ = fs::remove_dir_all(&full_path).await;
        } else {
            let _ = fs::remove_file(&full_path).await;
        }

        self.stats.deleted += 1;
        Ok(())
    }

    async fn apply_delta_static(
        root: &Path,
        file: &mut File,
        rel_path: &str,
        delta_data: &[u8],
    ) -> Result<()> {
        // Validate path before opening
        let original_path = validate_path(root, rel_path)?;
        let mut original = File::open(&original_path)
            .await
            .context("Failed to open original file for delta application")?;

        // Get file size for bounds checking
        let file_size = original.metadata().await?.len();

        // Reusable buffer for copy operations
        let mut copy_buf = Vec::new();

        let mut reader = delta_data;

        while reader.has_remaining() {
            let op_type = reader.get_u8();
            match op_type {
                0x00 => {
                    // Copy from original file
                    if reader.remaining() < 12 {
                        anyhow::bail!("Delta copy op truncated");
                    }
                    let offset = reader.get_u64();
                    let size = reader.get_u32() as usize;

                    // Bounds validation
                    if size > MAX_DELTA_COPY_SIZE {
                        anyhow::bail!(
                            "Delta copy size {} exceeds max {}",
                            size,
                            MAX_DELTA_COPY_SIZE
                        );
                    }
                    if offset > file_size {
                        anyhow::bail!(
                            "Delta copy offset {} exceeds file size {}",
                            offset,
                            file_size
                        );
                    }
                    if offset.saturating_add(size as u64) > file_size {
                        anyhow::bail!(
                            "Delta copy range {}..{} exceeds file size {}",
                            offset,
                            offset + size as u64,
                            file_size
                        );
                    }

                    // Reuse buffer
                    copy_buf.resize(size, 0);
                    original.seek(SeekFrom::Start(offset)).await?;
                    original.read_exact(&mut copy_buf).await?;
                    file.write_all(&copy_buf).await?;
                }
                0x01 => {
                    // Insert literal data
                    if reader.remaining() < 4 {
                        anyhow::bail!("Delta insert op truncated");
                    }
                    let len = reader.get_u32() as usize;

                    if len > MAX_DELTA_COPY_SIZE {
                        anyhow::bail!(
                            "Delta insert size {} exceeds max {}",
                            len,
                            MAX_DELTA_COPY_SIZE
                        );
                    }
                    if reader.remaining() < len {
                        anyhow::bail!("Delta insert data truncated");
                    }

                    // Use copy_buf for insert data too
                    copy_buf.resize(len, 0);
                    reader.copy_to_slice(&mut copy_buf);
                    file.write_all(&copy_buf).await?;
                }
                _ => anyhow::bail!("Unknown delta op type: {}", op_type),
            }
        }

        Ok(())
    }

    pub fn stats(&self) -> &SyncStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_receiver_basic() {
        let tmp = TempDir::new().unwrap();
        let config = ReceiverConfig {
            root: tmp.path().to_path_buf(),
            block_size: 4096,
        };
        let mut receiver = Receiver::new(config);

        // Send FileEntry
        let entry = FileEntry {
            path: "test.txt".to_string(),
            size: 11,
            mtime: 1234567890,
            mode: 0o644,
            inode: 0,
            flags: crate::streaming::protocol::FileFlags::empty(),
            symlink_target: None,
            link_target: None,
        };
        receiver
            .handle_message(MessageType::FileEntry, entry.encode().slice(5..))
            .await
            .unwrap();

        // Send Data
        let data = Data {
            path: "test.txt".to_string(),
            offset: 0,
            flags: crate::streaming::protocol::DataFlags::empty(),
            data: Bytes::from("hello world"),
        };
        receiver
            .handle_message(MessageType::Data, data.encode().slice(5..))
            .await
            .unwrap();

        // Send DataEnd
        let end = DataEnd {
            path: "test.txt".to_string(),
            status: DataEnd::STATUS_OK,
        };
        receiver
            .handle_message(MessageType::DataEnd, end.encode().slice(5..))
            .await
            .unwrap();

        // Check file exists and content is correct
        let content = fs::read_to_string(tmp.path().join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }
}
