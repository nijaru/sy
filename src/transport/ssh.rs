use super::{Transport, TransferResult};
use crate::compress::{compress, should_compress, Compression};
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
    // Extended metadata for full preservation
    is_symlink: bool,
    symlink_target: Option<String>,
    is_sparse: bool,
    allocated_size: u64,
    #[serde(default)]
    xattrs: Option<Vec<(String, String)>>, // (key, base64-encoded value)
    inode: Option<u64>,
    nlink: u64,
    #[serde(default)]
    acls: Option<String>, // ACL text format (one per line)
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

    /// Execute a command with stdin data (binary-safe)
    fn execute_command_with_stdin(
        session: Arc<Mutex<Session>>,
        command: &str,
        stdin_data: &[u8],
    ) -> Result<String> {
        use std::io::Write;

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

        // Write binary data to stdin
        channel.write_all(stdin_data).map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to write to stdin: {}",
                e
            )))
        })?;

        // Send EOF to stdin
        channel.send_eof().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!("Failed to send EOF: {}", e)))
        })?;

        // Read output
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

                // Decode xattrs from base64 if present
                let xattrs = e.xattrs.map(|xattr_vec| {
                    xattr_vec.into_iter()
                        .filter_map(|(key, base64_val)| {
                            use base64::{Engine as _, engine::general_purpose};
                            match general_purpose::STANDARD.decode(base64_val) {
                                Ok(decoded) => Some((key, decoded)),
                                Err(e) => {
                                    tracing::warn!("Failed to decode xattr {}: {}", key, e);
                                    None
                                }
                            }
                        })
                        .collect()
                });

                // Decode ACLs from text format
                let acls = e.acls.map(|acl_text| acl_text.into_bytes());

                Ok(FileEntry {
                    path: PathBuf::from(&e.path),
                    relative_path: PathBuf::from(&e.path)
                        .strip_prefix(path)
                        .unwrap_or(Path::new(&e.path))
                        .to_path_buf(),
                    size: e.size,
                    modified,
                    is_dir: e.is_dir,
                    is_symlink: e.is_symlink,
                    symlink_target: e.symlink_target.map(PathBuf::from),
                    is_sparse: e.is_sparse,
                    allocated_size: e.allocated_size,
                    xattrs,
                    inode: e.inode,
                    nlink: e.nlink,
                    acls,
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
        let source_path = source.to_path_buf();
        let dest_path = dest.to_path_buf();
        let session_arc = Arc::clone(&self.session);
        let remote_binary = self.remote_binary_path.clone();

        tokio::task::spawn_blocking(move || {
            // Get source metadata for mtime and size
            let metadata = std::fs::metadata(&source_path).map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Failed to get metadata for {}: {}", source_path.display(), e),
                ))
            })?;

            let file_size = metadata.len();
            let filename = source_path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            // Determine if compression would be beneficial
            let compression_mode = should_compress(filename, file_size);

            // Use compressed transfer for compressible files, SFTP for others
            match compression_mode {
                Compression::Zstd => {
                    tracing::debug!(
                        "File {}: {} bytes, using compressed transfer",
                        filename,
                        file_size
                    );

                    // Read entire file (compression only used for smaller files)
                    let file_data = std::fs::read(&source_path).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!("Failed to read {}: {}", source_path.display(), e),
                        ))
                    })?;

                    let uncompressed_size = file_data.len();

                    // Compress the data
                    let compressed_data = compress(&file_data, Compression::Zstd)
                        .map_err(|e| SyncError::Io(std::io::Error::other(format!(
                            "Failed to compress {}: {}",
                            source_path.display(), e
                        ))))?;

                    let compressed_size = compressed_data.len();
                    let ratio = uncompressed_size as f64 / compressed_size as f64;

                    tracing::debug!(
                        "Compressed {}: {} → {} bytes ({:.1}x)",
                        filename,
                        uncompressed_size,
                        compressed_size,
                        ratio
                    );

                    // Get mtime for receive-file command
                    let mtime_secs = metadata.modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());

                    // Send via receive-file command with stdin
                    let dest_path_str = dest_path.to_string_lossy();
                    let mtime_arg = mtime_secs
                        .map(|s| format!("--mtime {}", s))
                        .unwrap_or_default();

                    let command = format!(
                        "{} receive-file {} {}",
                        remote_binary,
                        dest_path_str,
                        mtime_arg
                    );

                    let output = Self::execute_command_with_stdin(
                        Arc::clone(&session_arc),
                        &command,
                        &compressed_data
                    )?;

                    // Parse response to verify
                    #[derive(serde::Deserialize)]
                    struct ReceiveResult {
                        bytes_written: u64,
                    }

                    let result: ReceiveResult = serde_json::from_str(&output)
                        .map_err(|e| SyncError::Io(std::io::Error::other(format!(
                            "Failed to parse receive-file output: {}",
                            e
                        ))))?;

                    tracing::info!(
                        "Transferred {} ({} bytes compressed, {:.1}x reduction)",
                        source_path.display(),
                        compressed_size,
                        ratio
                    );

                    Ok(TransferResult::with_compression(result.bytes_written, compressed_size as u64))
                }
                Compression::None => {
                    tracing::debug!(
                        "File {}: {} bytes, using SFTP streaming (incompressible or too large)",
                        filename,
                        file_size
                    );

                    let session = session_arc.lock().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!("Failed to lock session: {}", e)))
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
                    // 256KB optimal for modern networks (research: SFTP performance)
                    const CHUNK_SIZE: usize = 256 * 1024; // 256KB chunks
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

                    Ok(TransferResult::new(bytes_written))
                }
            }
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
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

                // Compress delta JSON (typically 5-10x reduction for JSON data)
                let uncompressed_size = delta_json.len();
                let compressed_delta = compress(delta_json.as_bytes(), Compression::Zstd)
                    .map_err(|e| SyncError::Io(std::io::Error::other(format!(
                        "Failed to compress delta: {}",
                        e
                    ))))?;
                let compressed_size = compressed_delta.len();

                tracing::debug!(
                    "Delta: {} ops, {} bytes JSON, {} bytes compressed ({:.1}x)",
                    delta.ops.len(),
                    uncompressed_size,
                    compressed_size,
                    uncompressed_size as f64 / compressed_size as f64
                );

                // Apply delta on remote side (avoids uploading full file!)
                // Send compressed delta via stdin to avoid command line length limits
                tracing::debug!("Sending compressed delta to remote for application...");
                let temp_remote_path = format!("{}.sy-tmp", dest_path.display());
                let command = format!(
                    "{} apply-delta {} {}",
                    remote_binary,
                    dest_path_str,
                    temp_remote_path
                );

                let output = tokio::task::block_in_place(|| {
                    Self::execute_command_with_stdin(Arc::clone(&session_arc), &command, &compressed_delta)
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

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        let source_str = source.to_string_lossy();
        let dest_str = dest.to_string_lossy();

        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            let parent_str = parent.to_string_lossy();
            let mkdir_cmd = format!("mkdir -p '{}'", parent_str);
            tokio::task::spawn_blocking({
                let session = Arc::clone(&self.session);
                move || Self::execute_command(session, &mkdir_cmd)
            })
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;
        }

        // Create hardlink using ln command
        // Retry if source doesn't exist yet (can happen in parallel execution)
        let command = format!("ln '{}' '{}'", source_str, dest_str);
        let max_retries = 10;
        let mut last_error = None;

        for attempt in 0..max_retries {
            match tokio::task::spawn_blocking({
                let session = Arc::clone(&self.session);
                let cmd = command.clone();
                move || Self::execute_command(session, &cmd)
            })
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            {
                Ok(_) => {
                    tracing::debug!("Created hardlink: {} -> {}", dest_str, source_str);
                    return Ok(());
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.contains("No such file or directory") && attempt < max_retries - 1 {
                        // Source file not ready yet, wait and retry
                        tracing::debug!("Hardlink source not ready (attempt {}), waiting...", attempt + 1);
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            SyncError::Io(std::io::Error::other("Failed to create hardlink after retries"))
        }))
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        let target_str = target.to_string_lossy();
        let dest_str = dest.to_string_lossy();

        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            let parent_str = parent.to_string_lossy();
            let mkdir_cmd = format!("mkdir -p '{}'", parent_str);
            tokio::task::spawn_blocking({
                let session = Arc::clone(&self.session);
                move || Self::execute_command(session, &mkdir_cmd)
            })
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;
        }

        // Create symlink using ln -s command
        let command = format!("ln -s '{}' '{}'", target_str, dest_str);

        tokio::task::spawn_blocking({
            let session = Arc::clone(&self.session);
            let cmd = command.clone();
            move || Self::execute_command(session, &cmd)
        })
        .await
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))??;

        tracing::debug!("Created symlink: {} -> {}", dest_str, target_str);
        Ok(())
    }
}
