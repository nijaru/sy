//! Sender task for streaming sync.
//!
//! Receives FileJobs from Generator, reads file content, computes deltas when
//! possible, and sends encoded frames through a bounded asynchronous wire queue.

use crate::compress::CompressionDetection;
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
use tokio::sync::mpsc;

/// Sender configuration.
pub struct SenderConfig {
    /// Root path for reading files.
    pub root: PathBuf,
    /// Compression mode.
    pub compress: CompressionDetection,
    /// Optional bandwidth limit in bytes per second.
    pub bwlimit: Option<u64>,
}

/// Sender state.
pub struct Sender {
    config: SenderConfig,
}

impl Sender {
    pub fn new(config: SenderConfig) -> Self {
        Self { config }
    }

    /// Process file jobs and emit encoded frames to a bounded wire queue.
    ///
    /// Awaiting `send` is intentional: network backpressure propagates through
    /// the sender to the generator instead of accumulating file data in memory.
    pub async fn run(self, mut rx: FileJobReceiver, wire_tx: mpsc::Sender<Bytes>) -> Result<()> {
        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::File(job) => self.process_file(job, &wire_tx).await?,
                GeneratorMessage::Mkdir { path, mode } => {
                    send_frame(
                        &wire_tx,
                        Mkdir {
                            path: path.to_string_lossy().to_string(),
                            mode,
                        }
                        .encode()?,
                    )
                    .await?;
                }
                GeneratorMessage::Symlink { path, target } => {
                    send_frame(
                        &wire_tx,
                        Symlink {
                            path: path.to_string_lossy().to_string(),
                            target,
                        }
                        .encode()?,
                    )
                    .await?;
                }
                GeneratorMessage::Delete { path, is_dir } => {
                    send_frame(
                        &wire_tx,
                        Delete {
                            path: path.to_string_lossy().to_string(),
                            is_dir,
                        }
                        .encode()?,
                    )
                    .await?;
                }
                GeneratorMessage::FileEnd {
                    total_files,
                    total_bytes,
                } => {
                    send_frame(
                        &wire_tx,
                        FileEnd {
                            total_files,
                            total_bytes,
                        }
                        .encode()?,
                    )
                    .await?;
                }
                GeneratorMessage::DeleteEnd { count } => {
                    send_frame(&wire_tx, DeleteEnd { count }.encode()?).await?;
                }
            }
        }
        Ok(())
    }

    async fn process_file(&self, job: FileJob, wire_tx: &mpsc::Sender<Bytes>) -> Result<()> {
        let path_str = job.path.to_string_lossy().to_string();
        let full_path = self.config.root.join(job.path.as_ref());

        send_frame(
            wire_tx,
            FileEntry {
                path: path_str.clone(),
                size: job.size,
                mtime: job.mtime,
                mode: job.mode,
                inode: job.inode,
                flags: FileFlags::empty(),
                symlink_target: None,
                link_target: None,
            }
            .encode()?,
        )
        .await?;

        match (job.need_delta, job.checksums) {
            (true, Some(checksums)) => {
                self.send_delta(&full_path, &path_str, checksums, wire_tx)
                    .await?;
            }
            _ => self.send_full(&full_path, &path_str, wire_tx).await?,
        }

        send_frame(
            wire_tx,
            DataEnd {
                path: path_str,
                status: DataEnd::STATUS_OK,
            }
            .encode()?,
        )
        .await
    }

    async fn send_full(
        &self,
        path: &Path,
        path_str: &str,
        wire_tx: &mpsc::Sender<Bytes>,
    ) -> Result<()> {
        let file = File::open(path)
            .await
            .context("Failed to open file for full transfer")?;
        let mut reader = BufReader::new(file);
        let mut offset = 0_u64;
        let mut buf = vec![0_u8; DATA_CHUNK_SIZE];
        let mut limiter = self
            .config
            .bwlimit
            .map(crate::sync::ratelimit::RateLimiter::new);
        let compression_allowed = self.config.compress != CompressionDetection::Never
            && !crate::compress::is_compressed_extension(path_str);

        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }

            if let Some(ref mut limiter) = limiter {
                let sleep_duration = limiter.consume(n as u64);
                if !sleep_duration.is_zero() {
                    tokio::time::sleep(sleep_duration).await;
                }
            }

            let (compressed, payload) = encode_payload(&buf[..n], compression_allowed);
            let mut flags = DataFlags::empty();
            if compressed {
                flags |= DataFlags::COMPRESSED;
            }

            send_frame(
                wire_tx,
                Data {
                    path: path_str.to_string(),
                    offset,
                    flags,
                    data: payload,
                }
                .encode()?,
            )
            .await?;

            offset += n as u64;
        }

        Ok(())
    }

    async fn send_delta(
        &self,
        path: &Path,
        path_str: &str,
        delta_info: DeltaInfo,
        wire_tx: &mpsc::Sender<Bytes>,
    ) -> Result<()> {
        let block_size = delta_info.block_size as usize;
        let file_size = delta_info.file_size;
        let checksum_count = delta_info.checksums.len();

        let dest_checksums: Vec<_> = delta_info
            .checksums
            .iter()
            .enumerate()
            .map(|(index, checksum)| {
                let actual_size = if index + 1 == checksum_count {
                    file_size
                        .saturating_sub(checksum.offset)
                        .min(block_size as u64) as usize
                } else {
                    block_size
                };

                crate::delta::BlockChecksum {
                    index: index as u64,
                    offset: checksum.offset,
                    size: actual_size,
                    weak: checksum.weak,
                    strong: checksum.strong,
                }
            })
            .collect();

        let source_path = path.to_path_buf();
        let delta = tokio::task::spawn_blocking(move || {
            generate_delta_streaming(&source_path, &dest_checksums, block_size)
        })
        .await??;

        let compression_allowed = self.config.compress != CompressionDetection::Never
            && !crate::compress::is_compressed_extension(path_str);
        let mut delta_bytes = Vec::new();

        for op in delta.ops {
            let op_bytes = encode_delta_op(op)?;
            if !delta_bytes.is_empty() && delta_bytes.len() + op_bytes.len() > DELTA_CHUNK_SIZE {
                self.send_delta_chunk(
                    path_str,
                    std::mem::take(&mut delta_bytes),
                    compression_allowed,
                    wire_tx,
                )
                .await?;
            }
            delta_bytes.extend(op_bytes);
        }

        if !delta_bytes.is_empty() {
            self.send_delta_chunk(path_str, delta_bytes, compression_allowed, wire_tx)
                .await?;
        }

        Ok(())
    }

    async fn send_delta_chunk(
        &self,
        path_str: &str,
        raw: Vec<u8>,
        compression_allowed: bool,
        wire_tx: &mpsc::Sender<Bytes>,
    ) -> Result<()> {
        let (compressed, payload) = encode_owned_payload(raw, compression_allowed);
        let mut flags = DataFlags::DELTA;
        if compressed {
            flags |= DataFlags::COMPRESSED;
        }

        send_frame(
            wire_tx,
            Data {
                path: path_str.to_string(),
                offset: 0,
                flags,
                data: payload,
            }
            .encode()?,
        )
        .await
    }
}

