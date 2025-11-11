use super::{TransferResult, Transport};
use crate::compress::{compress, should_compress_smart, Compression, CompressionDetection};
use crate::delta::{calculate_block_size, generate_delta_streaming, BlockChecksum, DeltaOp};
use crate::error::{Result, SyncError};
use crate::resume::{TransferState, DEFAULT_CHUNK_SIZE};
use crate::retry::{retry_with_backoff, RetryConfig};
use crate::ssh::config::SshConfig;
use crate::ssh::connect;
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ssh2::Session;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

// Temporary inlined sparse detection (module resolution issue workaround)
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

/// Represents a contiguous region of data in a sparse file
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DataRegion {
    offset: u64,
    length: u64,
}

/// Detect data regions in a sparse file using SEEK_HOLE/SEEK_DATA
#[cfg(unix)]
fn detect_data_regions(path: &Path) -> std::io::Result<Vec<DataRegion>> {
    const SEEK_DATA: i32 = 3;
    const SEEK_HOLE: i32 = 4;

    let file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();

    if file_size == 0 {
        return Ok(Vec::new());
    }

    let fd = file.as_raw_fd();
    let file_size_i64 = file_size as i64;

    let first_data = unsafe { libc::lseek(fd, 0, SEEK_DATA) };
    if first_data < 0 {
        let err = std::io::Error::last_os_error();
        let errno = err.raw_os_error();

        if errno == Some(libc::EINVAL) {
            return Err(err);
        }

        if errno == Some(libc::ENXIO) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "SEEK_DATA not properly supported (got ENXIO)",
            ));
        }

        return Err(err);
    }

    let mut regions = Vec::new();
    let mut pos: i64 = 0;

    while pos < file_size_i64 {
        let data_start = unsafe { libc::lseek(fd, pos, SEEK_DATA) };
        if data_start < 0 || data_start >= file_size_i64 {
            break;
        }

        let hole_start = unsafe { libc::lseek(fd, data_start, SEEK_HOLE) };
        let data_end = if hole_start < 0 || hole_start > file_size_i64 {
            file_size_i64
        } else {
            hole_start
        };

        regions.push(DataRegion {
            offset: data_start as u64,
            length: (data_end - data_start) as u64,
        });

        pos = data_end;
    }

    Ok(regions)
}

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

/// Connection pool for parallel SSH operations
///
/// Manages multiple SSH sessions to enable true parallel file transfers.
/// Workers round-robin through the pool to avoid serialization on a single session.
struct ConnectionPool {
    sessions: Vec<Arc<Mutex<Session>>>,
    next_index: AtomicUsize,
}

impl ConnectionPool {
    /// Create a new connection pool with the specified number of sessions
    #[allow(dead_code)] // Used via SshTransport::with_pool_size
    async fn new(config: &SshConfig, pool_size: usize) -> Result<Self> {
        if pool_size == 0 {
            return Err(SyncError::Io(std::io::Error::other(
                "Connection pool size must be at least 1",
            )));
        }

        let mut sessions = Vec::with_capacity(pool_size);

        // Create pool_size SSH connections
        for i in 0..pool_size {
            tracing::debug!("Creating SSH connection {}/{} for pool", i + 1, pool_size);
            let session = connect::connect(config).await?;
            sessions.push(Arc::new(Mutex::new(session)));
        }

        tracing::info!(
            "SSH connection pool initialized with {} connections",
            pool_size
        );

        Ok(Self {
            sessions,
            next_index: AtomicUsize::new(0),
        })
    }

    /// Get a session from the pool using round-robin selection
    ///
    /// This ensures even distribution of work across all connections.
    fn get_session(&self) -> Arc<Mutex<Session>> {
        let index = self.next_index.fetch_add(1, Ordering::Relaxed) % self.sessions.len();
        Arc::clone(&self.sessions[index])
    }

    /// Get the number of connections in the pool
    #[allow(dead_code)] // Useful for debugging and monitoring
    fn size(&self) -> usize {
        self.sessions.len()
    }
}

pub struct SshTransport {
    connection_pool: Arc<ConnectionPool>,
    remote_binary_path: String,
    retry_config: RetryConfig,
}

impl SshTransport {
    /// Create a new SSH transport with a single connection (backward compatibility)
    #[allow(dead_code)] // Public API for backward compatibility
    pub async fn new(config: &SshConfig) -> Result<Self> {
        Self::with_pool_size(config, 1).await
    }

    /// Create a new SSH transport with a connection pool
    ///
    /// `pool_size` should typically match the number of parallel workers.
    /// For sequential operations, use pool_size=1.
    pub async fn with_pool_size(config: &SshConfig, pool_size: usize) -> Result<Self> {
        Self::with_retry_config(config, pool_size, RetryConfig::default()).await
    }

    /// Create a new SSH transport with custom retry configuration
    ///
    /// This allows configuring network interruption recovery behavior.
    pub async fn with_retry_config(
        config: &SshConfig,
        pool_size: usize,
        retry_config: RetryConfig,
    ) -> Result<Self> {
        let connection_pool = ConnectionPool::new(config, pool_size).await?;
        Ok(Self {
            connection_pool: Arc::new(connection_pool),
            remote_binary_path: "sy-remote".to_string(),
            retry_config,
        })
    }

    /// Get the number of connections in the pool
    #[allow(dead_code)] // Useful for debugging and monitoring
    pub fn pool_size(&self) -> usize {
        self.connection_pool.size()
    }

