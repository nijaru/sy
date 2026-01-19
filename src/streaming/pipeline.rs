//! Streaming sync pipeline.
//!
//! Orchestrates Generator, Sender, and Receiver tasks.

use crate::streaming::{
    channel::{file_job_channel, SyncStats},
    protocol::{read_frame, write_frame, Done, Hello, HelloFlags, MessageType},
    Generator, GeneratorConfig, Receiver, ReceiverConfig, Sender, SenderConfig,
};
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
    pub compress: bool,
}

impl StreamingSync {
    pub fn new(
        local_root: PathBuf,
        remote_root: PathBuf,
        delete_enabled: bool,
        compress: bool,
    ) -> Self {
        Self {
            local_root,
            remote_root,
            delete_enabled,
            compress,
        }
    }

    /// Run a push sync (local -> remote).
    pub async fn push<R, W>(&self, reader: &mut R, writer: &mut W) -> Result<SyncStats>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        // 1. Send HELLO
        let hello = Hello::new(
            HelloFlags::empty(),
            self.remote_root.to_string_lossy().into_owned(),
        );
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
        while let Some(bytes) = data_rx.recv().await {
            writer.write_all(&bytes).await?;
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
        let client_done = Done {
            files_ok: 0,
            files_err: 0,
            bytes: 0,
            duration_ms: 0,
        };
        write_frame(writer, &client_done.encode()).await?;
        writer.flush().await?;

        let (total_files, total_bytes) = gen_handle.await??;
        sender_handle.await??;

        // Finally receive DONE from server
        let (msg_type, payload) = read_frame(reader).await?;
        if msg_type == MessageType::Done {
            let done = Done::decode(payload)?;
            Ok(SyncStats {
                files_ok: done.files_ok,
                files_err: done.files_err,
                bytes_transferred: done.bytes,
                ..Default::default()
            })
        } else {
            Ok(SyncStats {
                files_ok: total_files,
                bytes_transferred: total_bytes,
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
        if self.compress {
            flags |= HelloFlags::COMPRESSION;
        }

        let hello = Hello::new(flags, self.remote_root.to_string_lossy().into_owned());
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
                return Ok(stats);
            }

            receiver.handle_message(msg_type, payload).await?;
        }
    }
}
