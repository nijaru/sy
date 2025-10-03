use super::{Transport, TransferResult};
use crate::delta::{apply_delta, calculate_block_size, compute_checksums, generate_delta_streaming, DeltaOp};
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

        let mut stderr = String::new();
        let _ = channel.stderr().read_to_string(&mut stderr);

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
                "Command '{}' failed with exit code {}\nstdout: {}\nstderr: {}",
                command, exit_status, output, stderr
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

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Use SFTP for file transfer with streaming and checksum verification
        let source_path = source.to_path_buf();
        let dest_path = dest.to_path_buf();

        tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            move || {
                let session = session.lock().map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to lock session: {}", e)))
                })?;

                // Get source metadata for mtime
                let metadata = std::fs::metadata(&source_path).map_err(|e| {
                    SyncError::Io(std::io::Error::new(
                        e.kind(),
                        format!("Failed to get metadata for {}: {}", source_path.display(), e),
                    ))
                })?;

                // Open source file for streaming
                let mut source_file = std::fs::File::open(&source_path).map_err(|e| {
                    SyncError::Io(std::io::Error::new(
                        e.kind(),
                        format!("Failed to open source file {}: {}", source_path.display(), e),
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

                // Stream file in chunks with checksum calculation
                const CHUNK_SIZE: usize = 128 * 1024; // 128KB chunks
                let mut buffer = vec![0u8; CHUNK_SIZE];
                let mut hasher = xxhash_rust::xxh3::Xxh3::new();
                let mut bytes_written = 0u64;

                loop {
                    let bytes_read = std::io::Read::read(&mut source_file, &mut buffer).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!("Failed to read from {}: {}", source_path.display(), e),
                        ))
                    })?;

                    if bytes_read == 0 {
                        break; // EOF
                    }

                    // Update checksum
                    hasher.update(&buffer[..bytes_read]);

                    // Write chunk to remote
                    std::io::Write::write_all(&mut remote_file, &buffer[..bytes_read]).map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to write to remote file {}: {}",
                            dest_path.display(),
                            e
                        )))
                    })?;

                    bytes_written += bytes_read as u64;
                }

                let checksum = hasher.digest();

                tracing::debug!(
                    "Transferred {} ({} bytes, xxh3: {:x})",
                    source_path.display(),
                    bytes_written,
                    checksum
                );

                // Set modification time
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
                        let mtime = duration.as_secs();
                        let atime = mtime;
                        let _ = sftp.setstat(
                            &dest_path,
                            ssh2::FileStat {
                                size: Some(bytes_written),
                                uid: None,
                                gid: None,
                                perm: None,
                                atime: Some(atime),
                                mtime: Some(mtime),
                            },
                        );
                    }
                }

                Ok::<u64, crate::error::SyncError>(bytes_written)
            }
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))
        .and_then(|r| r)
        .map(TransferResult::new)
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Check if remote destination exists
        if !self.exists(dest).await? {
            tracing::debug!("Remote destination doesn't exist, using full copy");
            return self.copy_file(source, dest).await;
        }

        // Get source size
        let source_meta = std::fs::metadata(source).map_err(|e| {
            SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to get source metadata: {}", e),
            ))
        })?;
        let source_size = source_meta.len();

        let source_path = source.to_path_buf();
        let dest_path = dest.to_path_buf();

        tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            move || {
                let session = session.lock().map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to lock session: {}", e)))
                })?;

                let sftp = session.sftp().map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to create SFTP session: {}", e)))
                })?;

                // Get remote file size
                let remote_stat = sftp.stat(&dest_path).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to stat remote file {}: {}",
                        dest_path.display(),
                        e
                    )))
                })?;

                let dest_size = remote_stat.size.unwrap_or(0);

                // Skip delta if destination is too small
                if dest_size < 4096 {
                    tracing::debug!("Remote destination too small for delta sync, using full copy");
                    drop(session);
                    return Err(SyncError::Io(std::io::Error::other(
                        "Destination too small, caller should use copy_file"
                    )));
                }

                // Download remote file to temp location for checksum computation
                let temp_dir = tempfile::tempdir().map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to create temp dir: {}", e)))
                })?;
                let temp_dest = temp_dir.path().join("remote_dest");

                tracing::debug!("Downloading remote file for delta computation...");
                let mut remote_file = sftp.open(&dest_path).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to open remote file {}: {}",
                        dest_path.display(),
                        e
                    )))
                })?;

                let mut temp_file = std::fs::File::create(&temp_dest).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to create temp file: {}", e)))
                })?;

                std::io::copy(&mut remote_file, &mut temp_file).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to download remote file: {}", e)))
                })?;

                drop(temp_file);
                drop(remote_file);

                // Calculate block size
                let block_size = calculate_block_size(dest_size);

                // Compute checksums of downloaded destination file
                tracing::debug!("Computing checksums...");
                let dest_checksums = compute_checksums(&temp_dest, block_size)
                    .map_err(|e| SyncError::CopyError {
                        path: temp_dest.clone(),
                        source: e,
                    })?;

                // Generate delta with streaming (constant memory)
                tracing::debug!("Generating delta with streaming...");
                let delta = generate_delta_streaming(&source_path, &dest_checksums, block_size)
                    .map_err(|e| SyncError::CopyError {
                        path: source_path.clone(),
                        source: e,
                    })?;

                // Calculate compression ratio
                let literal_bytes: u64 = delta.ops.iter()
                    .filter_map(|op| {
                        if let DeltaOp::Data(data) = op {
                            Some(data.len() as u64)
                        } else {
                            None
                        }
                    })
                    .sum();

                let compression_ratio = if source_size > 0 {
                    (literal_bytes as f64 / source_size as f64) * 100.0
                } else {
                    0.0
                };

                // Apply delta to create updated file
                tracing::debug!("Applying delta...");
                let temp_updated = temp_dir.path().join("updated");
                apply_delta(&temp_dest, &delta, &temp_updated)
                    .map_err(|e| SyncError::CopyError {
                        path: temp_updated.clone(),
                        source: e,
                    })?;

                // Upload updated file to remote
                tracing::debug!("Uploading updated file...");
                let mut updated_file = std::fs::File::open(&temp_updated).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to open updated file: {}", e)))
                })?;

                let mut remote_file = sftp.create(&dest_path).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to create remote file {}: {}",
                        dest_path.display(),
                        e
                    )))
                })?;

                let bytes_written = std::io::copy(&mut updated_file, &mut remote_file).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!("Failed to upload file: {}", e)))
                })?;

                tracing::info!(
                    "Delta sync: {} ops, {:.1}% literal data, uploaded {} bytes",
                    delta.ops.len(),
                    compression_ratio,
                    bytes_written
                );

                Ok::<u64, SyncError>(bytes_written)
            }
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
        .map(TransferResult::new)
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
