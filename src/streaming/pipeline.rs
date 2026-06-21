//! Streaming sync pipeline.
//!
//! Orchestrates Generator, Sender, and Receiver tasks.

use crate::compress::CompressionDetection;
use crate::filter::FilterEngine;
use crate::streaming::{
    channel::{file_job_channel, SyncStats},
    protocol::{read_frame, write_frame, Done, Hello, HelloFlags, MessageType},
    Generator, GeneratorConfig, Receiver, ReceiverConfig, Sender, SenderConfig,
};
use crate::sync::scanner::ScanOptions;
use anyhow::Result;
use bytes::Bytes;
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Orchestrator for streaming sync
pub struct StreamingSync {
    pub local_root: PathBuf,
    pub remote_root: PathBuf,
    pub delete_enabled: bool,
    pub force_delete: bool,
    pub max_delete: Option<String>,
    pub compress: CompressionDetection,
    pub filter: Option<FilterEngine>,
    pub dry_run: bool,
    pub scan_options: ScanOptions,
}

impl StreamingSync {
    pub fn new(
        local_root: PathBuf,
        remote_root: PathBuf,
        delete_enabled: bool,
        compress: CompressionDetection,
    ) -> Self {
        Self {
            local_root,
            remote_root,
            delete_enabled,
            force_delete: false,
            max_delete: None,
            compress,
            filter: None,
            dry_run: false,
            scan_options: ScanOptions::default(),
        }
    }

    /// Set max-delete threshold
    pub fn with_max_delete(mut self, max_delete: String) -> Self {
        self.max_delete = Some(max_delete);
        self
    }

    /// Set force-delete to bypass threshold
    pub fn with_force_delete(mut self, force: bool) -> Self {
        self.force_delete = force;
        self
    }

    /// Set filter engine for --exclude/--include/--filter
    pub fn with_filter(mut self, filter: FilterEngine) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Set dry-run mode
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Set scanner options for .gitignore and VCS directory handling.
    pub fn with_scan_options(mut self, scan_options: ScanOptions) -> Self {
        self.scan_options = scan_options;
        self
    }

    /// Run a push sync (local -> remote).
    pub async fn push<R, W>(&self, reader: &mut R, writer: &mut W) -> Result<SyncStats>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        // 1. Send HELLO
        let mut hello_flags = HelloFlags::empty();
        if self.dry_run {
            hello_flags |= HelloFlags::DRY_RUN;
        }
        let hello = Hello::new(hello_flags, self.remote_root.to_string_lossy().into_owned());
        write_frame(writer, &hello.encode()).await?;
        writer.flush().await?;

        // 2. Receive HELLO response
        let (msg_type, payload) = read_frame(reader).await?;
        if msg_type != MessageType::Hello {
            anyhow::bail!("Expected Hello response, got {:?}", msg_type);
        }
        let _server_hello = Hello::decode(payload)?;

        // 3. Receive DEST_FILE_ENTRY messages (Initial Exchange)
        let mut generator = Generator::new(GeneratorConfig {
            root: self.local_root.clone(),
            include_hidden: true,
            follow_symlinks: false,
            delete_enabled: self.delete_enabled,
            force_delete: self.force_delete,
            max_delete: self.max_delete.clone(),
            filter: self.filter.clone(),
            scan_options: self.scan_options,
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
                MessageType::Fatal => {
                    let fatal = crate::streaming::protocol::Fatal::decode(payload)?;
                    anyhow::bail!("Remote fatal error: {}", fatal.message);
                }
                _ => {
                    anyhow::bail!("Unexpected message during Initial Exchange: {:?}", msg_type);
                }
            }
        }

        // 4. Run Generator and Sender
        let (tx, rx) = file_job_channel();

        let gen_handle = tokio::spawn(async move { generator.run(tx).await });

        let sender = Sender::new(SenderConfig {
            root: self.local_root.clone(),
            compress: self.compress,
        });

        // Use unbounded channel to avoid blocking_send (panics in tokio context)
        let (data_tx, mut data_rx) = mpsc::unbounded_channel::<Bytes>();

        // Spawn sender - uses unbounded_send which never blocks
        let sender_handle = tokio::spawn(async move {
            sender
                .run(rx, |bytes| {
                    data_tx
                        .send(bytes)
                        .map_err(|_| anyhow::anyhow!("Data channel closed"))
                })
                .await
        });

        // Pipe data to writer concurrently with sender
        // In dry-run mode, we still run the pipeline to compute stats,
        // but we don't send any data to the server.
        if !self.dry_run {
            while let Some(bytes) = data_rx.recv().await {
                writer.write_all(&bytes).await?;
            }
        } else {
            // Drain the channel without sending
            while data_rx.recv().await.is_some() {}
        }

