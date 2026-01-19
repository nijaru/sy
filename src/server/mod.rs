//! Server mode - runs when invoked as `sy --server <path>`
//!
//! Uses streaming protocol (v2) for all operations.
//!
//! Code appears "dead" to the compiler since it's only used at runtime.
#![allow(dead_code)]

use anyhow::Result;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{self, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::streaming::{
    channel::file_job_channel,
    protocol::{self as v2, HelloFlags, MessageType},
    Generator, GeneratorConfig, Receiver, ReceiverConfig, Sender, SenderConfig,
};

/// Expand tilde (~) in paths to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();

    if path_str == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else if let Some(rest) = path_str.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest)
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    }
}

/// Main server entry point
pub async fn run_server() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let raw_path = args
        .last()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let root_path = expand_tilde(&raw_path);

    if !root_path.exists() {
        std::fs::create_dir_all(&root_path)?;
    }

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    // Read Hello frame
    let (msg_type, payload) = v2::read_frame(&mut stdin).await?;

    if msg_type != MessageType::Hello {
        let fatal = v2::Fatal {
            code: 1,
            message: format!("Expected HELLO, got {:?}", msg_type),
        };
        v2::write_frame(&mut stdout, &fatal.encode()).await?;
        stdout.flush().await?;
        return Ok(());
    }

    let hello = v2::Hello::decode(payload)?;

    // Ensure root exists
    if !root_path.exists() {
        fs::create_dir_all(&root_path).await?;
    }

    // Send Hello response
    let resp = v2::Hello::new(HelloFlags::empty(), "");
    v2::write_frame(&mut stdout, &resp.encode()).await?;
    stdout.flush().await?;

    if hello.flags.contains(HelloFlags::PULL) {
        run_server_pull(hello, root_path, stdin, stdout).await
    } else {
        run_server_push(hello, root_path, stdin, stdout).await
    }
}

/// Handle PULL mode: client pulls files from server (we are source)
async fn run_server_pull(
    hello: v2::Hello,
    root_path: PathBuf,
    mut stdin: impl io::AsyncRead + Unpin,
    mut stdout: impl io::AsyncWrite + Unpin,
) -> Result<()> {
    // 1. Receive DEST_FILE_ENTRY messages from client (Initial Exchange)
    let mut generator = Generator::new(GeneratorConfig {
        root: root_path.clone(),
        include_hidden: true,
        follow_symlinks: false,
        delete_enabled: hello.flags.contains(HelloFlags::DELETE),
    });

    loop {
        let (msg_type, payload) = v2::read_frame(&mut stdin).await?;
        match msg_type {
            MessageType::DestFileEntry => {
                let entry = v2::DestFileEntry::decode(payload)?;
                generator.add_dest_entry(entry);
            }
            MessageType::DestFileEnd => break,
            _ => anyhow::bail!("Unexpected message during Initial Exchange: {:?}", msg_type),
        }
    }

    // 2. Run Generator and Sender pipeline
    let (tx, rx) = file_job_channel();
    let gen_handle = tokio::spawn(async move { generator.run(tx).await });

    let sender = Sender::new(SenderConfig {
        root: root_path,
        compress: hello.flags.contains(HelloFlags::COMPRESSION),
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

    // Stream data to client (concurrent with sender)
    while let Some(bytes) = data_rx.recv().await {
        v2::write_frame(&mut stdout, &bytes).await?;
    }
    stdout.flush().await?;

    let (total_files, total_bytes) = gen_handle.await??;
    sender_handle.await??;

    // Send DONE
    let done = v2::Done {
        files_ok: total_files,
        files_err: 0,
        bytes: total_bytes,
        duration_ms: 0,
    };
    v2::write_frame(&mut stdout, &done.encode()).await?;
    stdout.flush().await?;

    Ok(())
}

/// Handle PUSH mode: client pushes files to server (we are destination)
async fn run_server_push(
    _hello: v2::Hello,
    root_path: PathBuf,
    mut stdin: impl io::AsyncRead + Unpin,
    mut stdout: impl io::AsyncWrite + Unpin,
) -> Result<()> {
    let mut receiver = Receiver::new(ReceiverConfig {
        root: root_path.clone(),
        block_size: 4096,
    });

    // 1. Send Initial Exchange (our files metadata)
    // Use unbounded channel to avoid blocking_send (panics in tokio context)
    let (data_tx, mut data_rx) = mpsc::unbounded_channel::<Bytes>();
    let receiver_root = root_path.clone();

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
        v2::write_frame(&mut stdout, &bytes).await?;
    }
    stdout.flush().await?;

    // Wait for scanner to complete
    scan_handle.await??;

    // 2. Receive streaming messages
    loop {
        let (msg_type, payload) = v2::read_frame(&mut stdin).await?;

        if msg_type == MessageType::Done {
            break;
        }

        receiver.handle_message(msg_type, payload).await?;
    }

    // 3. Send DONE
    let done = v2::Done {
        files_ok: receiver.stats().files_ok,
        files_err: receiver.stats().files_err,
        bytes: receiver.stats().bytes_transferred,
        duration_ms: 0,
    };
    v2::write_frame(&mut stdout, &done.encode()).await?;
    stdout.flush().await?;

    Ok(())
}
