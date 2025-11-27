pub mod handler;
pub mod protocol;

use anyhow::Result;
use handler::ServerHandler;
use protocol::{
    ChecksumReq, DeltaData, ErrorMessage, Hello, MessageType, MkdirBatch, SymlinkBatch,
    PROTOCOL_VERSION,
};
use std::path::{Path, PathBuf};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

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

    // Main message loop
    loop {
        let _len = match stdin.read_u32().await {
            Ok(n) => n,
            Err(_) => break, // EOF
        };
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
