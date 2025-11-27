use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::compress::{compress, is_compressed_extension, Compression};
use crate::delta::{generate_delta_streaming, BlockChecksum as DeltaBlockChecksum};
use crate::path::SyncPath;
use crate::server::protocol::{
    Action, DeltaOp, FileListEntry, SymlinkEntry, DATA_FLAG_COMPRESSED, DELTA_BLOCK_SIZE,
    DELTA_MIN_SIZE,
};
use crate::ssh::config::SshConfig;
use crate::sync::scanner::{self, ScanOptions};
use crate::sync::SyncStats;
use crate::transport::server::ServerSession;

/// Minimum size for compression (1MB)
const COMPRESS_MIN_SIZE: u64 = 1024 * 1024;

/// Source entry with all info needed for transfer
struct SourceEntry {
    rel_path: String,
    abs_path: Arc<PathBuf>,
    size: u64,
    mtime: i64,
    mode: u32,
    is_dir: bool,
    is_symlink: bool,
    symlink_target: Option<String>,
}

/// Sync from local source to remote destination using server protocol
pub async fn sync_server_mode(source: &Path, dest: &SyncPath) -> Result<SyncStats> {
    let start = Instant::now();

    // Connect to server
    let mut session = connect(dest).await?;
    tracing::debug!("Connected to server");

    // Scan source
    tracing::debug!("Scanning source...");
    let source_entries = scan_source(source).await?;

    // Separate entries by type
    let mut directories: Vec<String> = Vec::new();
    let mut files: Vec<SourceEntry> = Vec::new();
    let mut symlinks: Vec<SourceEntry> = Vec::new();

    for entry in source_entries {
        if entry.is_dir {
            directories.push(entry.rel_path);
        } else if entry.is_symlink {
            symlinks.push(entry);
        } else {
            files.push(entry);
        }
    }

    // Build protocol entries (files only for FILE_LIST comparison)
    let proto_entries: Vec<FileListEntry> = files
        .iter()
        .map(|e| FileListEntry {
            path: e.rel_path.clone(),
            size: e.size,
            mtime: e.mtime,
            mode: e.mode,
            flags: 0,
            symlink_target: None,
        })
        .collect();

    let total_files = proto_entries.len();
    let total_dirs = directories.len();
    let total_symlinks = symlinks.len();

    tracing::info!(
        "Source: {} files, {} dirs, {} symlinks",
        total_files,
        total_dirs,
        total_symlinks
    );

    // Step 1: Create directories (if any)
    let mut dirs_created = 0u64;
    if !directories.is_empty() {
        tracing::debug!("Creating {} directories...", directories.len());
        session.send_mkdir_batch(directories).await?;
        let ack = session.read_mkdir_ack().await?;
        dirs_created = ack.created as u64;
        if !ack.failed.is_empty() {
            for (path, err) in &ack.failed {
                tracing::warn!("Failed to create dir {}: {}", path, err);
            }
        }
    }

    // Step 2: Send file list and get decisions
    tracing::debug!("Sending file list ({} files)...", total_files);
    session.send_file_list(proto_entries).await?;

    tracing::debug!("Waiting for server decisions...");
    let ack = session.read_ack().await?;

    // Count files needing transfer
    let files_to_transfer = ack
        .decisions
        .iter()
        .filter(|d| matches!(d.action, Action::Create | Action::Update))
        .count();
    tracing::info!("{} files need transfer", files_to_transfer);

    // Step 3: Transfer files - separate creates and updates
    let mut bytes_transferred = 0u64;
    let mut files_created = 0u64;
    let mut files_updated = 0u64;

    // Categorize by action type
    let creates: Vec<(u32, &SourceEntry)> = ack
        .decisions
        .iter()
        .filter_map(|d| {
            if d.action == Action::Create {
                Some((d.index, &files[d.index as usize]))
            } else {
                None
            }
        })
        .collect();

    let updates: Vec<(u32, &SourceEntry)> = ack
        .decisions
        .iter()
        .filter_map(|d| {
            if d.action == Action::Update {
                Some((d.index, &files[d.index as usize]))
            } else {
                None
            }
        })
        .collect();

    // Step 3a: Handle CREATES with full file transfer (+ compression)
    if !creates.is_empty() {
        tracing::debug!("Transferring {} new files...", creates.len());

        let paths: Vec<(u32, Arc<PathBuf>, String, u64)> = creates
            .iter()
            .map(|(idx, e)| (*idx, e.abs_path.clone(), e.rel_path.clone(), e.size))
            .collect();

        // Read and optionally compress files
        let files_data: Vec<(u32, Vec<u8>, u8)> = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .filter_map(|(idx, path, rel_path, size)| {
                    std::fs::read(&*path).ok().map(|data| {
                        // Compress if file is large enough and not already compressed
                        let (send_data, flags) =
                            if size >= COMPRESS_MIN_SIZE && !is_compressed_extension(&rel_path) {
                                match compress(&data, Compression::Zstd) {
                                    Ok(compressed) if compressed.len() < data.len() => {
                                        (compressed, DATA_FLAG_COMPRESSED)
                                    }
                                    _ => (data, 0),
                                }
                            } else {
                                (data, 0)
                            };
                        (idx, send_data, flags)
                    })
                })
                .collect()
        })
        .await?;

        // Send all creates
        for (idx, data, flags) in &files_data {
            bytes_transferred += data.len() as u64;
            session
                .send_file_data_with_flags(*idx, 0, *flags, data.clone())
                .await?;
        }
        session.flush().await?;

        // Read confirmations
        for _ in &files_data {
            let done = session.read_file_done().await?;
            if done.status != 0 {
                tracing::error!("Create failed: index {} status {}", done.index, done.status);
            } else {
                files_created += 1;
            }
        }
    }

    // Step 3b: Handle UPDATES - use delta sync for large files
    if !updates.is_empty() {
        let (delta_candidates, full_updates): (Vec<_>, Vec<_>) =
            updates.iter().partition(|(_, e)| e.size >= DELTA_MIN_SIZE);

        // Process delta candidates one by one (need checksums from server)
        if !delta_candidates.is_empty() {
            tracing::debug!(
                "Delta syncing {} large files (>{}KB)...",
                delta_candidates.len(),
                DELTA_MIN_SIZE / 1024
            );

            for (idx, entry) in &delta_candidates {
                let path = entry.abs_path.clone();
                let size = entry.size;
                let file_idx = *idx;

                let t0 = Instant::now();

                // Request checksums from server
                session
                    .send_checksum_req(file_idx, DELTA_BLOCK_SIZE as u32)
                    .await?;
                let resp = session.read_checksum_resp().await?;

                let t1 = Instant::now();
                tracing::debug!(
                    "  Checksum request/response: {:?} ({} blocks)",
                    t1.duration_since(t0),
                    resp.checksums.len()
                );

                // Convert protocol checksums to delta checksums
                let dest_checksums: Vec<DeltaBlockChecksum> = resp
                    .checksums
                    .iter()
                    .enumerate()
                    .map(|(i, c)| DeltaBlockChecksum {
                        index: i as u64,
                        offset: c.offset,
                        size: c.size as usize,
                        weak: c.weak,
                        strong: c.strong,
                    })
                    .collect();

                // Generate delta in blocking task
                let block_size = DELTA_BLOCK_SIZE;
                let delta = tokio::task::spawn_blocking(move || {
                    generate_delta_streaming(&path, &dest_checksums, block_size)
                })
                .await??;

                let t2 = Instant::now();
                tracing::debug!(
                    "  Delta generation: {:?} ({} ops)",
                    t2.duration_since(t1),
                    delta.ops.len()
                );

                // Convert to protocol delta ops
                // Note: We don't compress delta literals because they're usually small
                // and the overhead of per-chunk compression isn't worth it
                let mut ops: Vec<DeltaOp> = Vec::with_capacity(delta.ops.len());
                let mut delta_bytes = 0u64;

                for op in &delta.ops {
                    match op {
                        crate::delta::DeltaOp::Copy { offset, size } => {
                            ops.push(DeltaOp::Copy {
                                offset: *offset,
                                size: *size as u32,
                            });
                        }
                        crate::delta::DeltaOp::Data(data) => {
                            delta_bytes += data.len() as u64;
                            ops.push(DeltaOp::Data(data.clone()));
                        }
                    }
                }

                bytes_transferred += delta_bytes;

                // Send delta (no compression for delta ops - literals are small)
                session.send_delta_data(file_idx, 0, ops).await?;

                let t3 = Instant::now();
                tracing::debug!("  Delta send: {:?}", t3.duration_since(t2));

                let done = session.read_file_done().await?;

                let t4 = Instant::now();
                tracing::debug!("  Delta apply (server): {:?}", t4.duration_since(t3));
                if done.status != 0 {
                    tracing::error!(
                        "Delta update failed for {}: index {} status {}",
                        entry.rel_path,
                        done.index,
                        done.status
                    );
                } else {
                    files_updated += 1;
                    tracing::debug!(
                        "Delta sync {}: sent {}KB (file is {}KB)",
                        entry.rel_path,
                        delta_bytes / 1024,
                        size / 1024
                    );
                }
            }
        }

        // Process small file updates with full transfer
        if !full_updates.is_empty() {
            tracing::debug!(
                "Full transfer for {} small file updates...",
                full_updates.len()
            );

            let paths: Vec<(u32, Arc<PathBuf>, String, u64)> = full_updates
                .iter()
                .map(|(idx, e)| (*idx, e.abs_path.clone(), e.rel_path.clone(), e.size))
                .collect();

            let files_data: Vec<(u32, Vec<u8>, u8)> = tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .filter_map(|(idx, path, rel_path, size)| {
                        std::fs::read(&*path).ok().map(|data| {
                            let (send_data, flags) = if size >= COMPRESS_MIN_SIZE
                                && !is_compressed_extension(&rel_path)
                            {
                                match compress(&data, Compression::Zstd) {
                                    Ok(compressed) if compressed.len() < data.len() => {
                                        (compressed, DATA_FLAG_COMPRESSED)
                                    }
                                    _ => (data, 0),
                                }
                            } else {
                                (data, 0)
                            };
                            (idx, send_data, flags)
                        })
                    })
                    .collect()
            })
            .await?;

            for (idx, data, flags) in &files_data {
                bytes_transferred += data.len() as u64;
                session
                    .send_file_data_with_flags(*idx, 0, *flags, data.clone())
                    .await?;
            }
            session.flush().await?;

            for _ in &files_data {
                let done = session.read_file_done().await?;
                if done.status != 0 {
                    tracing::error!("Update failed: index {} status {}", done.index, done.status);
                } else {
                    files_updated += 1;
                }
            }
        }
    }

    // Step 4: Create symlinks (if any)
    let mut symlinks_created = 0u64;
    if !symlinks.is_empty() {
        tracing::debug!("Creating {} symlinks...", symlinks.len());

        let symlink_entries: Vec<SymlinkEntry> = symlinks
            .iter()
            .filter_map(|e| {
                e.symlink_target.as_ref().map(|target| SymlinkEntry {
                    path: e.rel_path.clone(),
                    target: target.clone(),
                })
            })
            .collect();

        if !symlink_entries.is_empty() {
            session.send_symlink_batch(symlink_entries).await?;
            let ack = session.read_symlink_ack().await?;
            symlinks_created = ack.created as u64;
            if !ack.failed.is_empty() {
                for (path, err) in &ack.failed {
                    tracing::warn!("Failed to create symlink {}: {}", path, err);
                }
            }
        }
    }

    let duration = start.elapsed();
    tracing::info!(
        "Server sync complete: {} files ({} created, {} updated), {} dirs, {} symlinks in {:?}",
        files_to_transfer,
        files_created,
        files_updated,
        dirs_created,
        symlinks_created,
        duration
    );

    Ok(SyncStats {
        files_scanned: total_files as u64,
        files_created,
        files_updated,
        files_deleted: 0,
        files_skipped: (total_files - files_to_transfer) as usize,
        bytes_transferred,
        duration,
        errors: Vec::new(),
        dirs_created,
        symlinks_created,
        ..Default::default()
    })
}

