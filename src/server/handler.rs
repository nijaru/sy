use anyhow::Result;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::compress::{decompress, Compression};
use crate::delta::Adler32;
use crate::server::protocol::{
    Action, BlockChecksum, ChecksumReq, ChecksumResp, Decision, DeltaData, DeltaOp, FileData,
    FileDone, FileList, FileListAck, FileListEntry, MkdirBatch, MkdirBatchAck, SymlinkBatch,
    SymlinkBatchAck, DATA_FLAG_COMPRESSED, STATUS_OK, STATUS_WRITE_ERROR,
};
use crate::sync::scanner::{self, ScanOptions};

/// Represents a file on the destination that we've scanned
struct DestEntry {
    size: u64,
    mtime: i64,
    is_symlink: bool,
    symlink_target: Option<String>,
}

/// Handle incoming messages on the server side
pub struct ServerHandler {
    root_path: PathBuf,
    dest_map: HashMap<String, DestEntry>,
    current_file_list: Vec<FileListEntry>,
}

impl ServerHandler {
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            dest_map: HashMap::new(),
            current_file_list: Vec::new(),
        }
    }

    /// Handle FILE_LIST message: scan destination, compare, return decisions
    pub async fn handle_file_list<W: AsyncWrite + Unpin>(
        &mut self,
        list: FileList,
        writer: &mut W,
    ) -> Result<()> {
        tracing::debug!("Processing file list with {} entries", list.entries.len());

        // Store file list for later reference
        self.current_file_list = list.entries.clone();

        // Scan destination if we have entries to compare
        if !list.entries.is_empty() {
            self.scan_destination().await?;
        }

        // Generate decisions
        let mut decisions = Vec::with_capacity(list.entries.len());

        for (idx, entry) in list.entries.iter().enumerate() {
            let action = self.decide_action(entry);
            decisions.push(Decision {
                index: idx as u32,
                action,
            });
        }

        // Send ACK
        let ack = FileListAck { decisions };
        ack.write(writer).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Scan the destination directory and populate dest_map
    async fn scan_destination(&mut self) -> Result<()> {
        self.dest_map.clear();

        if !self.root_path.exists() {
            return Ok(());
        }

        let scan_opts = ScanOptions::default();
        let root = self.root_path.clone();

        let entries = tokio::task::spawn_blocking(move || {
            scanner::Scanner::new(&root).with_options(scan_opts).scan()
        })
        .await??;

        for entry in entries {
            if let Ok(rel_path) = entry.path.strip_prefix(&self.root_path) {
                if let Some(path_str) = rel_path.to_str() {
                    if path_str.is_empty() {
                        continue;
                    }

                    let mtime = entry
                        .modified
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;

                    let symlink_target = entry
                        .symlink_target
                        .as_ref()
                        .and_then(|t| t.to_str().map(String::from));

                    self.dest_map.insert(
                        path_str.to_string(),
                        DestEntry {
                            size: entry.size,
                            mtime,
                            is_symlink: entry.is_symlink,
                            symlink_target,
                        },
                    );
                }
            }
        }

        Ok(())
    }

    /// Decide what action to take for a source entry
    fn decide_action(&self, entry: &FileListEntry) -> Action {
        // Skip directories - they're handled via MKDIR_BATCH
        if entry.is_dir() {
            return Action::Skip;
        }

        match self.dest_map.get(&entry.path) {
            Some(dest) => {
                // Entry exists on destination
                if entry.is_symlink() {
                    // Compare symlink targets
                    if dest.is_symlink {
                        if entry.symlink_target == dest.symlink_target {
                            Action::Skip
                        } else {
                            Action::Update
                        }
                    } else {
                        // Source is symlink, dest is file - update
                        Action::Update
                    }
                } else if dest.is_symlink {
                    // Source is file, dest is symlink - update
                    Action::Update
                } else {
                    // Both are regular files - compare size/mtime
                    if dest.size == entry.size && dest.mtime >= entry.mtime {
                        Action::Skip
                    } else {
                        Action::Update
                    }
                }
            }
            None => Action::Create,
        }
    }

    /// Handle MKDIR_BATCH message: create directories
    pub async fn handle_mkdir_batch<W: AsyncWrite + Unpin>(
        &mut self,
        batch: MkdirBatch,
        writer: &mut W,
    ) -> Result<()> {
        tracing::debug!("Creating {} directories", batch.paths.len());

        let mut created = 0u32;
        let mut failed = Vec::new();

        for path in batch.paths {
            let full_path = self.root_path.join(&path);
            match fs::create_dir_all(&full_path).await {
                Ok(()) => created += 1,
                Err(e) => {
                    tracing::warn!("Failed to create directory {}: {}", path, e);
                    failed.push((path, e.to_string()));
                }
            }
        }

        let ack = MkdirBatchAck { created, failed };
        ack.write(writer).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Handle SYMLINK_BATCH message: create symlinks
    pub async fn handle_symlink_batch<W: AsyncWrite + Unpin>(
        &mut self,
        batch: SymlinkBatch,
        writer: &mut W,
    ) -> Result<()> {
        tracing::debug!("Creating {} symlinks", batch.entries.len());

        let mut created = 0u32;
        let mut failed = Vec::new();

        for entry in batch.entries {
            let full_path = self.root_path.join(&entry.path);

            // Ensure parent directory exists
            if let Some(parent) = full_path.parent() {
                if let Err(e) = fs::create_dir_all(parent).await {
                    failed.push((entry.path, format!("mkdir parent: {}", e)));
                    continue;
                }
            }

            // Remove existing file/symlink if present
            if full_path.symlink_metadata().is_ok() {
                if let Err(e) = fs::remove_file(&full_path).await {
                    // Try remove_dir for directories
                    if fs::remove_dir(&full_path).await.is_err() {
                        failed.push((entry.path, format!("remove existing: {}", e)));
                        continue;
                    }
                }
            }

            // Create symlink
            #[cfg(unix)]
            match tokio::fs::symlink(&entry.target, &full_path).await {
                Ok(()) => created += 1,
                Err(e) => {
                    tracing::warn!("Failed to create symlink {}: {}", entry.path, e);
                    failed.push((entry.path, e.to_string()));
                }
            }

            #[cfg(windows)]
            {
                // Windows requires different symlink functions for files vs dirs
                let target_path = full_path
                    .parent()
                    .unwrap_or(&self.root_path)
                    .join(&entry.target);
                let result = if target_path.is_dir() {
                    tokio::fs::symlink_dir(&entry.target, &full_path).await
                } else {
                    tokio::fs::symlink_file(&entry.target, &full_path).await
                };
                match result {
                    Ok(()) => created += 1,
                    Err(e) => {
                        tracing::warn!("Failed to create symlink {}: {}", entry.path, e);
                        failed.push((entry.path, e.to_string()));
                    }
                }
            }
        }

        let ack = SymlinkBatchAck { created, failed };
        ack.write(writer).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Handle FILE_DATA message: write file content
    pub async fn handle_file_data<W: AsyncWrite + Unpin>(
        &mut self,
        data: FileData,
        writer: &mut W,
    ) -> Result<()> {
        if (data.index as usize) >= self.current_file_list.len() {
            return Err(anyhow::anyhow!("Invalid file index: {}", data.index));
        }

        let entry = &self.current_file_list[data.index as usize];
        let path = self.root_path.join(&entry.path);

        // Ensure parent dir exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Handle symlinks separately (should use SYMLINK_BATCH, but handle legacy)
        if entry.is_symlink() {
            if let Some(ref target) = entry.symlink_target {
                // Remove existing
                let _ = fs::remove_file(&path).await;

                #[cfg(unix)]
                tokio::fs::symlink(target, &path).await?;

                #[cfg(windows)]
                tokio::fs::symlink_file(target, &path).await?;

                let done = FileDone {
                    index: data.index,
                    status: STATUS_OK,
                    checksum: vec![],
                };
                done.write(writer).await?;
                writer.flush().await?;
                return Ok(());
            }
        }

        // Write regular file
        let status = match self.write_file_data(&path, &data, entry).await {
            Ok(complete) => {
                if complete {
                    // Set permissions if we have mode
                    if entry.mode != 0 {
                        let _ =
                            fs::set_permissions(&path, std::fs::Permissions::from_mode(entry.mode))
                                .await;
                    }
                    Some(STATUS_OK)
                } else {
                    None // Not complete yet, don't send FileDone
                }
            }
            Err(e) => {
                tracing::error!("Failed to write {}: {}", entry.path, e);
                Some(STATUS_WRITE_ERROR)
            }
        };

        if let Some(status) = status {
            let done = FileDone {
                index: data.index,
                status,
                checksum: vec![],
            };
            done.write(writer).await?;
            writer.flush().await?;
        }

        Ok(())
    }

    /// Write file data to disk, returns true if file is complete
    async fn write_file_data(
        &self,
        path: &PathBuf,
        data: &FileData,
        entry: &FileListEntry,
    ) -> Result<bool> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(data.offset == 0) // Truncate on first chunk
            .open(path)
            .await?;

        if data.offset > 0 {
            file.seek(std::io::SeekFrom::Start(data.offset)).await?;
        }

        // Decompress if needed
        let write_data = if data.flags & DATA_FLAG_COMPRESSED != 0 {
            decompress(&data.data, Compression::Zstd)?
        } else {
            data.data.clone()
        };

        file.write_all(&write_data).await?;
        file.flush().await?;

        // Check if file is complete
        let meta = file.metadata().await?;
        Ok(meta.len() >= entry.size)
    }

    /// Get the current file list (for reference by main loop)
    pub fn file_list(&self) -> &[FileListEntry] {
        &self.current_file_list
    }

    /// Handle CHECKSUM_REQ message: compute and return block checksums for delta sync
    pub async fn handle_checksum_req<W: AsyncWrite + Unpin>(
        &mut self,
        req: ChecksumReq,
        writer: &mut W,
    ) -> Result<()> {
        if (req.index as usize) >= self.current_file_list.len() {
            return Err(anyhow::anyhow!("Invalid file index: {}", req.index));
        }

        let entry = &self.current_file_list[req.index as usize];
        let path = self.root_path.join(&entry.path);

        // Compute checksums in blocking task
        let block_size = req.block_size as usize;
        let checksums =
            tokio::task::spawn_blocking(move || compute_block_checksums(&path, block_size))
                .await??;

        let file_size = checksums.iter().map(|c| c.size as u64).sum();

        let resp = ChecksumResp {
            index: req.index,
            file_size,
            checksums,
        };
        resp.write(writer).await?;
        writer.flush().await?;

        Ok(())
    }

    /// Handle DELTA_DATA message: apply delta operations to reconstruct file
    pub async fn handle_delta_data<W: AsyncWrite + Unpin>(
        &mut self,
        delta: DeltaData,
        writer: &mut W,
    ) -> Result<()> {
        if (delta.index as usize) >= self.current_file_list.len() {
            return Err(anyhow::anyhow!("Invalid file index: {}", delta.index));
        }

        let entry = &self.current_file_list[delta.index as usize];
        let path = self.root_path.join(&entry.path);
        let is_compressed = delta.flags & DATA_FLAG_COMPRESSED != 0;

        // Apply delta in blocking task
        let root = self.root_path.clone();
        let status = tokio::task::spawn_blocking(move || {
            apply_delta_ops(&path, &root, &delta.ops, is_compressed)
        })
        .await?;

        let status = match status {
            Ok(()) => {
                // Set permissions
                if entry.mode != 0 {
                    let _ = fs::set_permissions(
                        &self.root_path.join(&entry.path),
                        std::fs::Permissions::from_mode(entry.mode),
                    )
                    .await;
                }
                STATUS_OK
            }
            Err(e) => {
                tracing::error!("Delta apply failed for {}: {}", entry.path, e);
                STATUS_WRITE_ERROR
            }
        };

        let done = FileDone {
            index: delta.index,
            status,
            checksum: vec![],
        };
        done.write(writer).await?;
        writer.flush().await?;

        Ok(())
    }
}

/// Compute block checksums for a file (for delta sync)
/// Optimized: reads in large chunks to minimize syscalls
fn compute_block_checksums(path: &PathBuf, block_size: usize) -> Result<Vec<BlockChecksum>> {
    use std::io::BufReader;

    let file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();

    let num_blocks = (file_size as usize + block_size - 1) / block_size;
    let mut checksums = Vec::with_capacity(num_blocks);

    // Use larger read buffer (1MB) to reduce syscalls
    let read_buf_size = 1024 * 1024;
    let mut reader = BufReader::with_capacity(read_buf_size, file);

    let mut offset = 0u64;
    let mut buf = vec![0u8; block_size];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }

        let block = &buf[..n];

        // Weak checksum: Adler32
        let weak = Adler32::hash(block);

        // Strong checksum: xxHash3 (use direct function, not hasher)
        let strong = xxhash_rust::xxh3::xxh3_64(block);

        checksums.push(BlockChecksum {
            offset,
            size: n as u32,
            weak,
            strong,
        });

        offset += n as u64;
    }

    Ok(checksums)
}

