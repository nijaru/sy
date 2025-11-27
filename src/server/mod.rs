// Server mode code - used when running `sy --server` as a subprocess
// The code appears "dead" to the compiler since it's only used at runtime
#![allow(dead_code)]

pub mod handler;
pub mod protocol;

use anyhow::Result;
use handler::ServerHandler;
use protocol::{
    Action, ChecksumReq, DeltaData, ErrorMessage, FileData, FileList, FileListEntry, Hello,
    MessageType, MkdirBatch, MkdirBatchAck, SymlinkBatch, SymlinkBatchAck, SymlinkEntry,
    HELLO_FLAG_PULL, PROTOCOL_VERSION,
};
use std::path::{Path, PathBuf};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

use crate::sync::scanner::{self, ScanOptions};

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

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut handler = ServerHandler::new(root_path);

    // Handshake
    let _len = stdin.read_u32().await?;
    let type_byte = stdin.read_u8().await?;

    if type_byte != MessageType::Hello as u8 {
        let err = ErrorMessage {
            code: 1,
            message: format!("Expected HELLO (0x01), got 0x{:02X}", type_byte),
        };
        err.write(&mut stdout).await?;
        return Ok(());
    }

    let hello = Hello::read(&mut stdin).await?;

    if hello.version != PROTOCOL_VERSION {
        let err = ErrorMessage {
            code: 1,
            message: format!(
                "Version mismatch: client {}, server {}",
                hello.version, PROTOCOL_VERSION
            ),
        };
        err.write(&mut stdout).await?;
        return Ok(());
    }

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

    // Main message loop (PUSH mode - client sends files to server)
    while let Ok(_len) = stdin.read_u32().await {
        let type_byte = stdin.read_u8().await?;

        match MessageType::from_u8(type_byte) {
            Some(MessageType::FileList) => {
                let list = protocol::FileList::read(&mut stdin).await?;
                handler.handle_file_list(list, &mut stdout).await?;
            }

            Some(MessageType::MkdirBatch) => {
                let batch = MkdirBatch::read(&mut stdin).await?;
                handler.handle_mkdir_batch(batch, &mut stdout).await?;
            }

            Some(MessageType::SymlinkBatch) => {
                let batch = SymlinkBatch::read(&mut stdin).await?;
                handler.handle_symlink_batch(batch, &mut stdout).await?;
            }

            Some(MessageType::FileData) => {
                let data = protocol::FileData::read(&mut stdin).await?;
                handler.handle_file_data(data, &mut stdout).await?;
            }

            Some(MessageType::ChecksumReq) => {
                let req = ChecksumReq::read(&mut stdin).await?;
                handler.handle_checksum_req(req, &mut stdout).await?;
            }

            Some(MessageType::DeltaData) => {
                let delta = DeltaData::read(&mut stdin).await?;
                handler.handle_delta_data(delta, &mut stdout).await?;
            }

            Some(MessageType::Error) => {
                let err = protocol::ErrorMessage::read(&mut stdin).await?;
                tracing::error!("Received error: {}", err.message);
                return Err(anyhow::anyhow!("Remote error: {}", err.message));
            }

            Some(msg_type) => {
                tracing::warn!("Unhandled message type: {:?}", msg_type);
                let err = ErrorMessage {
                    code: 1,
                    message: format!("Unhandled message type: 0x{:02X}", type_byte),
                };
                err.write(&mut stdout).await?;
                stdout.flush().await?;
            }

            None => {
                tracing::error!("Unknown message type: 0x{:02X}", type_byte);
                let err = ErrorMessage {
                    code: 1,
                    message: format!("Unknown message type: 0x{:02X}", type_byte),
                };
                err.write(&mut stdout).await?;
                stdout.flush().await?;
                break;
            }
        }
    }

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

    // Step 1: Send directories (MKDIR_BATCH)
    if !directories.is_empty() {
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
    }

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
