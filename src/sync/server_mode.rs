use anyhow::Result;
use std::path::Path;
use std::time::Instant;

use crate::path::SyncPath;
use crate::ssh::config::SshConfig;
use crate::streaming::StreamingSync;
use crate::sync::SyncStats;

/// Sync from local source to remote destination
pub async fn sync_push(
    source: &Path,
    dest: &SyncPath,
    delete: bool,
    compress: bool,
) -> Result<SyncStats> {
    let config = if let SyncPath::Remote { host, user, .. } = dest {
        if let Some(user) = user {
            SshConfig {
                hostname: host.clone(),
                user: user.clone(),
                ..Default::default()
            }
        } else {
            crate::ssh::config::parse_ssh_config(host)?
        }
    } else {
        anyhow::bail!("Destination must be remote for sync_push");
    };

    let session =
        crate::transport::server::ServerSession::connect_ssh(&config, dest.path()).await?;
    let (mut stdin, mut stdout) = session.split();

    let sync = StreamingSync::new(
        source.to_path_buf(),
        dest.path().to_path_buf(),
        delete,
        compress,
    );

    let stats = sync.push(&mut stdout, &mut stdin).await?;

    Ok(SyncStats {
        files_scanned: stats.files_ok,
        files_created: stats.files_ok,
        files_updated: 0,
        files_deleted: stats.deleted as usize,
        files_skipped: 0,
        bytes_transferred: stats.bytes_transferred,
        files_delta_synced: stats.delta_files as usize,
        delta_bytes_saved: stats.delta_bytes_saved,
        files_compressed: 0,
        compression_bytes_saved: 0,
        files_verified: 0,
        verification_failures: 0,
        duration: Instant::now().elapsed(),
        bytes_would_add: 0,
        bytes_would_change: 0,
        bytes_would_delete: 0,
        dirs_created: stats.dirs_created,
        symlinks_created: stats.symlinks_created,
        errors: vec![],
    })
}

/// Sync from remote source to local destination
pub async fn sync_pull(
    source: &SyncPath,
    dest: &Path,
    delete: bool,
    compress: bool,
) -> Result<SyncStats> {
    let config = if let SyncPath::Remote { host, user, .. } = source {
        if let Some(user) = user {
            SshConfig {
                hostname: host.clone(),
                user: user.clone(),
                ..Default::default()
            }
        } else {
            crate::ssh::config::parse_ssh_config(host)?
        }
    } else {
        anyhow::bail!("Source must be remote for sync_pull");
    };

    let session =
        crate::transport::server::ServerSession::connect_ssh(&config, source.path()).await?;
    let (mut stdin, mut stdout) = session.split();

    let sync = StreamingSync::new(
        dest.to_path_buf(),
        source.path().to_path_buf(),
        delete,
        compress,
    );

    let stats = sync.pull(&mut stdout, &mut stdin).await?;

    Ok(SyncStats {
        files_scanned: stats.files_ok,
        files_created: stats.files_ok,
        files_updated: 0,
        files_deleted: stats.deleted as usize,
        files_skipped: 0,
        bytes_transferred: stats.bytes_transferred,
        files_delta_synced: stats.delta_files as usize,
        delta_bytes_saved: stats.delta_bytes_saved,
        files_compressed: 0,
        compression_bytes_saved: 0,
        files_verified: 0,
        verification_failures: 0,
        duration: Instant::now().elapsed(),
        bytes_would_add: 0,
        bytes_would_change: 0,
        bytes_would_delete: 0,
        dirs_created: stats.dirs_created,
        symlinks_created: stats.symlinks_created,
        errors: vec![],
    })
}