/// Apply delta operations to reconstruct a file
/// Optimized: uses memory mapping for existing file, buffered writer for output
fn apply_delta_ops(
    dest_path: &PathBuf,
    _root: &PathBuf,
    ops: &[DeltaOp],
    is_compressed: bool,
) -> Result<()> {
    use memmap2::Mmap;
    use std::io::BufWriter;

    // Create temp file for reconstruction
    let temp_path = dest_path.with_extension("sy-tmp");

    // Memory-map existing file for fast random access (for Copy ops)
    let existing_file = std::fs::File::open(dest_path).ok();
    let mmap = existing_file
        .as_ref()
        .and_then(|f| unsafe { Mmap::map(f).ok() });

    // Create temp file with buffered writer
    let temp_file = std::fs::File::create(&temp_path)?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, temp_file);

    for op in ops {
        match op {
            DeltaOp::Copy { offset, size } => {
                if let Some(ref map) = mmap {
                    let start = *offset as usize;
                    let end = start + *size as usize;
                    if end <= map.len() {
                        writer.write_all(&map[start..end])?;
                    } else {
                        return Err(anyhow::anyhow!(
                            "Copy op out of bounds: {}..{} > {}",
                            start,
                            end,
                            map.len()
                        ));
                    }
                } else {
                    return Err(anyhow::anyhow!("Copy op but no existing file"));
                }
            }
            DeltaOp::Data(data) => {
                let write_data = if is_compressed {
                    decompress(data, Compression::Zstd)?
                } else {
                    data.clone()
                };
                writer.write_all(&write_data)?;
            }
        }
    }

    writer.flush()?;
    drop(writer);
    drop(mmap);
    drop(existing_file);

    // Atomic replace
    std::fs::rename(&temp_path, dest_path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_handler_empty_dest() {
        let tmp = TempDir::new().unwrap();
        let mut handler = ServerHandler::new(tmp.path().to_path_buf());

        let list = FileList {
            entries: vec![FileListEntry {
                path: "test.txt".to_string(),
                size: 100,
                mtime: 1234567890,
                mode: 0o644,
                flags: 0,
                symlink_target: None,
            }],
        };

        let mut buf = Vec::new();
        handler.handle_file_list(list, &mut buf).await.unwrap();

        // Parse ACK
        let mut cursor = std::io::Cursor::new(&buf[5..]);
        let ack = FileListAck::read(&mut cursor).await.unwrap();

        assert_eq!(ack.decisions.len(), 1);
        assert_eq!(ack.decisions[0].action, Action::Create);
    }

    #[tokio::test]
    async fn test_handler_mkdir_batch() {
        let tmp = TempDir::new().unwrap();
        let mut handler = ServerHandler::new(tmp.path().to_path_buf());

        let batch = MkdirBatch {
            paths: vec!["a/b/c".to_string(), "d/e/f".to_string()],
        };

        let mut buf = Vec::new();
        handler.handle_mkdir_batch(batch, &mut buf).await.unwrap();

        assert!(tmp.path().join("a/b/c").is_dir());
        assert!(tmp.path().join("d/e/f").is_dir());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_handler_symlink_batch() {
        let tmp = TempDir::new().unwrap();
        let mut handler = ServerHandler::new(tmp.path().to_path_buf());

        // Create target file
        std::fs::write(tmp.path().join("target.txt"), "hello").unwrap();

        let batch = SymlinkBatch {
            entries: vec![crate::server::protocol::SymlinkEntry {
                path: "link".to_string(),
                target: "target.txt".to_string(),
            }],
        };

        let mut buf = Vec::new();
        handler.handle_symlink_batch(batch, &mut buf).await.unwrap();

        let link_path = tmp.path().join("link");
        assert!(link_path
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&link_path).unwrap().to_str().unwrap(),
            "target.txt"
        );
    }

    #[tokio::test]
    async fn test_handler_skip_existing() {
        let tmp = TempDir::new().unwrap();

        // Create existing file
        std::fs::write(tmp.path().join("test.txt"), "existing content").unwrap();

        let mut handler = ServerHandler::new(tmp.path().to_path_buf());

        let list = FileList {
            entries: vec![FileListEntry {
                path: "test.txt".to_string(),
                size: 16, // "existing content".len()
                mtime: 0, // Old mtime
                mode: 0o644,
                flags: 0,
                symlink_target: None,
            }],
        };

        let mut buf = Vec::new();
        handler.handle_file_list(list, &mut buf).await.unwrap();

        let mut cursor = std::io::Cursor::new(&buf[5..]);
        let ack = FileListAck::read(&mut cursor).await.unwrap();

        assert_eq!(ack.decisions[0].action, Action::Skip);
    }
}
