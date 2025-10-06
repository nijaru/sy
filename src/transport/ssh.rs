use super::{Transport, TransferResult};
use crate::delta::{calculate_block_size, generate_delta_streaming, BlockChecksum, DeltaOp};
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
        let remote_binary = self.remote_binary_path.clone();
        let session_clone = Arc::clone(&self.session);

        tokio::task::spawn_blocking({
            let session_arc = session_clone;
            move || {
                let session = session_arc.lock().map_err(|e| {
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

                // Calculate block size
                let block_size = calculate_block_size(dest_size);

                // Compute checksums on remote side (avoid downloading entire file!)
                tracing::debug!("Computing remote checksums via sy-remote...");
                drop(session); // Unlock session before remote command

                let dest_path_str = dest_path.to_string_lossy();
                let command = format!(
                    "{} checksums {} --block-size {}",
                    remote_binary, dest_path_str, block_size
                );

                let output = tokio::task::block_in_place(|| {
                    Self::execute_command(Arc::clone(&session_arc), &command)
                })?;

                let dest_checksums: Vec<BlockChecksum> = serde_json::from_str(&output)
                    .map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to parse remote checksums: {}",
                            e
                        )))
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

                // Serialize delta to JSON
                let delta_json = serde_json::to_string(&delta).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to serialize delta: {}",
                        e
                    )))
                })?;

                // Apply delta on remote side (avoids uploading full file!)
                tracing::debug!("Sending delta to remote for application...");
                let temp_remote_path = format!("{}.sy-tmp", dest_path.display());
                let command = format!(
                    "{} apply-delta {} {} --delta-json '{}'",
                    remote_binary,
                    dest_path_str,
                    temp_remote_path,
                    delta_json.replace('\'', "'\\''")  // Escape single quotes
                );

                let output = tokio::task::block_in_place(|| {
                    Self::execute_command(Arc::clone(&session_arc), &command)
                })?;

                #[derive(Deserialize)]
                struct ApplyStats {
                    operations_count: usize,
                    literal_bytes: u64,
                }

                let stats: ApplyStats = serde_json::from_str(&output).map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to parse apply-delta output: {}",
                        e
                    )))
                })?;

                // Rename temp file to final destination (atomic)
                let rename_command = format!("mv '{}' '{}'", temp_remote_path, dest_path_str);
                tokio::task::block_in_place(|| {
                    Self::execute_command(Arc::clone(&session_arc), &rename_command)
                })?;

                tracing::info!(
                    "Delta sync: {} ops, {:.1}% literal data, transferred ~{} bytes (delta only)",
                    stats.operations_count,
                    compression_ratio,
                    literal_bytes
                );

                Ok::<TransferResult, SyncError>(TransferResult::with_delta(
                    source_size, // Full file size
                    stats.operations_count,
                    stats.literal_bytes,
                ))
            }
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
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
