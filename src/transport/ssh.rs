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
        // For now, return error - metadata is complex to bridge from remote to local
        Err(SyncError::Io(std::io::Error::other(
            "SSH transport metadata requires local Metadata struct which doesn't work for remote files"
        )))
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();
        let command = format!("mkdir -p '{}'", path_str);

        tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            let cmd = command.clone();
            move || Self::execute_command(session, &cmd)
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;

        Ok(())
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<()> {
        // Use SFTP for file transfer
        let source_path = source.to_path_buf();
        let dest_path = dest.to_path_buf();

        tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            move || {
                let session = session.lock().map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to lock session: {}", e)))
                })?;

                // Read local file
                let content = std::fs::read(&source_path).map_err(|e| {
                    SyncError::Io(std::io::Error::new(
                        e.kind(),
                        format!("Failed to read source file {}: {}", source_path.display(), e),
                    ))
                })?;

                // Get source metadata for mtime
                let metadata = std::fs::metadata(&source_path).map_err(|e| {
                    SyncError::Io(std::io::Error::new(
                        e.kind(),
                        format!("Failed to get metadata for {}: {}", source_path.display(), e),
                    ))
                })?;

                // Get SFTP session
                let sftp = session.sftp().map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to create SFTP session: {}", e)))
                })?;

                // Write to remote file
                let mut remote_file = sftp.create(&dest_path).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to create remote file {}: {}",
                        dest_path.display(),
                        e
                    )))
                })?;

                std::io::Write::write_all(&mut remote_file, &content).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to write to remote file {}: {}",
                        dest_path.display(),
                        e
                    )))
                })?;

                // Set modification time
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
                        let mtime = duration.as_secs();
                        let atime = mtime; // Use same time for access time
                        let _ = sftp.setstat(
                            &dest_path,
                            ssh2::FileStat {
                                size: Some(content.len() as u64),
                                uid: None,
                                gid: None,
                                perm: None,
                                atime: Some(atime),
                                mtime: Some(mtime),
                            },
                        );
                    }
                }

                Ok::<(), crate::error::SyncError>(())
            }
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;

        Ok(())
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        let path_str = path.to_string_lossy();
        let command = if is_dir {
            format!("rm -rf '{}'", path_str)
        } else {
            format!("rm -f '{}'", path_str)
        };

        tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            let cmd = command.clone();
            move || Self::execute_command(session, &cmd)
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;

        Ok(())
    }
}