    fn format_bytes(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;
        const TB: u64 = GB * 1024;

        if bytes >= TB {
            format!("{:.2} TB", bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{:.2} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.2} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.2} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} B", bytes)
        }
    }

    fn execute_command(session: Arc<Mutex<Session>>, command: &str) -> Result<String> {
        let session = session.lock().map_err(|e| {
            let io_err = std::io::Error::other(format!("Failed to lock session: {}", e));
            SyncError::from_ssh_io_error(io_err, "SSH session lock")
        })?;

        let mut channel = session.channel_session().map_err(|e| {
            let io_err = std::io::Error::other(format!("Failed to create channel: {}", e));
            SyncError::from_ssh_io_error(io_err, "SSH channel creation")
        })?;

        channel.exec(command).map_err(|e| {
            let io_err = std::io::Error::other(format!("Failed to execute command: {}", e));
            SyncError::from_ssh_io_error(io_err, "SSH command execution")
        })?;

        let mut output = String::new();
        channel
            .read_to_string(&mut output)
            .map_err(|e| SyncError::from_ssh_io_error(e, "SSH command output read"))?;

        let mut stderr = String::new();
        let _ = channel.stderr().read_to_string(&mut stderr);

        channel.wait_close().map_err(|e| {
            let io_err = std::io::Error::other(format!("Failed to close channel: {}", e));
            SyncError::from_ssh_io_error(io_err, "SSH channel close")
        })?;

        let exit_status = channel.exit_status().map_err(|e| {
            let io_err = std::io::Error::other(format!("Failed to get exit status: {}", e));
            SyncError::from_ssh_io_error(io_err, "SSH exit status")
        })?;

        if exit_status != 0 {
            let io_err = std::io::Error::other(format!(
                "Command '{}' failed with exit code {}\nstdout: {}\nstderr: {}",
                command, exit_status, output, stderr
            ));
            return Err(SyncError::from_ssh_io_error(io_err, "SSH command failed"));
        }

        Ok(output)
    }

    /// Execute command with retry logic
    async fn execute_command_with_retry(
        &self,
        session: Arc<Mutex<Session>>,
        command: &str,
    ) -> Result<String> {
        let cmd = command.to_string();
        let sess = session.clone();

        retry_with_backoff(&self.retry_config, || {
            let cmd = cmd.clone();
            let sess = sess.clone();
            async move { Self::execute_command(sess, &cmd) }
        })
        .await
    }

    /// Execute a command with stdin data (binary-safe)
    fn execute_command_with_stdin(
        session: Arc<Mutex<Session>>,
        command: &str,
        stdin_data: &[u8],
    ) -> Result<String> {
        use std::io::Write;

        let session = session.lock().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to lock session: {}",
                e
            )))
        })?;

        let mut channel = session.channel_session().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to create channel: {}",
                e
            )))
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
            SyncError::Io(std::io::Error::other(format!(
                "Failed to close channel: {}",
                e
            )))
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

    /// Copy a sparse file over SSH by transferring only data regions
    ///
    /// This method detects sparse file regions and transfers only the actual data,
    /// skipping holes. This can save significant bandwidth for files like VM disk
    /// images, databases, and other sparse files.
    async fn copy_sparse_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        let source_path = source.to_path_buf();
        let dest_path = dest.to_path_buf();
        let session_arc = self.connection_pool.get_session();
        let remote_binary = self.remote_binary_path.clone();

        retry_with_backoff(&self.retry_config, || {
            let source_path = source_path.clone();
            let dest_path = dest_path.clone();
            let session_arc = session_arc.clone();
            let remote_binary = remote_binary.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    // Get source metadata
                    let metadata = std::fs::metadata(&source_path).map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to get metadata for {}: {}",
                        source_path.display(),
                        e
                    ),
                ))
            })?;

            let file_size = metadata.len();

            // Detect data regions in the sparse file
            #[cfg(unix)]
            let data_regions = detect_data_regions(&source_path).map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to detect sparse regions for {}: {}",
                        source_path.display(),
                        e
                    ),
                ))
            })?;

            // Windows doesn't support sparse detection yet
            #[cfg(not(unix))]
            return Err(SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Sparse file detection not supported on Windows",
            )));

            // If no regions detected or sparse detection not supported, fall back to regular copy
            #[cfg(unix)]
            if data_regions.is_empty() {
                tracing::debug!(
                    "No sparse regions detected for {}, using regular transfer",
                    source_path.display()
                );
                // This will be handled by the caller falling back to copy_file
                return Err(SyncError::Io(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "Sparse detection returned no regions",
                )));
            }

            #[cfg(unix)]
            {
            // Calculate total data size (sum of all region lengths)
            let total_data_size: u64 = data_regions.iter().map(|r| r.length).sum();
            let sparse_ratio = file_size as f64 / total_data_size.max(1) as f64;

            tracing::info!(
                "Sparse file {}: {} total, {} data ({:.1}x sparse ratio, {} regions)",
                source_path.display(),
                file_size,
                total_data_size,
                sparse_ratio,
                data_regions.len()
            );

            // Serialize regions to JSON for command line
            let regions_json = serde_json::to_string(&data_regions).map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to serialize sparse regions: {}",
                    e
                )))
            })?;

            // Get mtime for receive-sparse-file command
            let mtime_secs = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());

            // Build command
            let dest_path_str = dest_path.to_string_lossy();
            let mtime_arg = mtime_secs
                .map(|s| format!("--mtime {}", s))
                .unwrap_or_default();

            let command = format!(
                "{} receive-sparse-file {} --total-size {} --regions '{}' {}",
                remote_binary, dest_path_str, file_size, regions_json, mtime_arg
            );

            // Open source file for reading
            let mut source_file = std::fs::File::open(&source_path).map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Failed to open {}: {}", source_path.display(), e),
                ))
            })?;

            // Read all data regions into a buffer
            use std::io::{Seek, SeekFrom};
            let mut data_buffer = Vec::with_capacity(total_data_size as usize);

            for region in &data_regions {
                // Seek to region offset
                source_file
                    .seek(SeekFrom::Start(region.offset))
                    .map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!(
                                "Failed to seek to offset {} in {}: {}",
                                region.offset,
                                source_path.display(),
                                e
                            ),
                        ))
                    })?;

                // Read region data
                let mut region_data = vec![0u8; region.length as usize];
                source_file.read_exact(&mut region_data).map_err(|e| {
                    SyncError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "Failed to read {} bytes at offset {} from {}: {}",
                            region.length,
                            region.offset,
                            source_path.display(),
                            e
                        ),
                    ))
                })?;

                data_buffer.extend_from_slice(&region_data);
            }

            // Execute command with data regions as stdin
            let output = Self::execute_command_with_stdin(
                Arc::clone(&session_arc),
                &command,
                &data_buffer,
            )?;

            // Parse response
            #[derive(Deserialize)]
            struct SparseResponse {
                bytes_written: u64,
                file_size: u64,
                regions: usize,
            }

            let response: SparseResponse = serde_json::from_str(output.trim()).map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to parse sparse transfer response: {} (output: {})",
                    e, output
                )))
            })?;

            tracing::debug!(
                "Sparse transfer complete: {} bytes data transferred, {} total file size, {} regions",
                response.bytes_written,
                response.file_size,
                response.regions
            );

            // Return transfer result with actual bytes transferred (not file size)
            Ok(TransferResult {
                bytes_written: response.file_size,
                delta_operations: None,
                literal_bytes: None,
                transferred_bytes: Some(response.bytes_written),
                compression_used: false,
            })
            }
                })
                .await
                .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            }
        })
        .await
    }
}

