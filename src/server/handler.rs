use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::server::protocol::{
    self, Action, Decision, FileData, FileDone, FileList, FileListAck, FileListEntry, MessageType,
};
use crate::sync::scanner::{self, ScanOptions};

/// Handle incoming messages on the server side
pub struct ServerHandler {
    root_path: PathBuf,
    file_map: HashMap<String, (u64, i64)>,
}

impl ServerHandler {
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            file_map: HashMap::new(),
        }
    }

    pub async fn handle_file_list<W: AsyncWrite + Unpin>(
        &mut self,
        list: FileList,
        writer: &mut W,
    ) -> Result<()> {
        tracing::info!("Processing file list with {} entries", list.entries.len());

        // 1. Scan local directory to build map
        if !list.entries.is_empty() {
            let scan_opts = ScanOptions::default(); // Use defaults for server side (or config?)

            let root = self.root_path.clone();
            let entries = tokio::task::spawn_blocking(move || {
                scanner::Scanner::new(&root).with_options(scan_opts).scan()
            })
            .await??;

            for entry in entries {
                // Convert absolute path to relative path string
                if let Ok(rel_path) = entry.path.strip_prefix(&self.root_path) {
                    if let Some(path_str) = rel_path.to_str() {
                        let mtime = entry
                            .modified
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        self.file_map
                            .insert(path_str.to_string(), (entry.size, mtime));
                    }
                }
            }
        }

        // 2. Compare and generate decisions
        let mut decisions = Vec::new();

        for (idx, entry) in list.entries.iter().enumerate() {
            let decision = if let Some((size, _mtime)) = self.file_map.get(&entry.path) {
                // File exists, compare
                // Simple size/mtime check for Phase 1
                let size_match = *size == entry.size;

                // Check mtime if available
                // For now, let's just use size + existence.
                // Protocol entry.mtime is i64.

                if size_match {
                    // TODO: More robust check (mtime, checksum if requested)
                    Action::Skip
                } else {
                    Action::Update
                }
            } else {
                // File does not exist
                Action::Create
            };

            decisions.push(Decision {
                index: idx as u32,
                action: decision,
            });
        }

        // 3. Send ACK
        let ack = FileListAck { decisions };
        ack.write(writer).await?;
        writer.flush().await?;

        Ok(())
    }

    pub async fn handle_file_data<W: AsyncWrite + Unpin>(
        &mut self,
        data: FileData,
        writer: &mut W,
        file_list: &[FileListEntry], // We need the original list to know path
    ) -> Result<()> {
        if (data.index as usize) >= file_list.len() {
            return Err(anyhow::anyhow!("Invalid file index: {}", data.index));
        }

        let entry = &file_list[data.index as usize];
        let path = self.root_path.join(&entry.path);

        // Ensure parent dir exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Open file (create or append?)
        // For Phase 1 streaming, we assume sequential chunks or one-shot.
        // Protocol supports offset.

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path)
            .await?;

        file.seek(std::io::SeekFrom::Start(data.offset)).await?;
        file.write_all(&data.data).await?;
        file.flush().await?;

        // Send Done
        // In a real pipeline, we wouldn't send Done for every chunk, only when complete.
        // Protocol: FILE_DATA doesn't have "is_last" flag?
        // We need to track bytes received vs expected size.
        // For Phase 1 MVP, let's assume 1 chunk per file or just send Done always (inefficient but functional)
        // Actually, client expects Done when file is complete.
        // WE NEED STATE TRACKING for partial uploads.

        // Optimization: Check if we received full size
        let meta = file.metadata().await?;
        if meta.len() >= entry.size {
            // File complete
            let done = FileDone {
                index: data.index,
                status: 0,        // OK
                checksum: vec![], // TODO
            };
            done.write(writer).await?;
            writer.flush().await?;
        }

        Ok(())
    }
}
