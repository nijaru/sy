use super::Transport;
use crate::error::{Result, SyncError};
use crate::ssh::config::SshConfig;
use crate::ssh::connect;
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ssh2::Session;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
struct ScanOutput {
    entries: Vec<FileEntryJson>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileEntryJson {
    path: String,
    size: u64,
    mtime: i64,
    is_dir: bool,
}

pub struct SshTransport {
    session: Arc<Mutex<Session>>,
    remote_binary_path: String,
}

impl SshTransport {
    pub async fn new(config: &SshConfig) -> Result<Self> {
        let session = connect::connect(config).await?;
        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            remote_binary_path: "sy-remote".to_string(), // Assume in PATH for now
        })
    }

    fn execute_command(session: Arc<Mutex<Session>>, command: &str) -> Result<String> {
        let session = session.lock().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!("Failed to lock session: {}", e)))
        })?;

        let mut channel = session.channel_session().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!("Failed to create channel: {}", e)))
        })?;

        channel.exec(command).map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to execute command: {}",
                e
            )))
        })?;

        let mut output = String::new();
        channel.read_to_string(&mut output).map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to read command output: {}",
                e
            )))
        })?;

        channel.wait_close().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!("Failed to close channel: {}", e)))
        })?;

        let exit_status = channel.exit_status().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to get exit status: {}",
                e
            )))
        })?;

        if exit_status != 0 {
            return Err(SyncError::Io(std::io::Error::other(format!(
                "Command failed with exit code {}: {}",
                exit_status, output
            ))));
        }

        Ok(output)
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>> {
        let path_str = path.to_string_lossy();
        let command = format!("{} scan {}", self.remote_binary_path, path_str);

        let output = tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            let cmd = command.clone();
            move || Self::execute_command(session, &cmd)
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;

        let scan_output: ScanOutput = serde_json::from_str(&output).map_err(|e| {
            SyncError::Io(std::io::Error::other(format!("Failed to parse JSON: {}", e)))
        })?;

        let entries: Result<Vec<FileEntry>> = scan_output
            .entries
            .into_iter()
            .map(|e| {
                let modified =
                    UNIX_EPOCH + Duration::from_secs(e.mtime.max(0) as u64);
                Ok(FileEntry {
                    path: PathBuf::from(&e.path),
                    relative_path: PathBuf::from(&e.path)
                        .strip_prefix(path)
                        .unwrap_or(Path::new(&e.path))
                        .to_path_buf(),
                    size: e.size,
                    modified,
                    is_dir: e.is_dir,
                })
            })
            .collect();

        entries
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy();
        let command = format!("test -e {} && echo 'exists' || echo 'not found'", path_str);

        let output = tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            let cmd = command.clone();
            move || Self::execute_command(session, &cmd)
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;

        Ok(output.trim() == "exists")
    }

    async fn metadata(&self, _path: &Path) -> Result<std::fs::Metadata> {
        unimplemented!("SSH transport metadata not yet implemented")
    }

    async fn create_dir_all(&self, _path: &Path) -> Result<()> {
        unimplemented!("SSH transport create_dir_all not yet implemented")
    }

    async fn copy_file(&self, _source: &Path, _dest: &Path) -> Result<()> {
        unimplemented!("SSH transport copy_file not yet implemented")
    }

    async fn remove(&self, _path: &Path, _is_dir: bool) -> Result<()> {
        unimplemented!("SSH transport remove not yet implemented")
    }
}
