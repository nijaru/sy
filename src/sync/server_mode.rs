use anyhow::Result;
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::path::SyncPath;
use crate::server::protocol::{Action, FileListEntry};
use crate::ssh::config::SshConfig;
use crate::sync::scanner::{self, ScanOptions};
use crate::transport::server::ServerSession;

pub async fn sync_server_mode(source: &Path, dest: &SyncPath) -> Result<()> {
    // 1. Connect
    let mut session = match dest {
        SyncPath::Local { path, .. } => {
            // Local -> Local via server mode (for testing mostly)
            ServerSession::connect_local(path).await?
        }
        SyncPath::Remote {
            host, user, path, ..
        } => {
            // Local -> Remote SSH
            let mut config = SshConfig::default();
            config.hostname = host.clone();
            if let Some(u) = user {
                config.user = u.clone();
            }
            // TODO: Load real config

            ServerSession::connect_ssh(&config, path).await?
        }
        _ => anyhow::bail!("Unsupported destination for server mode"),
    };

    tracing::info!("Connected to server mode");

    // 2. Scan Source
    tracing::info!("Scanning source...");
    let scan_opts = ScanOptions::default();
    let entries = tokio::task::spawn_blocking({
        let src = source.to_path_buf();
        move || scanner::Scanner::new(&src).with_options(scan_opts).scan()
    })
    .await??;

    // Convert to protocol entries
    let mut proto_entries = Vec::with_capacity(entries.len());
    let mut file_map = Vec::with_capacity(entries.len()); // Index -> Path map

    for entry in &entries {
        if let Ok(rel_path) = entry.path.strip_prefix(source) {
            if let Some(path_str) = rel_path.to_str() {
                proto_entries.push(FileListEntry {
                    path: path_str.to_string(),
                    size: entry.size,
                    mtime: entry
                        .modified
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                    mode: 0o644, // TODO: Real mode
                    flags: 0,    // TODO: Dir/Symlink
                });
                file_map.push(entry.path.clone());
            }
        }
    }

    // 3. Send File List
    tracing::info!("Sending file list ({} files)...", proto_entries.len());
    session.send_file_list(proto_entries).await?;

    // 4. Receive ACK (Decisions)
    tracing::info!("Waiting for server decisions...");
    let ack = session.read_ack().await?;

    // 5. Send Data
    for decision in ack.decisions {
        match decision.action {
            Action::Create | Action::Update => {
                let path = &file_map[decision.index as usize];
                tracing::info!("Sending {}", path.display());

                let mut file = fs::File::open(&**path).await?;
                let mut buffer = Vec::new();
                file.read_to_end(&mut buffer).await?;

                session.send_file_data(decision.index, 0, buffer).await?;

                // Wait for Done
                let done = session.read_file_done().await?;
                if done.status != 0 {
                    tracing::error!(
                        "Failed to send file index {}: status {}",
                        decision.index,
                        done.status
                    );
                }
            }
            Action::Skip => {
                tracing::debug!("Skipping index {}", decision.index);
            }
            Action::Delete => {
                // Not handled in Push mode by default yet
            }
        }
    }

    tracing::info!("Server sync complete");
    Ok(())
}
