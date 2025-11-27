pub mod handler;
pub mod protocol;

use anyhow::Result;
use handler::ServerHandler;
use protocol::{ErrorMessage, FileListEntry, Hello, MessageType, PROTOCOL_VERSION};
use std::path::PathBuf;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

pub async fn run_server() -> Result<()> {
    // Parse args to get root path
    // Args: sy --server <path>
    let args: Vec<String> = std::env::args().collect();
    // Find --server and take next arg, or just take the last arg?
    // Clap parser in main.rs already consumed flags.
    // But main passes control here.
    // We need to parse the path manually or pass it from main.
    // Let's assume it's the last argument for now, or check main.rs integration.
    // Main uses `cli.parse()`. `cli.destination` should contain it if parsed correctly.
    // But `run_server` takes no args.
    // Let's modify `run_server` to take the root path.

    // For now, parse simple: last arg
    let root_path = args
        .last()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut handler = ServerHandler::new(root_path);

    // Keep track of file list for context
    let mut current_file_list: Vec<FileListEntry> = Vec::new();

    // 1. Handshake
    // Read HELLO
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

    // Read HELLO payload
    let hello = Hello::read(&mut stdin).await?;

    // Check version
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

    // Main Loop
    loop {
        // Read header
        let _len = match stdin.read_u32().await {
            Ok(n) => n,
            Err(_) => break, // EOF
        };
        let type_byte = stdin.read_u8().await?;

        match MessageType::from_u8(type_byte) {
            Some(MessageType::FileList) => {
                let list = protocol::FileList::read(&mut stdin).await?;
                current_file_list = list
                    .entries
                    .iter()
                    .map(|e| FileListEntry {
                        path: e.path.clone(),
                        size: e.size,
                        mtime: e.mtime,
                        mode: e.mode,
                        flags: e.flags,
                    })
                    .collect(); // Store copy for context

                // Restore original list for handler
                let list_for_handler = protocol::FileList {
                    entries: list.entries,
                };
                handler
                    .handle_file_list(list_for_handler, &mut stdout)
                    .await?;
            }
            Some(MessageType::FileData) => {
                let data = protocol::FileData::read(&mut stdin).await?;
                handler
                    .handle_file_data(data, &mut stdout, &current_file_list)
                    .await?;
            }
            Some(MessageType::Error) => {
                let err = protocol::ErrorMessage::read(&mut stdin).await?;
                tracing::error!("Received error: {}", err.message);
                return Err(anyhow::anyhow!("Remote error: {}", err.message));
            }
            _ => {
                tracing::error!("Unknown message type: 0x{:02X}", type_byte);
                break;
            }
        }
    }

    Ok(())
}
