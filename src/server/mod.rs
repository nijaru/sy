// Server mode code - used when running `sy --server` as a subprocess
// The code appears "dead" to the compiler since it's only used at runtime
#![allow(dead_code)]

pub mod handler;
pub mod protocol;

use anyhow::Result;
use handler::{compute_checksum_response, ServerHandler};
use protocol::{
    Action, ChecksumReq, ChecksumResp, DeltaData, ErrorMessage, FileData, FileList, FileListEntry,
    Hello, MessageType, MkdirBatch, MkdirBatchAck, SymlinkBatch, SymlinkBatchAck, SymlinkEntry,
    HELLO_FLAG_PULL, PROTOCOL_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::sync::scanner::{self, ScanOptions};
use bytes::Bytes;
use tokio::fs;

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

pub async fn run_server() -> Result<()> {
    // Parse args: sy --server <path>
    let args: Vec<String> = std::env::args().collect();
    let raw_path = args
        .last()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let root_path = expand_tilde(&raw_path);

    // Ensure root directory exists
    if !root_path.exists() {
        std::fs::create_dir_all(&root_path)?;
    }

    let stdin = io::stdin();
    let stdout = io::stdout();

    let handler = ServerHandler::new(root_path);

    // Handshake
    let mut stdin_pin = stdin;
    let mut stdout_pin = stdout;

    let len = stdin_pin.read_u32().await?;
    let type_byte = stdin_pin.read_u8().await?;

    if type_byte != MessageType::Hello as u8 {
        let err = ErrorMessage {
            code: 1,
            message: format!("Expected HELLO (0x01), got 0x{:02X}", type_byte),
        };
        err.write(&mut stdout_pin).await?;
        return Ok(());
    }

    // Read payload to detect version
    let mut payload = vec![0u8; len as usize];
    stdin_pin.read_exact(&mut payload).await?;
    let payload = Bytes::from(payload);

    if payload.len() < 2 {
        anyhow::bail!("HELLO payload too short");
    }
    let version = u16::from_be_bytes([payload[0], payload[1]]);

    if version == 1 {
        let hello = Hello::decode_payload(payload)?;
        return run_server_v1(hello, stdin_pin, stdout_pin, handler).await;
    } else if version >= 2 {
        return run_server_v2(payload, stdin_pin, stdout_pin).await;
    } else {
        let err = ErrorMessage {
            code: 1,
            message: format!("Unsupported protocol version: {}", version),
        };
        err.write(&mut stdout_pin).await?;
        Ok(())
    }
}

pub async fn run_server_v1(
    hello: Hello,
    mut stdin: impl io::AsyncRead + Unpin,
    mut stdout: impl io::AsyncWrite + Unpin,
    mut handler: ServerHandler,
) -> Result<()> {
    // Send HELLO response
    let resp = Hello {
        version: PROTOCOL_VERSION,
        flags: 0,
        capabilities: vec![],
    };
    resp.write(&mut stdout).await?;
    stdout.flush().await?;

    // Check if client requested PULL mode (server sends files to client)
    if hello.flags & HELLO_FLAG_PULL != 0 {
        return run_server_pull_mode(&handler.root_path, &mut stdin, &mut stdout).await;
    }

    // Shared state for concurrent CHECKSUM_REQ handling
    let mut file_list: Option<Arc<Vec<FileListEntry>>> = None;
    let root_path_ref = Arc::new(handler.root_path.clone());

    // Channel for checksum results (spawned tasks send completed responses here)
    let (checksum_tx, mut checksum_rx) = mpsc::channel::<ChecksumResp>(32);
    let mut pending_checksum_count = 0usize;

    // Main message loop (PUSH mode - client sends files to server)
    loop {
        // Use select! to handle both incoming messages and outgoing checksum responses
        tokio::select! {
            biased;

            // Prioritize writing completed checksum responses
            Some(resp) = checksum_rx.recv(), if pending_checksum_count > 0 => {
                resp.write(&mut stdout).await?;
                pending_checksum_count -= 1;

                // Batch writes: only flush when channel is empty or we've written all pending
                if pending_checksum_count == 0 || checksum_rx.is_empty() {
                    stdout.flush().await?;
                }
            }

            // Read and handle incoming messages
            len_result = stdin.read_u32() => {
                let len = match len_result {
                    Ok(len) => len,
                    Err(_) => break, // EOF or error, exit loop
                };
                let type_byte = stdin.read_u8().await?;

                match MessageType::from_u8(type_byte) {
                    Some(MessageType::FileList) => {
                        // Wait for all pending checksums before processing file list
                        drain_pending_checksums(&mut checksum_rx, &mut pending_checksum_count, &mut stdout).await?;

                        let list = protocol::FileList::read(&mut stdin).await?;
                        // Store file list for concurrent checksum handling
                        file_list = Some(Arc::new(list.entries.clone()));
                        handler.handle_file_list(list, &mut stdout).await?;
                    }

                    Some(MessageType::MkdirBatch) => {
                        drain_pending_checksums(&mut checksum_rx, &mut pending_checksum_count, &mut stdout).await?;
                        let batch = MkdirBatch::read(&mut stdin).await?;
                        handler.handle_mkdir_batch(batch, &mut stdout).await?;
                    }

                    Some(MessageType::SymlinkBatch) => {
                        drain_pending_checksums(&mut checksum_rx, &mut pending_checksum_count, &mut stdout).await?;
                        let batch = SymlinkBatch::read(&mut stdin).await?;
                        handler.handle_symlink_batch(batch, &mut stdout).await?;
                    }

                    Some(MessageType::ChecksumReq) => {
                        let req = ChecksumReq::read(&mut stdin).await?;

                        // Capture needed state for the task
                        let tx = checksum_tx.clone();
                        let root = root_path_ref.clone();
                        let files = file_list.clone();

                        if let Some(entries) = files {
                            // Spawn task for async checksum computation
                            tokio::spawn(async move {
                                let resp = compute_checksum_response(req.index, req.block_size as usize, &entries, &root).await;
                                match resp {
                                    Ok(r) => { let _ = tx.send(r).await; }
                                    Err(e) => { tracing::error!("Checksum error: {}", e); }
                                }
                            });
                            pending_checksum_count += 1;
                        }
                    }

                    Some(MessageType::DeltaData) => {
                        drain_pending_checksums(&mut checksum_rx, &mut pending_checksum_count, &mut stdout).await?;
                        let delta = DeltaData::read(&mut stdin).await?;
                        handler.handle_delta_data(delta, &mut stdout).await?;
                    }

                    Some(MessageType::FileData) => {
                        drain_pending_checksums(&mut checksum_rx, &mut pending_checksum_count, &mut stdout).await?;
                        let data = FileData::read(&mut stdin).await?;
                        handler.handle_file_data(data, &mut stdout).await?;
                    }

                    Some(MessageType::FileDone) => {
                        drain_pending_checksums(&mut checksum_rx, &mut pending_checksum_count, &mut stdout).await?;
                        break;
                    }

                    _ => {
                        tracing::warn!("Unknown message type: 0x{:02X}", type_byte);
                        // Skip unknown message payload
                        let mut buf = vec![0u8; len as usize];
                        stdin.read_exact(&mut buf).await?;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_server_v2(
    payload: Bytes,
    mut stdin: impl io::AsyncRead + Unpin,
    mut stdout: impl io::AsyncWrite + Unpin,
) -> Result<()> {
    use crate::streaming::{
        channel::file_job_channel, protocol as v2, Generator, GeneratorConfig, Receiver,
        ReceiverConfig, Sender, SenderConfig,
    };

    let hello = v2::Hello::decode(payload)?;
    let root_path = expand_tilde(Path::new(&hello.root_path));

    // Ensure root exists
    if !root_path.exists() {
        fs::create_dir_all(&root_path).await?;
    }

    // Send HELLO response
    let resp = v2::Hello::new(v2::HelloFlags::empty(), "");
    v2::write_frame(&mut stdout, &resp.encode()).await?;
    stdout.flush().await?;

    if hello.flags.contains(v2::HelloFlags::PULL) {
        // Client wants to pull - we are the source
        // 1. Receive DEST_FILE_ENTRY messages from client (Initial Exchange)
        let mut generator = Generator::new(GeneratorConfig {
            root: root_path.clone(),
            include_hidden: true,
            follow_symlinks: false,
            delete_enabled: hello.flags.contains(v2::HelloFlags::DELETE),
        });

        loop {
            let (msg_type, payload) = v2::read_frame(&mut stdin).await?;
            match msg_type {
                v2::MessageType::DestFileEntry => {
                    let entry = v2::DestFileEntry::decode(payload)?;
                    generator.add_dest_entry(entry);
                }
                v2::MessageType::DestFileEnd => {
                    break;
                }
                _ => anyhow::bail!("Unexpected message during Initial Exchange: {:?}", msg_type),
            }
        }

        // 2. Run Generator and Sender
        let (tx, rx) = file_job_channel();
        let gen_handle = tokio::spawn(async move { generator.run(tx).await });

        let sender = Sender::new(SenderConfig {
            root: root_path,
            compress: hello.flags.contains(v2::HelloFlags::COMPRESSION),
        });

        let (data_tx, mut data_rx) = mpsc::channel::<Bytes>(100);
        let sender_handle = tokio::spawn(async move {
            sender
                .run(rx, |bytes| {
                    if data_tx.blocking_send(bytes).is_err() {
                        return Err(anyhow::anyhow!("Data channel closed"));
                    }
                    Ok(())
                })
                .await
        });

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
    } else {
        // Client wants to push - we are the destination
        let mut receiver = Receiver::new(ReceiverConfig {
            root: root_path.clone(),
            block_size: 4096,
        });

        // 1. Send Initial Exchange (our files metadata)
        let (data_tx, mut data_rx) = mpsc::channel::<Bytes>(100);
        let receiver_root = root_path.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let receiver = Receiver::new(ReceiverConfig {
                root: receiver_root,
                block_size: 4096,
            });
            rt.block_on(receiver.scan_dest(|bytes| {
                data_tx
                    .blocking_send(bytes)
                    .map_err(|_| anyhow::anyhow!("Data channel closed"))
            }))
        })
        .await??;

        while let Some(bytes) = data_rx.recv().await {
            v2::write_frame(&mut stdout, &bytes).await?;
        }
        stdout.flush().await?;

        // 2. Receive streaming messages
        loop {
            let (msg_type, payload) = v2::read_frame(&mut stdin).await?;

            if msg_type == v2::MessageType::Done {
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
    }

    Ok(())
}

/// Drain all pending checksum responses from the channel
async fn drain_pending_checksums<W: AsyncWriteExt + Unpin>(
    rx: &mut mpsc::Receiver<ChecksumResp>,
    pending_count: &mut usize,
    writer: &mut W,
) -> Result<()> {
    if *pending_count == 0 {
        return Ok(());
    }

    // Write all pending responses
    while *pending_count > 0 {
        if let Some(resp) = rx.recv().await {
            resp.write(writer).await?;
            *pending_count -= 1;
        } else {
            break;
        }
    }

    // Single flush for the entire batch
    writer.flush().await?;
    Ok(())
}

/// PULL mode: Server scans source and sends files to client
async fn run_server_pull_mode<R, W>(root_path: &Path, stdin: &mut R, stdout: &mut W) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // Scan source directory
    let scan_opts = ScanOptions::default();
    let root = root_path.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || {
        scanner::Scanner::new(&root).with_options(scan_opts).scan()
    })
    .await??;

    // Separate entries by type
    let mut directories: Vec<String> = Vec::new();
    let mut files: Vec<(String, PathBuf, u64, i64, u32)> = Vec::new(); // (rel_path, abs_path, size, mtime, mode)
    let mut symlinks: Vec<SymlinkEntry> = Vec::new();

    for entry in entries {
        if let Ok(rel_path) = entry.path.strip_prefix(root_path) {
            if rel_path.as_os_str().is_empty() {
                continue; // Skip root
            }
            if let Some(path_str) = rel_path.to_str() {
                let mtime = entry
                    .modified
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;

                if entry.is_dir {
                    directories.push(path_str.to_string());
                } else if entry.is_symlink {
                    if let Some(target) = entry.symlink_target {
                        if let Some(target_str) = target.to_str() {
                            symlinks.push(SymlinkEntry {
                                path: path_str.to_string(),
                                target: target_str.to_string(),
                            });
                        }
                    }
                } else {
                    files.push((
                        path_str.to_string(),
                        entry.path.to_path_buf(),
                        entry.size,
                        mtime,
                        0o644,
                    ));
                }
            }
        }
    }

    // Step 1: Send directories (MKDIR_BATCH) - always send, even if empty
    let batch = MkdirBatch {
        paths: directories.clone(),
    };
    batch.write(stdout).await?;
    stdout.flush().await?;

    // Wait for MKDIR_BATCH_ACK
    let _len = stdin.read_u32().await?;
    let type_byte = stdin.read_u8().await?;
    if type_byte != MessageType::MkdirBatchAck as u8 {
        return Err(anyhow::anyhow!(
            "Expected MKDIR_BATCH_ACK, got 0x{:02X}",
            type_byte
        ));
    }
    let _ack = MkdirBatchAck::read(stdin).await?;

    // Step 2: Send file list (FILE_LIST)
    let file_entries: Vec<FileListEntry> = files
        .iter()
        .map(|(rel_path, _, size, mtime, mode)| FileListEntry {
            path: rel_path.clone(),
            size: *size,
            mtime: *mtime,
            mode: *mode,
            flags: 0,
            symlink_target: None,
        })
        .collect();

    let file_list = FileList {
        entries: file_entries,
    };
    file_list.write(stdout).await?;
    stdout.flush().await?;

    // Wait for FILE_LIST_ACK with decisions
    let _len = stdin.read_u32().await?;
    let type_byte = stdin.read_u8().await?;
    if type_byte != MessageType::FileListAck as u8 {
        return Err(anyhow::anyhow!(
            "Expected FILE_LIST_ACK, got 0x{:02X}",
            type_byte
        ));
    }
    let ack = protocol::FileListAck::read(stdin).await?;

    // Step 3: Send files that client requested
    for decision in &ack.decisions {
        if decision.action == Action::Skip {
            continue;
        }

        let idx = decision.index as usize;
        if idx >= files.len() {
            continue;
        }

        let (_, abs_path, _, _, _) = &files[idx];

        // Read file data
        let data = match std::fs::read(abs_path) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to read {}: {}", abs_path.display(), e);
                continue;
            }
        };

        // Send FILE_DATA
        let file_data = FileData {
            index: decision.index,
            offset: 0,
            flags: 0,
            data,
        };
        file_data.write(stdout).await?;
        stdout.flush().await?;

        // Wait for FILE_DONE
        let _len = stdin.read_u32().await?;
        let type_byte = stdin.read_u8().await?;
        if type_byte != MessageType::FileDone as u8 {
            return Err(anyhow::anyhow!(
                "Expected FILE_DONE, got 0x{:02X}",
                type_byte
            ));
        }
        let _done = protocol::FileDone::read(stdin).await?;
    }

    // Step 4: Send symlinks (SYMLINK_BATCH)
    if !symlinks.is_empty() {
        let batch = SymlinkBatch {
            entries: symlinks.clone(),
        };
        batch.write(stdout).await?;
        stdout.flush().await?;

        // Wait for SYMLINK_BATCH_ACK
        let _len = stdin.read_u32().await?;
        let type_byte = stdin.read_u8().await?;
        if type_byte != MessageType::SymlinkBatchAck as u8 {
            return Err(anyhow::anyhow!(
                "Expected SYMLINK_BATCH_ACK, got 0x{:02X}",
                type_byte
            ));
        }
        let _ack = SymlinkBatchAck::read(stdin).await?;
    }

    Ok(())
}