        // Send DONE (Wait, DONE is sent by Receiver)
        // Wait, the Sender should send a final message to signal completion
        // Protocol v2 uses DONE (0x10) from Receiver to Client.
        // Client doesn't send DONE. It just finishes sending messages.
        // But we should signal the server that we are done.
        // Let's use DONE with 0 values or just close the stream?
        // Actually, the protocol says DONE is from R->client.
        // Maybe we need a message from client to server to say "I'm finished sending".
        // Let's use Done message but client side.
        // Wait for generator to complete first (may fail on deletion threshold)
        let gen_result = gen_handle.await?;
        sender_handle.await??;

        // Send DONE only after generator completes
        let client_done = Done {
            files_ok: 0,
            files_err: 0,
            bytes: 0,
            duration_ms: 0,
            files_scanned: 0,
        };
        write_frame(writer, &client_done.encode()).await?;
        writer.flush().await?;

        // Propagate generator error (e.g., deletion threshold exceeded)
        let (total_files, total_bytes, source_scanned) = gen_result?;

        // Finally receive DONE from server
        let (msg_type, payload) = read_frame(reader).await?;
        if msg_type == MessageType::Done {
            let done = Done::decode(payload)?;
            // In push mode, server doesn't know source scan count — use local generator's.
            let scanned = if done.files_scanned > 0 {
                done.files_scanned
            } else {
                source_scanned
            };
            Ok(SyncStats {
                files_ok: done.files_ok,
                files_err: done.files_err,
                bytes_transferred: done.bytes,
                files_scanned: scanned,
                ..Default::default()
            })
        } else {
            Ok(SyncStats {
                files_ok: total_files,
                bytes_transferred: total_bytes,
                files_scanned: source_scanned,
                ..Default::default()
            })
        }
    }

    /// Run a pull sync (remote -> local).
    pub async fn pull<R, W>(&self, reader: &mut R, writer: &mut W) -> Result<SyncStats>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        // 1. Send HELLO with PULL flag
        let mut flags = HelloFlags::PULL;
        if self.delete_enabled {
            flags |= HelloFlags::DELETE;
        }
        if self.compress != CompressionDetection::Never {
            flags |= HelloFlags::COMPRESSION;
        }
        if self.force_delete {
            flags |= HelloFlags::FORCE_DELETE;
        }
        if self.scan_options.respect_gitignore {
            flags |= HelloFlags::RESPECT_GITIGNORE;
        }
        if !self.scan_options.include_git_dir {
            flags |= HelloFlags::EXCLUDE_GIT_DIR;
        }
        if self.scan_options.dirs_only {
            flags |= HelloFlags::DIRS_ONLY;
        }

        let filter_patterns = self.filter.as_ref().map(|f| f.to_rule_strings().join("\n"));

        let hello = Hello::new(flags, self.remote_root.to_string_lossy().into_owned())
            .with_max_delete(self.max_delete.clone())
            .with_filter_patterns(filter_patterns);
        write_frame(writer, &hello.encode()).await?;
        writer.flush().await?;

        // 2. Receive HELLO response
        let (msg_type, payload) = read_frame(reader).await?;
        if msg_type != MessageType::Hello {
            anyhow::bail!("Expected Hello response, got {:?}", msg_type);
        }
        let _server_hello = Hello::decode(payload)?;

        // Ensure local root exists
        if !self.local_root.exists() {
            tokio::fs::create_dir_all(&self.local_root).await?;
        }

        // 3. Send DEST_FILE_ENTRY messages (Initial Exchange)
        // Use unbounded channel to avoid blocking_send (panics in tokio context)
        let (data_tx, mut data_rx) = mpsc::unbounded_channel::<Bytes>();
        let receiver_root = self.local_root.clone();

        // Spawn scanner - uses unbounded_send which never blocks
        let scan_handle = tokio::spawn(async move {
            let receiver = Receiver::new(ReceiverConfig {
                root: receiver_root,
                block_size: 4096,
            });
            receiver
                .scan_dest(|bytes| {
                    data_tx
                        .send(bytes)
                        .map_err(|_| anyhow::anyhow!("Data channel closed"))
                })
                .await
        });

        // Write data as it arrives (concurrent with scan)
        while let Some(bytes) = data_rx.recv().await {
            writer.write_all(&bytes).await?;
        }
        writer.flush().await?;

        // Wait for scanner to complete
        scan_handle.await??;

        // 4. Receive and process streaming messages
        let mut receiver = Receiver::new(ReceiverConfig {
            root: self.local_root.clone(),
            block_size: 4096,
        });

        loop {
            let (msg_type, payload) = read_frame(reader).await?;

            if msg_type == MessageType::Done {
                let done = Done::decode(payload)?;
                let mut stats = receiver.stats().clone();
                stats.files_ok = done.files_ok;
                stats.files_err = done.files_err;
                stats.bytes_transferred = done.bytes;
                stats.files_scanned = done.files_scanned;
                return Ok(stats);
            }

            receiver.handle_message(msg_type, payload).await?;
        }
    }
}