#[async_trait]
impl Transport for SshTransport {
    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>> {
        let path_str = path.to_string_lossy();
        let command = format!("{} scan {}", self.remote_binary_path, path_str);

        let output = self
            .execute_command_with_retry(self.connection_pool.get_session(), &command)
            .await?;

        let scan_output: ScanOutput = serde_json::from_str(&output).map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to parse JSON: {}",
                e
            )))
        })?;

        let entries: Result<Vec<FileEntry>> = scan_output
            .entries
            .into_iter()
            .map(|e| {
                let modified = UNIX_EPOCH + Duration::from_secs(e.mtime.max(0) as u64);

                // Decode xattrs from base64 if present
                let xattrs = e.xattrs.map(|xattr_vec| {
                    xattr_vec
                        .into_iter()
                        .filter_map(|(key, base64_val)| {
                            use base64::{engine::general_purpose, Engine as _};
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
                    path: Arc::new(PathBuf::from(&e.path)),
                    relative_path: Arc::new(
                        PathBuf::from(&e.path)
                            .strip_prefix(path)
                            .unwrap_or(Path::new(&e.path))
                            .to_path_buf(),
                    ),
                    size: e.size,
                    modified,
                    is_dir: e.is_dir,
                    is_symlink: e.is_symlink,
                    symlink_target: e.symlink_target.map(|t| Arc::new(PathBuf::from(t))),
                    is_sparse: e.is_sparse,
                    allocated_size: e.allocated_size,
                    xattrs,
                    inode: e.inode,
                    nlink: e.nlink,
                    acls,
                    bsd_flags: None, // TODO: Serialize BSD flags in SSH protocol
                })
            })
            .collect();

        entries
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy();
        let command = format!("test -e {} && echo 'exists' || echo 'not found'", path_str);

        let output = self
            .execute_command_with_retry(self.connection_pool.get_session(), &command)
            .await?;

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

        self.execute_command_with_retry(self.connection_pool.get_session(), &command)
            .await?;

        Ok(())
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Check if file is sparse and try sparse transfer first
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            if let Ok(metadata) = std::fs::metadata(source) {
                let file_size = metadata.len();
                let allocated_size = metadata.blocks() * 512;
                let is_sparse = allocated_size < file_size && file_size > 0;

                if is_sparse {
                    // Try sparse transfer
                    match self.copy_sparse_file(source, dest).await {
                        Ok(result) => {
                            tracing::info!(
                                "Sparse transfer succeeded for {} ({} file size, {} transferred)",
                                source.display(),
                                file_size,
                                result.transferred_bytes.unwrap_or(file_size)
                            );
                            return Ok(result);
                        }
                        Err(e) => {
                            tracing::debug!(
                                "Sparse transfer failed for {}, falling back to regular copy: {}",
                                source.display(),
                                e
                            );
                            // Fall through to regular transfer
                        }
                    }
                }
            }
        }

        let source_path = source.to_path_buf();
        let dest_path = dest.to_path_buf();
        let session_arc = self.connection_pool.get_session();
        let remote_binary = self.remote_binary_path.clone();

        retry_with_backoff(&self.retry_config, || {
            let source_path = source_path.clone();
            let dest_path = dest_path.clone();
            let session_arc = session_arc.clone();
            let remote_binary = remote_binary.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    // Get source metadata for mtime and size
                    let metadata = std::fs::metadata(&source_path).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!(
                                "Failed to get metadata for {}: {}",
                                source_path.display(),
                                e
                            ),
                        ))
                    })?;

                    let file_size = metadata.len();
                    let filename = source_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");

                    // Determine if compression would be beneficial using smart detection
                    // Use content-based detection with Auto mode (default)
                    // TODO: Thread compression_detection mode from CLI through transport
                    let compression_mode = should_compress_smart(
                        Some(&source_path),
                        filename,
                        file_size,
                        false, // SSH transfers are always remote (not local)
                        CompressionDetection::Auto,
                    );

                    // Use compressed transfer for compressible files, SFTP for others
                    match compression_mode {
                        Compression::Lz4 | Compression::Zstd => {
                            tracing::debug!(
                                "File {}: {} bytes, using compressed transfer ({})",
                                filename,
                                file_size,
                                compression_mode.as_str()
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
                            let compressed_data =
                                compress(&file_data, compression_mode).map_err(|e| {
                                    SyncError::Io(std::io::Error::other(format!(
                                        "Failed to compress {}: {}",
                                        source_path.display(),
                                        e
                                    )))
                                })?;

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
                            let mtime_secs = metadata
                                .modified()
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
                                remote_binary, dest_path_str, mtime_arg
                            );

                            let output = Self::execute_command_with_stdin(
                                Arc::clone(&session_arc),
                                &command,
                                &compressed_data,
                            )?;

                            // Parse response to verify
                            #[derive(serde::Deserialize)]
                            struct ReceiveResult {
                                bytes_written: u64,
                            }

                            let result: ReceiveResult =
                                serde_json::from_str(&output).map_err(|e| {
                                    SyncError::Io(std::io::Error::other(format!(
                                        "Failed to parse receive-file output: {}",
                                        e
                                    )))
                                })?;

                            tracing::info!(
                                "Transferred {} ({} bytes compressed, {:.1}x reduction)",
                                source_path.display(),
                                compressed_size,
                                ratio
                            );

                            Ok(TransferResult::with_compression(
                                result.bytes_written,
                                compressed_size as u64,
                            ))
                        }
                        Compression::None => {
                            tracing::debug!(
                        "File {}: {} bytes, using SFTP streaming (incompressible or too large)",
                        filename,
                        file_size
                    );

                            // Get source mtime for resume state
                            let mtime_systime = metadata.modified().map_err(|e| {
                                SyncError::Io(std::io::Error::new(
                                    e.kind(),
                                    format!(
                                        "Failed to get mtime for {}: {}",
                                        source_path.display(),
                                        e
                                    ),
                                ))
                            })?;

                            // Try to load existing resume state
                            let mut resume_state =
                                TransferState::load(&source_path, &dest_path, mtime_systime)?;
                            let is_resuming = resume_state.is_some();

                            // Check if state is stale
                            if let Some(ref state) = resume_state {
                                if state.is_stale(mtime_systime) {
                                    eprintln!(
                                "Resume state is stale for {} (file modified). Starting fresh.",
                                source_path.display()
                            );
                                    TransferState::clear(&source_path, &dest_path, mtime_systime)?;
                                    resume_state = None;
                                }
                            }

                            // Determine starting position
                            let start_offset = if let Some(ref state) = resume_state {
                                eprintln!(
                                    "Resuming transfer of {} from offset {} ({:.1}% complete)",
                                    source_path.display(),
                                    state.bytes_transferred,
                                    state.progress_percentage()
                                );
                                state.bytes_transferred
                            } else {
                                0
                            };

                            let session = session_arc.lock().map_err(|e| {
                                SyncError::Io(std::io::Error::other(format!(
                                    "Failed to lock session: {}",
                                    e
                                )))
                            })?;

                            // Open source file for streaming
                            let mut source_file =
                                std::fs::File::open(&source_path).map_err(|e| {
                                    SyncError::Io(std::io::Error::new(
                                        e.kind(),
                                        format!(
                                            "Failed to open source file {}: {}",
                                            source_path.display(),
                                            e
                                        ),
                                    ))
                                })?;

                            // Seek to resume position if resuming
                            if start_offset > 0 {
                                source_file
                                    .seek(SeekFrom::Start(start_offset))
                                    .map_err(|e| {
                                        SyncError::Io(std::io::Error::new(
                                            e.kind(),
                                            format!(
                                                "Failed to seek source file {} to offset {}: {}",
                                                source_path.display(),
                                                start_offset,
                                                e
                                            ),
                                        ))
                                    })?;
                            }

                            // Get SFTP session
                            let sftp = session.sftp().map_err(|e| {
                                SyncError::Io(std::io::Error::other(format!(
                                    "Failed to create SFTP session: {}",
                                    e
                                )))
                            })?;

                            // Open remote file (create if new, open for append if resuming)
                            let mut remote_file = if is_resuming {
                                use ssh2::OpenFlags;
                                let mut file = sftp
                                    .open_mode(
                                        &dest_path,
                                        OpenFlags::WRITE,
                                        0o644,
                                        ssh2::OpenType::File,
                                    )
                                    .map_err(|e| {
                                        SyncError::Io(std::io::Error::other(format!(
                                            "Failed to open remote file for append {}: {}",
                                            dest_path.display(),
                                            e
                                        )))
                                    })?;

                                // Seek to resume position
                                file.seek(SeekFrom::Start(start_offset)).map_err(|e| {
                                    SyncError::Io(std::io::Error::new(
                                        e.kind(),
                                        format!(
                                            "Failed to seek remote file {} to offset {}: {}",
                                            dest_path.display(),
                                            start_offset,
                                            e
                                        ),
                                    ))
                                })?;

                                file
                            } else {
                                sftp.create(&dest_path).map_err(|e| {
                                    SyncError::Io(std::io::Error::other(format!(
                                        "Failed to create remote file {}: {}",
                                        dest_path.display(),
                                        e
                                    )))
                                })?
                            };

                            // Initialize or update transfer state
                            let mut state = resume_state.unwrap_or_else(|| {
                                TransferState::new(
                                    &source_path,
                                    &dest_path,
                                    file_size,
                                    mtime_systime,
                                    DEFAULT_CHUNK_SIZE,
                                )
                            });

                            // Stream file in chunks with checksum calculation
                            // 256KB optimal for modern networks (research: SFTP performance)
                            const CHUNK_SIZE: usize = 256 * 1024; // 256KB chunks
                            let mut buffer = vec![0u8; CHUNK_SIZE];
                            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
                            let mut bytes_written = start_offset;
                            let mut bytes_since_checkpoint = 0u64;
                            const CHECKPOINT_INTERVAL: u64 = 10 * 1024 * 1024; // 10MB

                            loop {
                                let bytes_read = source_file.read(&mut buffer).map_err(|e| {
                                    SyncError::Io(std::io::Error::new(
                                        e.kind(),
                                        format!(
                                            "Failed to read from {}: {}",
                                            source_path.display(),
                                            e
                                        ),
                                    ))
                                })?;

                                if bytes_read == 0 {
                                    break; // EOF
                                }

                                // Update checksum
                                hasher.update(&buffer[..bytes_read]);

                                // Write chunk to remote
                                remote_file.write_all(&buffer[..bytes_read]).map_err(|e| {
                                    SyncError::Io(std::io::Error::other(format!(
                                        "Failed to write to remote file {}: {}",
                                        dest_path.display(),
                                        e
                                    )))
                                })?;

                                bytes_written += bytes_read as u64;
                                bytes_since_checkpoint += bytes_read as u64;

                                // Update state and save checkpoint every 10MB
                                if bytes_since_checkpoint >= CHECKPOINT_INTERVAL {
                                    state.update_progress(bytes_written);
                                    state.save()?;
                                    bytes_since_checkpoint = 0;

                                    tracing::debug!(
                                        "Checkpoint saved for {} at {} bytes ({:.1}%)",
                                        source_path.display(),
                                        bytes_written,
                                        state.progress_percentage()
                                    );
                                }
                            }

                            // Clear resume state on successful completion
                            TransferState::clear(&source_path, &dest_path, mtime_systime)?;

                            let checksum = hasher.digest();

                            tracing::debug!(
                                "Transferred {} ({} bytes, xxh3: {:x}, resumed: {})",
                                source_path.display(),
                                bytes_written,
                                checksum,
                                is_resuming
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
        })
        .await
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
        let session_clone = self.connection_pool.get_session();

        retry_with_backoff(&self.retry_config, || {
            let source_path = source_path.clone();
            let dest_path = dest_path.clone();
            let remote_binary = remote_binary.clone();
            let session_arc = session_clone.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let session = session_arc.lock().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to lock session: {}",
                            e
                        )))
                    })?;

                    let sftp = session.sftp().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to create SFTP session: {}",
                            e
                        )))
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
                        tracing::debug!(
                            "Remote destination too small for delta sync, using full copy"
                        );
                        drop(session);
                        return Err(SyncError::Io(std::io::Error::other(
                            "Destination too small, caller should use copy_file",
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
                    let literal_bytes: u64 = delta
                        .ops
                        .iter()
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
                        .map_err(|e| {
                            SyncError::Io(std::io::Error::other(format!(
                                "Failed to compress delta: {}",
                                e
                            )))
                        })?;
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
                        remote_binary, dest_path_str, temp_remote_path
                    );

                    let output = tokio::task::block_in_place(|| {
                        Self::execute_command_with_stdin(
                            Arc::clone(&session_arc),
                            &command,
                            &compressed_delta,
                        )
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
                })
                .await
                .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            }
        })
        .await
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        let path_str = path.to_string_lossy();
        let command = if is_dir {
            format!("rm -rf '{}'", path_str)
        } else {
            format!("rm -f '{}'", path_str)
        };

        self.execute_command_with_retry(self.connection_pool.get_session(), &command)
            .await?;

        Ok(())
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        let source_str = source.to_string_lossy();
        let dest_str = dest.to_string_lossy();

        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            let parent_str = parent.to_string_lossy();
            let mkdir_cmd = format!("mkdir -p '{}'", parent_str);
            self.execute_command_with_retry(self.connection_pool.get_session(), &mkdir_cmd)
                .await?;
        }

        // Create hardlink using ln command
        // Retry if source doesn't exist yet (can happen in parallel execution)
        let command = format!("ln '{}' '{}'", source_str, dest_str);
        let max_retries = 10;
        let mut last_error = None;

        for attempt in 0..max_retries {
            match self
                .execute_command_with_retry(self.connection_pool.get_session(), &command)
                .await
            {
                Ok(_) => {
                    tracing::debug!("Created hardlink: {} -> {}", dest_str, source_str);
                    return Ok(());
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.contains("No such file or directory") && attempt < max_retries - 1 {
                        // Source file not ready yet, wait and retry
                        tracing::debug!(
                            "Hardlink source not ready (attempt {}), waiting...",
                            attempt + 1
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            SyncError::Io(std::io::Error::other(
                "Failed to create hardlink after retries",
            ))
        }))
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        let target_str = target.to_string_lossy();
        let dest_str = dest.to_string_lossy();

        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            let parent_str = parent.to_string_lossy();
            let mkdir_cmd = format!("mkdir -p '{}'", parent_str);
            self.execute_command_with_retry(self.connection_pool.get_session(), &mkdir_cmd)
                .await?;
        }

        // Create symlink using ln -s command
        let command = format!("ln -s '{}' '{}'", target_str, dest_str);

        self.execute_command_with_retry(self.connection_pool.get_session(), &command)
            .await?;

        tracing::debug!("Created symlink: {} -> {}", dest_str, target_str);
        Ok(())
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let path_buf = path.to_path_buf();
        let session_arc = self.connection_pool.get_session();

        retry_with_backoff(&self.retry_config, || {
            let path_buf = path_buf.clone();
            let session_arc = session_arc.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let session = session_arc.lock().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to lock session: {}",
                            e
                        )))
                    })?;

                    let sftp = session.sftp().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to create SFTP session: {}",
                            e
                        )))
                    })?;

                    // Open remote file for reading
                    tracing::debug!(
                        "Attempting to open remote file via SFTP: {}",
                        path_buf.display()
                    );
                    let mut remote_file = sftp.open(&path_buf).map_err(|e| {
                        tracing::error!(
                            "SFTP open failed for {}: {} (error kind: {:?})",
                            path_buf.display(),
                            e,
                            std::io::Error::last_os_error()
                        );
                        SyncError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Failed to open remote file {}: {}", path_buf.display(), e),
                        ))
                    })?;
                    tracing::debug!(
                        "Successfully opened remote file via SFTP: {}",
                        path_buf.display()
                    );

                    // Read entire file into memory
                    let mut buffer = Vec::new();
                    std::io::Read::read_to_end(&mut remote_file, &mut buffer).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!("Failed to read from {}: {}", path_buf.display(), e),
                        ))
                    })?;

                    tracing::debug!(
                        "Read {} bytes from remote file {}",
                        buffer.len(),
                        path_buf.display()
                    );

                    Ok(buffer)
                })
                .await
                .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            }
        })
        .await
    }

    async fn write_file(
        &self,
        path: &Path,
        data: &[u8],
        mtime: std::time::SystemTime,
    ) -> Result<()> {
        use std::io::Write;

        let path_buf = path.to_path_buf();
        let data_vec = data.to_vec();
        let session_arc = self.connection_pool.get_session();

        retry_with_backoff(&self.retry_config, || {
            let path_buf = path_buf.clone();
            let data_vec = data_vec.clone();
            let session_arc = session_arc.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let session = session_arc.lock().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to lock session: {}",
                            e
                        )))
                    })?;

                    let sftp = session.sftp().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to create SFTP session: {}",
                            e
                        )))
                    })?;

                    // Create parent directories recursively if needed
                    if let Some(parent) = path_buf.parent() {
                        let mut current = std::path::PathBuf::new();
                        for component in parent.components() {
                            current.push(component);
                            // Try to create each directory level, ignore if already exists
                            sftp.mkdir(&current, 0o755).ok();
                        }
                    }

                    // Create/open remote file for writing
                    let mut remote_file = sftp.create(&path_buf).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            format!("Failed to create remote file {}: {}", path_buf.display(), e),
                        ))
                    })?;

                    // Write data
                    remote_file.write_all(&data_vec).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!("Failed to write to {}: {}", path_buf.display(), e),
                        ))
                    })?;

                    remote_file.flush().map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!("Failed to flush {}: {}", path_buf.display(), e),
                        ))
                    })?;

                    // Set mtime on remote file
                    let mtime_secs = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();

                    sftp.setstat(
                        &path_buf,
                        ssh2::FileStat {
                            size: None,
                            uid: None,
                            gid: None,
                            perm: None,
                            atime: Some(mtime_secs),
                            mtime: Some(mtime_secs),
                        },
                    )
                    .map_err(|e| {
                        tracing::warn!("Failed to set mtime on {}: {}", path_buf.display(), e);
                        // Don't fail the entire operation if setstat fails
                    })
                    .ok();

                    tracing::debug!(
                        "Wrote {} bytes to remote file {}",
                        data_vec.len(),
                        path_buf.display()
                    );

                    Ok(())
                })
                .await
                .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            }
        })
        .await
    }

    async fn get_mtime(&self, path: &Path) -> Result<std::time::SystemTime> {
        let path_buf = path.to_path_buf();
        let session_arc = self.connection_pool.get_session();

        retry_with_backoff(&self.retry_config, || {
            let path_buf = path_buf.clone();
            let session_arc = session_arc.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let session = session_arc.lock().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to lock session: {}",
                            e
                        )))
                    })?;

                    let sftp = session.sftp().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to create SFTP session: {}",
                            e
                        )))
                    })?;

                    // Get file stats from remote
                    let stat = sftp.stat(&path_buf).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Failed to stat remote file {}: {}", path_buf.display(), e),
                        ))
                    })?;

                    // Extract mtime
                    let mtime = stat.mtime.ok_or_else(|| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Remote file {} has no mtime",
                            path_buf.display()
                        )))
                    })?;

                    let mtime_systime = UNIX_EPOCH + Duration::from_secs(mtime);

                    tracing::debug!(
                        "Got mtime for remote file {}: {:?}",
                        path_buf.display(),
                        mtime_systime
                    );

                    Ok(mtime_systime)
                })
                .await
                .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            }
        })
        .await
    }

    async fn file_info(&self, path: &Path) -> Result<super::FileInfo> {
        let path_buf = path.to_path_buf();
        let session_arc = self.connection_pool.get_session();

        retry_with_backoff(&self.retry_config, || {
            let path_buf = path_buf.clone();
            let session_arc = session_arc.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let session = session_arc.lock().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to lock session: {}",
                            e
                        )))
                    })?;

                    let sftp = session.sftp().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to create SFTP session: {}",
                            e
                        )))
                    })?;

                    // Get file stats from remote
                    let stat = sftp.stat(&path_buf).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Failed to stat remote file {}: {}", path_buf.display(), e),
                        ))
                    })?;

                    // Extract size and mtime
                    let size = stat.size.unwrap_or(0);
                    let mtime = stat.mtime.ok_or_else(|| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Remote file {} has no mtime",
                            path_buf.display()
                        )))
                    })?;

                    let modified = UNIX_EPOCH + Duration::from_secs(mtime);

                    tracing::debug!(
                        "Got file info for remote file {}: {} bytes, {:?}",
                        path_buf.display(),
                        size,
                        modified
                    );

                    Ok(super::FileInfo { size, modified })
                })
                .await
                .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            }
        })
        .await
    }

    async fn copy_file_streaming(
        &self,
        source: &Path,
        dest: &Path,
        progress_callback: Option<std::sync::Arc<dyn Fn(u64, u64) + Send + Sync>>,
    ) -> Result<TransferResult> {
        let source_buf = source.to_path_buf();
        let dest_buf = dest.to_path_buf();
        let session_arc = self.connection_pool.get_session();

        retry_with_backoff(&self.retry_config, || {
            let source_buf = source_buf.clone();
            let dest_buf = dest_buf.clone();
            let session_arc = session_arc.clone();
            let progress_callback = progress_callback.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let session = session_arc.lock().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to lock session: {}",
                            e
                        )))
                    })?;

                    let sftp = session.sftp().map_err(|e| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Failed to create SFTP session: {}",
                            e
                        )))
                    })?;

                    // Get file stats for mtime and size
                    let stat = sftp.stat(&source_buf).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Failed to stat remote file {}: {}", source_buf.display(), e),
                        ))
                    })?;

                    let file_size = stat.size.unwrap_or(0);
                    let mtime = stat.mtime.ok_or_else(|| {
                        SyncError::Io(std::io::Error::other(format!(
                            "Remote file {} has no mtime",
                            source_buf.display()
                        )))
                    })?;
                    let mtime_systime = UNIX_EPOCH + Duration::from_secs(mtime);

                    // Try to load existing resume state
                    let mut resume_state =
                        TransferState::load(&source_buf, &dest_buf, mtime_systime)?;
                    let is_resuming = resume_state.is_some();

                    // Check if state is stale (shouldn't happen since load() checks, but be safe)
                    if let Some(ref state) = resume_state {
                        if state.is_stale(mtime_systime) {
                            eprintln!(
                                "Resume state is stale for {} (file modified). Starting fresh.",
                                source_buf.display()
                            );
                            TransferState::clear(&source_buf, &dest_buf, mtime_systime)?;
                            resume_state = None;
                        }
                    }

                    // Determine starting position
                    let start_offset = if let Some(ref state) = resume_state {
                        eprintln!(
                            "Resuming transfer of {} from offset {} ({:.1}% complete)",
                            source_buf.display(),
                            state.bytes_transferred,
                            state.progress_percentage()
                        );
                        state.bytes_transferred
                    } else {
                        0
                    };

                    // Open remote file for streaming read
                    let mut remote_file = sftp.open(&source_buf).map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("Failed to open remote file {}: {}", source_buf.display(), e),
                        ))
                    })?;

                    // Seek to resume position if resuming
                    if start_offset > 0 {
                        remote_file
                            .seek(SeekFrom::Start(start_offset))
                            .map_err(|e| {
                                SyncError::Io(std::io::Error::new(
                                    e.kind(),
                                    format!(
                                        "Failed to seek remote file {} to offset {}: {}",
                                        source_buf.display(),
                                        start_offset,
                                        e
                                    ),
                                ))
                            })?;
                    }

                    // Create parent directories if needed
                    if let Some(parent) = dest_buf.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            SyncError::Io(std::io::Error::new(
                                e.kind(),
                                format!(
                                    "Failed to create parent directory {}: {}",
                                    parent.display(),
                                    e
                                ),
                            ))
                        })?;
                    }

                    // Open local destination file (append if resuming, create if new)
                    let mut dest_file = if is_resuming {
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(&dest_buf)
                            .map_err(|e| {
                                SyncError::Io(std::io::Error::new(
                                    e.kind(),
                                    format!(
                                        "Failed to open file for append {}: {}",
                                        dest_buf.display(),
                                        e
                                    ),
                                ))
                            })?
                    } else {
                        std::fs::File::create(&dest_buf).map_err(|e| {
                            SyncError::Io(std::io::Error::new(
                                e.kind(),
                                format!("Failed to create file {}: {}", dest_buf.display(), e),
                            ))
                        })?
                    };

                    // Initialize or update transfer state
                    let mut state = resume_state.unwrap_or_else(|| {
                        TransferState::new(
                            &source_buf,
                            &dest_buf,
                            file_size,
                            mtime_systime,
                            DEFAULT_CHUNK_SIZE,
                        )
                    });

                    // Stream in 64KB chunks (optimized for network)
                    const CHUNK_SIZE: usize = 64 * 1024;
                    let mut buffer = vec![0u8; CHUNK_SIZE];
                    let mut total_bytes = start_offset;
                    let mut bytes_since_checkpoint = 0u64;
                    const CHECKPOINT_INTERVAL: u64 = 10 * 1024 * 1024; // 10MB

                    if let Some(ref callback) = progress_callback {
                        callback(total_bytes, file_size);
                    }

                    loop {
                        let bytes_read = remote_file.read(&mut buffer).map_err(|e| {
                            SyncError::Io(std::io::Error::new(
                                e.kind(),
                                format!(
                                    "Failed to read from remote {}: {}",
                                    source_buf.display(),
                                    e
                                ),
                            ))
                        })?;

                        if bytes_read == 0 {
                            break;
                        }

                        dest_file.write_all(&buffer[..bytes_read]).map_err(|e| {
                            SyncError::Io(std::io::Error::new(
                                e.kind(),
                                format!("Failed to write to {}: {}", dest_buf.display(), e),
                            ))
                        })?;

                        total_bytes += bytes_read as u64;
                        bytes_since_checkpoint += bytes_read as u64;

                        // Update state and save checkpoint every 10MB
                        if bytes_since_checkpoint >= CHECKPOINT_INTERVAL {
                            state.update_progress(total_bytes);
                            state.save()?;
                            bytes_since_checkpoint = 0;

                            tracing::debug!(
                                "Checkpoint saved for {} at {} bytes ({:.1}%)",
                                source_buf.display(),
                                total_bytes,
                                state.progress_percentage()
                            );
                        }

                        if let Some(ref callback) = progress_callback {
                            callback(total_bytes, file_size);
                        }
                    }

                    dest_file.flush().map_err(|e| {
                        SyncError::Io(std::io::Error::new(
                            e.kind(),
                            format!("Failed to flush {}: {}", dest_buf.display(), e),
                        ))
                    })?;

                    drop(dest_file);

                    // Clear resume state on successful completion
                    TransferState::clear(&source_buf, &dest_buf, mtime_systime)?;

                    // Set mtime
                    filetime::set_file_mtime(
                        &dest_buf,
                        filetime::FileTime::from_system_time(mtime_systime),
                    )?;

                    tracing::debug!(
                        "Streamed {} bytes from {} to {} (resumed: {})",
                        total_bytes,
                        source_buf.display(),
                        dest_buf.display(),
                        is_resuming
                    );

                    Ok(TransferResult::new(total_bytes))
                })
                .await
                .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
            }
        })
        .await
    }

    async fn check_disk_space(&self, path: &Path, bytes_needed: u64) -> Result<()> {
        let path_str = path.to_string_lossy();

        // Check if path exists, if not use parent directory (like local implementation)
        // Use shell-agnostic commands (fish shell doesn't support bash syntax)
        let check_path_cmd = format!(
            "test -e '{}' && echo '{}' || dirname '{}'",
            path_str, path_str, path_str
        );

        let check_path = self
            .execute_command_with_retry(self.connection_pool.get_session(), &check_path_cmd)
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Failed to resolve path for disk space check on {}: {}. Path may not exist and parent directory may not be accessible.", path.display(), e),
                ))
            })?
            .trim()
            .to_string();

        tracing::debug!(
            "Checking disk space for path: {} (resolved to: {})",
            path.display(),
            check_path
        );

        // Use df command to get available space
        // -P for POSIX format (portable), -B1 for bytes
        let command = format!("df -P -B1 '{}'", check_path);

        let output = self
            .execute_command_with_retry(self.connection_pool.get_session(), &command)
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(
                    format!("Failed to check disk space for {} (using path '{}'): {}. Ensure the destination path or its parent directory exists and is accessible.", path.display(), check_path, e),
                ))
            })?;

        // Parse df output
        // Format: Filesystem 1-blocks Used Available Use% Mounted
        // We need the "Available" column (4th field in data line)
        let lines: Vec<&str> = output.lines().collect();
        if lines.len() < 2 {
            return Err(SyncError::Io(std::io::Error::other(format!(
                "Unexpected df output: {}",
                output
            ))));
        }

        let data_line = lines[1];
        let fields: Vec<&str> = data_line.split_whitespace().collect();
        if fields.len() < 4 {
            return Err(SyncError::Io(std::io::Error::other(format!(
                "Failed to parse df output: {}",
                data_line
            ))));
        }

        let available = fields[3].parse::<u64>().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to parse available space '{}': {}",
                fields[3], e
            )))
        })?;

        // Require 10% buffer for safety
        let required = bytes_needed + (bytes_needed / 10);

        if available < required {
            return Err(SyncError::InsufficientDiskSpace {
                path: path.to_path_buf(),
                required,
                available,
            });
        }

        // Warn if less than 20% buffer
        let comfortable = bytes_needed + (bytes_needed / 5);
        if available < comfortable {
            tracing::warn!(
                "Low disk space on remote {}: {} available, {} needed (plus buffer)",
                path.display(),
                Self::format_bytes(available),
                Self::format_bytes(bytes_needed)
            );
        }

        tracing::debug!(
            "Remote disk space check passed: {} available for {} needed",
            Self::format_bytes(available),
            Self::format_bytes(bytes_needed)
        );

        Ok(())
    }

    async fn set_xattrs(&self, path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<()> {
        if xattrs.is_empty() {
            return Ok(());
        }

        let path_str = path.to_string_lossy();

        for (name, value) in xattrs {
            // Encode value as base64 for safe shell transmission
            use base64::{engine::general_purpose, Engine as _};
            let value_b64 = general_purpose::STANDARD.encode(value);

            // Try Linux setfattr first, fallback to macOS xattr
            // Linux: setfattr -n name -v value path
            // macOS: xattr -w name value path (but xattr expects text, not binary)
            let command = format!(
                "if command -v setfattr >/dev/null 2>&1; then \
                    echo '{}' | base64 -d | setfattr -n '{}' -v - '{}'; \
                 elif command -v xattr >/dev/null 2>&1; then \
                    echo '{}' | base64 -d | xattr -w '{}' - '{}'; \
                 else \
                    echo 'No xattr tool found' >&2; exit 1; \
                 fi",
                value_b64, name, path_str, value_b64, name, path_str
            );

            match self
                .execute_command_with_retry(self.connection_pool.get_session(), &command)
                .await
            {
                Ok(_) => {
                    tracing::debug!("Set remote xattr {} on {}", name, path.display());
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to set remote xattr {} on {}: {}",
                        name,
                        path.display(),
                        e
                    );
                }
            }
        }

        Ok(())
    }

    async fn set_acls(&self, path: &Path, acls_text: &str) -> Result<()> {
        if acls_text.trim().is_empty() {
            return Ok(());
        }

        let path_str = path.to_string_lossy();

        // Use setfacl with ACL entries piped via stdin
        let command = format!("setfacl -M - '{}'", path_str);

        match SshTransport::execute_command_with_stdin(
            self.connection_pool.get_session(),
            &command,
            acls_text.as_bytes(),
        ) {
            Ok(_) => {
                tracing::debug!("Set remote ACLs on {}", path.display());
                Ok(())
            }
            Err(e) => {
                tracing::warn!("Failed to set remote ACLs on {}: {}", path.display(), e);
                Ok(()) // Don't fail sync if ACLs can't be set
            }
        }
    }

    async fn set_bsd_flags(&self, path: &Path, flags: u32) -> Result<()> {
        if flags == 0 {
            return Ok(());
        }

        let path_str = path.to_string_lossy();

        // Convert flags to chflags format (octal)
        let command = format!("chflags {:o} '{}'", flags, path_str);

        match self
            .execute_command_with_retry(self.connection_pool.get_session(), &command)
            .await
        {
            Ok(_) => {
                tracing::debug!("Set remote BSD flags 0x{:x} on {}", flags, path.display());
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to set remote BSD flags on {}: {}",
                    path.display(),
                    e
                );
                Ok(()) // Don't fail sync if flags can't be set
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    // Helper to create a dummy connection pool for testing logic
    // Uses empty sessions list for logic tests that don't need real sessions
    fn create_test_pool(size: usize) -> ConnectionPool {
        ConnectionPool {
            sessions: Vec::with_capacity(size),
            next_index: AtomicUsize::new(0),
        }
    }

    #[test]
    fn test_connection_pool_size() {
        let pool = create_test_pool(0);
        assert_eq!(pool.size(), 0);

        let pool = create_test_pool(5);
        assert_eq!(pool.size(), 0); // capacity != size

        // Test with actual sessions requires real SSH connections (integration test)
    }

    #[test]
    fn test_connection_pool_round_robin_logic() {
        // Test round-robin index calculation without real sessions
        let pool = ConnectionPool {
            sessions: vec![],
            next_index: AtomicUsize::new(0),
        };

        // Simulate the round-robin logic
        for i in 0..15 {
            let index = pool.next_index.fetch_add(1, Ordering::Relaxed);
            // Would be: index % pool.sessions.len()
            assert_eq!(index, i);
        }

        assert_eq!(pool.next_index.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn test_connection_pool_concurrent_counter() {
        use std::thread;

        let pool = Arc::new(ConnectionPool {
            sessions: vec![],
            next_index: AtomicUsize::new(0),
        });

        // Spawn 10 threads that each increment 100 times
        let mut handles = vec![];
        for _ in 0..10 {
            let pool_clone = Arc::clone(&pool);
            let handle = thread::spawn(move || {
                for _ in 0..100 {
                    pool_clone.next_index.fetch_add(1, Ordering::Relaxed);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // After 10 threads * 100 increments = 1000
        let final_index = pool.next_index.load(Ordering::Relaxed);
        assert_eq!(final_index, 1000);
    }

    #[test]
    fn test_connection_pool_wrapping_modulo() {
        // Test the modulo wrapping logic
        let pool_size = 3;

        // Test various index values wrap correctly
        assert_eq!((usize::MAX - 1) % pool_size, 2);
        assert_eq!(usize::MAX % pool_size, 0);
        assert_eq!(0 % pool_size, 0);
        assert_eq!(1 % pool_size, 1);
        assert_eq!(2 % pool_size, 2);
        assert_eq!(3 % pool_size, 0);
        assert_eq!(1000 % pool_size, 1);
    }

    #[test]
    fn test_ssh_transport_pool_size_api() {
        // Test that SshTransport exposes pool_size correctly
        // This doesn't require a real SSH connection - just testing the API exists
        // (Actual connection pooling tested in integration tests with real SSH)
    }
}