/// Connect to remote server
async fn connect(dest: &SyncPath) -> Result<ServerSession> {
    match dest {
        SyncPath::Local { path, .. } => ServerSession::connect_local(path).await,
        SyncPath::Remote {
            host, user, path, ..
        } => {
            let mut config = SshConfig::default();
            config.hostname = host.clone();
            if let Some(u) = user {
                config.user = u.clone();
            }
            ServerSession::connect_ssh(&config, path).await
        }
        _ => anyhow::bail!("Unsupported destination for server mode"),
    }
}

/// Scan source directory and return entries
async fn scan_source(source: &Path) -> Result<Vec<SourceEntry>> {
    let scan_opts = ScanOptions::default();
    let src = source.to_path_buf();

    let entries = tokio::task::spawn_blocking(move || {
        scanner::Scanner::new(&src).with_options(scan_opts).scan()
    })
    .await??;

    let mut result = Vec::with_capacity(entries.len());

    for entry in entries {
        if let Ok(rel_path) = entry.path.strip_prefix(source) {
            // Skip root directory
            if rel_path.as_os_str().is_empty() {
                continue;
            }

            if let Some(path_str) = rel_path.to_str() {
                let mtime = entry
                    .modified
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;

                let symlink_target = entry
                    .symlink_target
                    .as_ref()
                    .and_then(|t| t.to_str().map(String::from));

                result.push(SourceEntry {
                    rel_path: path_str.to_string(),
                    abs_path: entry.path.clone(),
                    size: entry.size,
                    mtime,
                    mode: 0o644, // TODO: Get real mode from entry
                    is_dir: entry.is_dir,
                    is_symlink: entry.is_symlink,
                    symlink_target,
                });
            }
        }
    }

    Ok(result)
}
