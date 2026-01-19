//! Sender task for streaming sync.
//!
//! Receives FileJobs from Generator, reads file content,
//! computes deltas when possible, and sends Data chunks.

use crate::delta::generator::{generate_delta_streaming, DeltaOp};
use crate::streaming::channel::{
    DeltaInfo, FileJob, FileJobReceiver, GeneratorMessage, DATA_CHUNK_SIZE, DELTA_CHUNK_SIZE,
};
use crate::streaming::protocol::{
    Data, DataEnd, DataFlags, Delete, DeleteEnd, FileEnd, FileEntry, FileFlags, Mkdir, Symlink,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};

/// Sender configuration
pub struct SenderConfig {
    /// Root path for reading files
    pub root: PathBuf,
    /// Whether to compress data
    pub compress: bool,
}

/// Sender state
pub struct Sender {
    config: SenderConfig,
}

impl Sender {
    pub fn new(config: SenderConfig) -> Self {
        Self { config }
    }

    /// Run the sender, processing FileJobs and outputting Data messages.
    /// Returns encoded Data messages via callback.
    pub async fn run<F>(self, mut rx: FileJobReceiver, mut on_data: F) -> Result<()>
    where
        F: FnMut(Bytes) -> Result<()>,
    {
        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::File(job) => {
                    self.process_file(job, &mut on_data).await?;
                }
                GeneratorMessage::Mkdir { path, mode } => {
                    let msg = Mkdir {
                        path: path.to_string_lossy().to_string(),
                        mode,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::Symlink { path, target } => {
                    let msg = Symlink {
                        path: path.to_string_lossy().to_string(),
                        target,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::Delete { path, is_dir } => {
                    let msg = Delete {
                        path: path.to_string_lossy().to_string(),
                        is_dir,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::FileEnd {
                    total_files,
                    total_bytes,
                } => {
                    let msg = FileEnd {
                        total_files,
                        total_bytes,
                    };
                    on_data(msg.encode())?;
                }
                GeneratorMessage::DeleteEnd { count } => {
                    let msg = DeleteEnd { count };
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
        let entry = FileEntry {
            path: path_str.clone(),
            size: job.size,
            mtime: job.mtime,
            mode: job.mode,
            inode: job.inode,
            flags: FileFlags::empty(),
            symlink_target: None,
            link_target: None,
        };
        on_data(entry.encode())?;

        // Read and send data chunks
        if job.need_delta && job.checksums.is_some() {
            // Delta transfer
            self.send_delta(&full_path, &path_str, job.checksums.unwrap(), on_data)
                .await?;
        } else {
            // Full transfer
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
        let file = File::open(path)
            .await
            .context("Failed to open file for full transfer")?;
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
        // Convert protocol checksums to delta engine checksums
        let block_size = delta_info.block_size as usize;
        let file_size = delta_info.file_size;
        let num_checksums = delta_info.checksums.len();

        let dest_checksums: Vec<_> = delta_info
            .checksums
            .iter()
            .enumerate()
            .map(|(i, c)| {
                // Calculate actual block size - last block may be smaller
                let actual_size = if i == num_checksums - 1 {
                    // Last block: calculate remaining bytes
                    let remaining = file_size.saturating_sub(c.offset);
                    remaining.min(block_size as u64) as usize
                } else {
                    block_size
                };

                crate::delta::BlockChecksum {
                    index: i as u64,
                    offset: c.offset,
                    size: actual_size,
                    weak: c.weak,
                    strong: c.strong,
                }
            })
            .collect();

        // generate_delta_streaming is blocking
        let p = path.to_path_buf();
        let delta = tokio::task::spawn_blocking(move || {
            generate_delta_streaming(&p, &dest_checksums, block_size)
        })
        .await??;

        // Encode delta ops into DATA messages, chunking to avoid frame size limits
        let mut flags = DataFlags::DELTA;
        if self.config.compress {
            flags |= DataFlags::COMPRESSED;
        }

        // Serialize delta ops, chunking into multiple messages if needed
        let mut delta_bytes = Vec::new();
        let mut chunk_offset = 0u64;

        for op in delta.ops {
            // Serialize the op
            let op_bytes = match &op {
                DeltaOp::Copy { offset, size } => {
                    let mut buf = Vec::with_capacity(13);
                    buf.push(0x00);
                    buf.extend_from_slice(&offset.to_be_bytes());
                    buf.extend_from_slice(&(*size as u32).to_be_bytes());
                    buf
                }
                DeltaOp::Data(data) => {
                    let mut buf = Vec::with_capacity(5 + data.len());
                    buf.push(0x01);
                    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
                    buf.extend_from_slice(data);
                    buf
                }
            };

            // Check if adding this op would exceed chunk size
            if !delta_bytes.is_empty() && delta_bytes.len() + op_bytes.len() > DELTA_CHUNK_SIZE {
                // Flush current chunk
                let data = Data {
                    path: path_str.to_string(),
                    offset: chunk_offset,
                    flags,
                    data: Bytes::from(std::mem::take(&mut delta_bytes)),
                };
                on_data(data.encode())?;
                chunk_offset += 1; // Increment to signal continuation
            }

            delta_bytes.extend(op_bytes);
        }

        // Flush remaining ops
        if !delta_bytes.is_empty() {
            let data = Data {
                path: path_str.to_string(),
                offset: chunk_offset,
                flags,
                data: Bytes::from(delta_bytes),
            };
            on_data(data.encode())?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

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
        }))
        .await
        .unwrap();

        tx.send(GeneratorMessage::FileEnd {
            total_files: 1,
            total_bytes: 11,
        })
        .await
        .unwrap();

        drop(tx);

        let mut messages = Vec::new();
        sender
            .run(rx, |bytes| {
                messages.push(bytes);
                Ok(())
            })
            .await
            .unwrap();

        // Should have: FileEntry, Data, DataEnd, FileEnd
        assert!(messages.len() >= 4);
    }
}
