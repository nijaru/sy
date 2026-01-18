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
use bytes::{Buf, Bytes};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

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
    _guard: TempFileGuard,
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
    pub async fn scan_dest<F>(&self, mut on_entry: F) -> Result<(u64, u64)>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        let mut total_files = 0u64;
        let mut total_bytes = 0u64;

        let scanner = crate::sync::scanner::Scanner::new(&self.config.root);
        // Use blocking scan in spawn_blocking
        let entries = tokio::task::spawn_blocking(move || scanner.scan()).await??;

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
        let full_path = self.config.root.join(&entry.path);

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
                _guard: guard,
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

            let full_path = self.config.root.join(&end.path);

            if end.status == DataEnd::STATUS_OK {
                // Move temp file to final destination
                fs::rename(&pending.temp_path, &full_path).await?;

                // Set permissions
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(pending.entry.mode);
                    let _ = fs::set_permissions(&full_path, perms).await;
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
        let full_path = self.config.root.join(&mkdir.path);
        fs::create_dir_all(&full_path).await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mkdir.mode);
            let _ = fs::set_permissions(&full_path, perms).await;
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
        // The original file is at the final destination path
        let original_path = root.join(rel_path);
        let mut original = File::open(&original_path)
            .await
            .context("Failed to open original file for delta application")?;

        let mut reader = delta_data;

        while reader.has_remaining() {
            let op_type = reader.get_u8();
            match op_type {
                0x00 => {
                    // Copy
                    let offset = reader.get_u64();
                    let size = reader.get_u32() as usize;

                    let mut buf = vec![0u8; size];
                    original.seek(SeekFrom::Start(offset)).await?;
                    original.read_exact(&mut buf).await?;
                    file.write_all(&buf).await?;
                }
                0x01 => {
                    // Insert
                    let len = reader.get_u32() as usize;
                    let mut buf = vec![0u8; len];
                    reader.copy_to_slice(&mut buf);
                    file.write_all(&buf).await?;
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