async fn send_frame(wire_tx: &mpsc::Sender<Bytes>, frame: Bytes) -> Result<()> {
    wire_tx
        .send(frame)
        .await
        .map_err(|_| anyhow::anyhow!("wire output channel closed"))
}

fn encode_payload(raw: &[u8], compression_allowed: bool) -> (bool, Bytes) {
    if compression_allowed {
        if let Ok(compressed) = crate::compress::compress(raw, crate::compress::Compression::Lz4) {
            if compressed.len() < raw.len() {
                return (true, Bytes::from(compressed));
            }
        }
    }
    (false, Bytes::copy_from_slice(raw))
}

fn encode_owned_payload(raw: Vec<u8>, compression_allowed: bool) -> (bool, Bytes) {
    if compression_allowed {
        if let Ok(compressed) = crate::compress::compress(&raw, crate::compress::Compression::Lz4) {
            if compressed.len() < raw.len() {
                return (true, Bytes::from(compressed));
            }
        }
    }
    (false, Bytes::from(raw))
}

fn encode_delta_op(op: DeltaOp) -> Result<Vec<u8>> {
    match op {
        DeltaOp::Copy { offset, size } => {
            let size = u32::try_from(size).context("delta copy operation exceeds u32 wire size")?;
            let mut buf = Vec::with_capacity(13);
            buf.push(0x00);
            buf.extend_from_slice(&offset.to_be_bytes());
            buf.extend_from_slice(&size.to_be_bytes());
            Ok(buf)
        }
        DeltaOp::Data(data) => {
            let size = u32::try_from(data.len()).context("delta literal exceeds u32 wire size")?;
            let mut buf = Vec::with_capacity(5 + data.len());
            buf.push(0x01);
            buf.extend_from_slice(&size.to_be_bytes());
            buf.extend_from_slice(&data);
            Ok(buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::channel::SENDER_CHANNEL_SIZE;
    use crate::streaming::protocol::BlockChecksum;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn collect_wire(sender: Sender, rx: FileJobReceiver) -> Vec<Bytes> {
        let (wire_tx, mut wire_rx) = mpsc::channel(SENDER_CHANNEL_SIZE);
        sender.run(rx, wire_tx).await.unwrap();

        let mut messages = Vec::new();
        while let Some(frame) = wire_rx.recv().await {
            messages.push(frame);
        }
        messages
    }

    fn data_flags(frame: &[u8]) -> DataFlags {
        let path_len = u16::from_be_bytes([frame[5], frame[6]]) as usize;
        let flags_offset = 4 + 1 + 2 + path_len + 8;
        DataFlags::from_bits_retain(frame[flags_offset])
    }

    #[tokio::test]
    async fn sender_simple_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("test.txt"), "hello world").unwrap();
        let sender = Sender::new(SenderConfig {
            root: tmp.path().to_path_buf(),
            compress: CompressionDetection::Never,
            bwlimit: None,
        });
        let (tx, rx) = crate::streaming::channel::file_job_channel();

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

        let messages = collect_wire(sender, rx).await;
        assert!(messages.len() >= 4);
    }

    #[tokio::test]
    async fn sender_delta_file_sets_delta_flag() {
        let tmp = TempDir::new().unwrap();
        let content = "new content that differs from original";
        fs::write(tmp.path().join("test.txt"), content).unwrap();
        let sender = Sender::new(SenderConfig {
            root: tmp.path().to_path_buf(),
            compress: CompressionDetection::Never,
            bwlimit: None,
        });
        let (tx, rx) = crate::streaming::channel::file_job_channel();
        let delta_info = DeltaInfo {
            block_size: 16,
            file_size: 32,
            checksums: vec![
                BlockChecksum {
                    offset: 0,
                    weak: 0xDEADBEEF,
                    strong: 0,
                },
                BlockChecksum {
                    offset: 16,
                    weak: 0xCAFEBABE,
                    strong: 1,
                },
            ],
        };

        tx.send(GeneratorMessage::File(FileJob {
            path: Arc::new(PathBuf::from("test.txt")),
            size: content.len() as u64,
            mtime: 0,
            mode: 0o644,
            inode: 0,
            need_delta: true,
            checksums: Some(delta_info),
        }))
        .await
        .unwrap();
        tx.send(GeneratorMessage::FileEnd {
            total_files: 1,
            total_bytes: content.len() as u64,
        })
        .await
        .unwrap();
        drop(tx);

        let messages = collect_wire(sender, rx).await;
        assert!(messages.len() >= 4);
        assert_eq!(messages[1][4], 0x06);
        assert!(data_flags(&messages[1]).contains(DataFlags::DELTA));
    }

    #[tokio::test]
    async fn delta_messages_use_zero_offset() {
        let tmp = TempDir::new().unwrap();
        let content = "a".repeat(100_000);
        fs::write(tmp.path().join("large.txt"), &content).unwrap();
        let sender = Sender::new(SenderConfig {
            root: tmp.path().to_path_buf(),
            compress: CompressionDetection::Never,
            bwlimit: None,
        });
        let (tx, rx) = crate::streaming::channel::file_job_channel();
        let delta_info = DeltaInfo {
            block_size: 1024,
            file_size: 2048,
            checksums: vec![
                BlockChecksum {
                    offset: 0,
                    weak: 0x12345678,
                    strong: 0x99,
                },
                BlockChecksum {
                    offset: 1024,
                    weak: 0x87654321,
                    strong: 0x88,
                },
            ],
        };

        tx.send(GeneratorMessage::File(FileJob {
            path: Arc::new(PathBuf::from("large.txt")),
            size: content.len() as u64,
            mtime: 0,
            mode: 0o644,
            inode: 0,
            need_delta: true,
            checksums: Some(delta_info),
        }))
        .await
        .unwrap();
        tx.send(GeneratorMessage::FileEnd {
            total_files: 1,
            total_bytes: content.len() as u64,
        })
        .await
        .unwrap();
        drop(tx);

        for frame in collect_wire(sender, rx).await {
            if frame.len() > 4 && frame[4] == 0x06 {
                let path_len = u16::from_be_bytes([frame[5], frame[6]]) as usize;
                let offset_start = 4 + 1 + 2 + path_len;
                let offset = u64::from_be_bytes(
                    frame[offset_start..offset_start + 8]
                        .try_into()
                        .expect("fixed-size offset slice"),
                );
                assert_eq!(offset, 0);
            }
        }
    }

    #[test]
    fn compression_flag_tracks_actual_compression() {
        let compressible = vec![b'a'; 16 * 1024];
        let (compressed, payload) = encode_payload(&compressible, true);
        assert!(compressed);
        assert!(payload.len() < compressible.len());

        let mut state = 0x9e37_79b9_u32;
        let incompressible: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let (compressed, payload) = encode_payload(&incompressible, true);
        if compressed {
            assert!(payload.len() < incompressible.len());
        } else {
            assert_eq!(payload.as_ref(), incompressible.as_slice());
        }
    }
}
