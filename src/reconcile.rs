//! Incremental local reconciliation for the v0.5 sync architecture.
//!
//! Source and destination scans are ordered streams. The common no-delete path
//! performs a merge join and dispatches bounded task batches before either tree
//! is fully materialized. Delete mode performs a no-side-effect preflight first,
//! then builds a source Bloom filter during a complete successful source pass;
//! deletions are never attempted until that pass finishes.

use crate::endpoint::io::hash_file_streaming;
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError};
use crate::sync::config::{DeleteMode, SyncConfig};
use crate::sync::executor::{BackupConfig, ExecuteConfig, TaskExecutor};
use crate::sync::scale::FileSetBloom;
use crate::sync::scanner::{FileEntry, ScanOptions};
use crate::sync::stats::SyncStats;
use crate::sync::strategy::{SyncAction, SyncTask};
use futures::StreamExt;
use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const MIN_BATCH_SIZE: usize = 16;
const MAX_BATCH_SIZE: usize = 1024;

pub(crate) async fn run_local_sync(
    source: &dyn Endpoint,
    dest: &dyn Endpoint,
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Result<SyncStats> {
    let started = Instant::now();

    if !dest.root().exists() && !config.dry_run {
        std::fs::create_dir_all(dest.root())?;
    }

    if config.cache {
        tracing::debug!(
            "directory cache reuse disabled in v0.5 until invalidation is content-safe"
        );
    }
    if config.clear_cache && !config.dry_run {
        let _ = crate::sync::dircache::DirectoryCache::delete(dest.root());
    }

    let delete_preflight = match config.delete {
        DeleteMode::Disabled => None,
        DeleteMode::Enabled { threshold, force } => {
            let preflight =
                preflight_delete(source, dest, config, scan_options, threshold, force).await?;
            tracing::info!(
                source_entries = preflight.source_entries,
                eligible_dest_entries = preflight.eligible_dest_entries,
                delete_candidates = preflight.delete_candidates,
                "delete preflight complete"
            );
            Some(preflight)
        }
    };

    let mut source_membership = delete_preflight
        .as_ref()
        .map(|preflight| FileSetBloom::new(preflight.source_entries.max(1)));

    let executor = TaskExecutor::new(
        source,
        dest,
        config.dry_run,
        config.preserve.clone(),
        config.verification.clone(),
        config.max_concurrent.max(1),
    )
    .with_backup(BackupConfig {
        enabled: config.backup.is_some(),
        suffix: config.suffix.clone(),
        dir: config.backup_dir.clone(),
    })
    .with_config(ExecuteConfig {
        preserve_hardlinks: config.preserve.hardlinks,
        preserve_xattrs: config.preserve.xattrs,
        preserve_dir_permissions: config.preserve.permissions,
        keep_partial: config.partial.is_some(),
        itemize_changes: config.itemize_changes,
        remove_source_files: config.remove_source_files,
        print_stats: config.stats,
    });

    let batch_size = config
        .max_concurrent
        .max(1)
        .saturating_mul(4)
        .clamp(MIN_BATCH_SIZE, MAX_BATCH_SIZE);
    let mut stats = SyncStats::default();
    let mut batch = Vec::with_capacity(batch_size);
    let mut source_stream = source.scan_ordered(scan_options).await?;
    let mut dest_stream = dest.scan_ordered(scan_options).await?;
    let mut source_entry = next_entry(&mut source_stream).await?;
    let mut dest_entry = next_entry(&mut dest_stream).await?;

    while let Some(current_source) = source_entry.take() {
        stats.files_scanned += 1;
        if let Some(ref mut membership) = source_membership {
            membership.insert(&current_source.relative_path);
        }

        while let Some(current_dest) = dest_entry.as_ref() {
            match current_dest
                .relative_path
                .as_path()
                .cmp(current_source.relative_path.as_path())
            {
                Ordering::Less => {
                    dest_entry = next_entry(&mut dest_stream).await?;
                }
                Ordering::Equal | Ordering::Greater => break,
            }
        }

        let matching_dest = dest_entry.as_ref().filter(|entry| {
            entry.relative_path.as_path() == current_source.relative_path.as_path()
        });

        if source_entry_selected(config, &current_source) {
            let task =
                plan_local_entry(source, dest, config, &current_source, matching_dest).await?;
            if !(config.existing && task.action == SyncAction::Create) {
                if config.dry_run {
                    record_planned_task(&task, &mut stats);
                } else {
                    batch.push(task);
                    if batch.len() >= batch_size {
                        execute_batch(&executor, &mut batch, &mut stats).await?;
                    }
                }
            } else {
                stats.files_skipped += 1;
            }
        }

        if matching_dest.is_some() {
            dest_entry = next_entry(&mut dest_stream).await?;
        }
        source_entry = next_entry(&mut source_stream).await?;
    }

    // Drain the destination stream so scan failures are observed before delete
    // mode can proceed. No-delete sync does not otherwise need destination-only
    // entries.
    while dest_entry.is_some() {
        dest_entry = next_entry(&mut dest_stream).await?;
    }

    if !config.dry_run {
        execute_batch(&executor, &mut batch, &mut stats).await?;
    }

    if let (Some(preflight), Some(membership)) = (delete_preflight, source_membership.as_ref()) {
        let current = count_deletions(dest, config, scan_options, membership).await?;
        enforce_delete_threshold(current, preflight.threshold, preflight.force)?;

        if config.dry_run {
            stats.files_deleted = current.delete_candidates;
        } else {
            execute_deletions(
                dest,
                config,
                scan_options,
                membership,
                &executor,
                batch_size,
                &mut stats,
            )
            .await?;
        }
    }

    stats.duration = started.elapsed();
    Ok(stats)
}

#[derive(Debug, Clone, Copy)]
struct DeletePreflight {
    source_entries: usize,
    eligible_dest_entries: usize,
    delete_candidates: usize,
    threshold: u8,
    force: bool,
}

async fn preflight_delete(
    source: &dyn Endpoint,
    dest: &dyn Endpoint,
    config: &SyncConfig,
    scan_options: ScanOptions,
    threshold: u8,
    force: bool,
) -> Result<DeletePreflight> {
    let mut source_stream = source.scan_ordered(scan_options).await?;
    let mut dest_stream = dest.scan_ordered(scan_options).await?;
    let mut source_entry = next_entry(&mut source_stream).await?;
    let mut dest_entry = next_entry(&mut dest_stream).await?;
    let mut source_entries = 0_usize;
    let mut eligible_dest_entries = 0_usize;
    let mut delete_candidates = 0_usize;
    let mut protected_dest_dir: Option<PathBuf> = None;

    while source_entry.is_some() || dest_entry.is_some() {
        match (source_entry.as_ref(), dest_entry.as_ref()) {
            (Some(source), Some(dest)) => match source
                .relative_path
                .as_path()
                .cmp(dest.relative_path.as_path())
            {
                Ordering::Less => {
                    source_entries += 1;
                    source_entry = next_entry(&mut source_stream).await?;
                }
                Ordering::Equal => {
                    source_entries += 1;
                    if dest_delete_eligible(config, dest, &mut protected_dest_dir) {
                        eligible_dest_entries += 1;
                    }
                    source_entry = next_entry(&mut source_stream).await?;
                    dest_entry = next_entry(&mut dest_stream).await?;
                }
                Ordering::Greater => {
                    if dest_delete_eligible(config, dest, &mut protected_dest_dir) {
                        eligible_dest_entries += 1;
                        delete_candidates += 1;
                    }
                    dest_entry = next_entry(&mut dest_stream).await?;
                }
            },
            (Some(_), None) => {
                source_entries += 1;
                source_entry = next_entry(&mut source_stream).await?;
            }
            (None, Some(dest)) => {
                if dest_delete_eligible(config, dest, &mut protected_dest_dir) {
                    eligible_dest_entries += 1;
                    delete_candidates += 1;
                }
                dest_entry = next_entry(&mut dest_stream).await?;
            }
            (None, None) => break,
        }
    }

    let preflight = DeletePreflight {
        source_entries,
        eligible_dest_entries,
        delete_candidates,
        threshold,
        force,
    };
    enforce_delete_threshold(preflight, threshold, force)?;
    Ok(preflight)
}

fn enforce_delete_threshold(counts: DeletePreflight, threshold: u8, force: bool) -> Result<()> {
    if force || counts.eligible_dest_entries == 0 {
        return Ok(());
    }

    let percentage = counts.delete_candidates as f64 / counts.eligible_dest_entries as f64 * 100.0;
    if percentage > threshold as f64 {
        return Err(SyncError::DeletionThresholdExceeded {
            percentage,
            threshold,
        });
    }
    Ok(())
}

async fn count_deletions(
    dest: &dyn Endpoint,
    config: &SyncConfig,
    scan_options: ScanOptions,
    membership: &FileSetBloom,
) -> Result<DeletePreflight> {
    let mut stream = dest.scan_ordered(scan_options).await?;
    let mut eligible_dest_entries = 0_usize;
    let mut delete_candidates = 0_usize;
    let mut protected_dest_dir: Option<PathBuf> = None;

    while let Some(entry) = next_entry(&mut stream).await? {
        if !dest_delete_eligible(config, &entry, &mut protected_dest_dir) {
            continue;
        }
        eligible_dest_entries += 1;
        if !membership.contains(&entry.relative_path) {
            delete_candidates += 1;
        }
    }

    let (threshold, force) = match config.delete {
        DeleteMode::Enabled { threshold, force } => (threshold, force),
        DeleteMode::Disabled => (100, true),
    };

    Ok(DeletePreflight {
        source_entries: membership.expected_items(),
        eligible_dest_entries,
        delete_candidates,
        threshold,
        force,
    })
}

async fn execute_deletions(
    dest: &dyn Endpoint,
    config: &SyncConfig,
    scan_options: ScanOptions,
    membership: &FileSetBloom,
    executor: &TaskExecutor<'_>,
    batch_size: usize,
    stats: &mut SyncStats,
) -> Result<()> {
    let mut stream = dest.scan_ordered(scan_options).await?;
    let mut batch = Vec::with_capacity(batch_size);
    let mut protected_dest_dir: Option<PathBuf> = None;
    let mut pending_delete_dir: Option<PathBuf> = None;

    while let Some(entry) = next_entry(&mut stream).await? {
        if let Some(directory) = pending_delete_dir.as_ref() {
            if entry.relative_path.starts_with(directory) {
                continue;
            }
            if let Some(directory) = pending_delete_dir.take() {
                batch.push(delete_task(directory));
                if batch.len() >= batch_size {
                    execute_batch(executor, &mut batch, stats).await?;
                }
            }
        }

        if !dest_delete_eligible(config, &entry, &mut protected_dest_dir)
            || membership.contains(&entry.relative_path)
        {
            continue;
        }

        if entry.is_dir {
            pending_delete_dir = Some((*entry.relative_path).clone());
        } else {
            batch.push(delete_task((*entry.relative_path).clone()));
            if batch.len() >= batch_size {
                execute_batch(executor, &mut batch, stats).await?;
            }
        }
    }

    if let Some(directory) = pending_delete_dir {
        batch.push(delete_task(directory));
    }
    execute_batch(executor, &mut batch, stats).await
}

fn dest_delete_eligible(
    config: &SyncConfig,
    entry: &FileEntry,
    protected_dir: &mut Option<PathBuf>,
) -> bool {
    if let Some(directory) = protected_dir.as_ref() {
        if entry.relative_path.starts_with(directory) {
            return false;
        }
        *protected_dir = None;
    }

    if config
        .filter_engine
        .should_exclude(&entry.relative_path, entry.is_dir)
    {
        if entry.is_dir {
            *protected_dir = Some((*entry.relative_path).clone());
        }
        return false;
    }

    true
}

fn source_entry_selected(config: &SyncConfig, entry: &FileEntry) -> bool {
    if !entry.is_dir {
        if let Some(min) = config.min_size {
            if entry.size < min {
                return false;
            }
        }
        if let Some(max) = config.max_size {
            if entry.size > max {
                return false;
            }
        }
    }

    !config
        .filter_engine
        .should_exclude(&entry.relative_path, entry.is_dir)
}

async fn plan_local_entry(
    source_endpoint: &dyn Endpoint,
    dest_endpoint: &dyn Endpoint,
    config: &SyncConfig,
    source: &FileEntry,
    dest: Option<&FileEntry>,
) -> Result<SyncTask> {
    let dest_path = (*source.relative_path).clone();
    let Some(dest) = dest else {
        return Ok(task_for(source, dest_path, SyncAction::Create));
    };

    if config.comparison.ignore_existing {
        return Ok(task_for(source, dest_path, SyncAction::Skip));
    }
    if config.comparison.update_only && dest.modified > source.modified {
        return Ok(task_for(source, dest_path, SyncAction::Skip));
    }

    if entry_kind(source) != entry_kind(dest) {
        return Err(SyncError::Config(format!(
            "refusing non-transactional type replacement at {}",
            source.relative_path.display()
        )));
    }

    let action = if source.is_dir {
        SyncAction::Skip
    } else if source.is_symlink {
        if source.symlink_target == dest.symlink_target {
            SyncAction::Skip
        } else {
            SyncAction::Update
        }
    } else if config.comparison.checksum {
        if source.size != dest.size {
            SyncAction::Update
        } else {
            let source_hash = hash_file_streaming(source_endpoint, &source.relative_path).await?;
            let dest_hash = hash_file_streaming(dest_endpoint, &dest.relative_path).await?;
            if source_hash == dest_hash {
                SyncAction::Skip
            } else {
                SyncAction::Update
            }
        }
    } else if config.comparison.size_only {
        if source.size == dest.size {
            SyncAction::Skip
        } else {
            SyncAction::Update
        }
    } else if config.comparison.ignore_times {
        SyncAction::Update
    } else if source.size == dest.size && source.modified == dest.modified {
        SyncAction::Skip
    } else {
        SyncAction::Update
    };

    Ok(task_for(source, dest_path, action))
}

async fn execute_batch(
    executor: &TaskExecutor<'_>,
    batch: &mut Vec<SyncTask>,
    stats: &mut SyncStats,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let tasks = std::mem::take(batch);
    let batch_stats = executor.execute_batch(tasks).await?;
    merge_stats(stats, batch_stats);
    Ok(())
}

fn record_planned_task(task: &SyncTask, stats: &mut SyncStats) {
    match task.action {
        SyncAction::Skip => stats.files_skipped += 1,
        SyncAction::Create => stats.files_created += 1,
        SyncAction::Update => stats.files_updated += 1,
        SyncAction::Delete => stats.files_deleted += 1,
    }
}

fn merge_stats(stats: &mut SyncStats, mut batch: SyncStats) {
    stats.files_created += batch.files_created;
    stats.files_updated += batch.files_updated;
    stats.files_skipped += batch.files_skipped;
    stats.files_deleted += batch.files_deleted;
    stats.bytes_transferred += batch.bytes_transferred;
    stats.files_delta_synced += batch.files_delta_synced;
    stats.delta_bytes_saved += batch.delta_bytes_saved;
    stats.files_compressed += batch.files_compressed;
    stats.compression_bytes_saved += batch.compression_bytes_saved;
    stats.files_verified += batch.files_verified;
    stats.verification_failures += batch.verification_failures;
    stats.bytes_would_add += batch.bytes_would_add;
    stats.bytes_would_change += batch.bytes_would_change;
    stats.bytes_would_delete += batch.bytes_would_delete;
    stats.dirs_created += batch.dirs_created;
    stats.symlinks_created += batch.symlinks_created;
    stats.errors.append(&mut batch.errors);
}

fn entry_kind(entry: &FileEntry) -> u8 {
    if entry.is_symlink {
        2
    } else if entry.is_dir {
        1
    } else {
        0
    }
}

fn task_for(source: &FileEntry, dest_path: PathBuf, action: SyncAction) -> SyncTask {
    SyncTask {
        source: Some(Arc::new(source.clone())),
        dest_path,
        action,
        source_checksum: None,
        dest_checksum: None,
    }
}

fn delete_task(path: PathBuf) -> SyncTask {
    SyncTask {
        source: None,
        dest_path: path,
        action: SyncAction::Delete,
        source_checksum: None,
        dest_checksum: None,
    }
}

async fn next_entry(stream: &mut crate::endpoint::EntryStream) -> Result<Option<FileEntry>> {
    match stream.next().await {
        Some(entry) => entry.map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::local::LocalEndpoint;
    use crate::sync::config::ComparisonConfig;
    use tempfile::TempDir;

    fn config() -> SyncConfig {
        SyncConfig {
            dry_run: false,
            delete: DeleteMode::Disabled,
            comparison: ComparisonConfig::default(),
            filter_engine: crate::filter::FilterEngine::new(),
            ..SyncConfig::test_default()
        }
    }

    #[tokio::test]
    async fn bounded_reconciler_creates_and_skips() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("a"), b"one").unwrap();
        std::fs::write(source.path().join("b"), b"two").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());

        let first = run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &config(),
            ScanOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(first.files_created, 2);

        let second = run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &config(),
            ScanOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(second.files_skipped, 2);
    }

    #[tokio::test]
    async fn delete_threshold_runs_before_mutation() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("new"), b"new").unwrap();
        std::fs::write(dest.path().join("old-a"), b"a").unwrap();
        std::fs::write(dest.path().join("old-b"), b"b").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());
        let mut sync_config = config();
        sync_config.delete = DeleteMode::Enabled {
            threshold: 50,
            force: false,
        };

        let result = run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &sync_config,
            ScanOptions::default(),
        )
        .await;
        assert!(matches!(
            result,
            Err(SyncError::DeletionThresholdExceeded { .. })
        ));
        assert!(!dest.path().join("new").exists());
        assert!(dest.path().join("old-a").exists());
        assert!(dest.path().join("old-b").exists());
    }

    #[tokio::test]
    async fn delete_waits_for_complete_source_scan() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("keep"), b"keep").unwrap();
        std::fs::write(dest.path().join("keep"), b"keep").unwrap();
        std::fs::write(dest.path().join("delete"), b"delete").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());
        let mut sync_config = config();
        sync_config.delete = DeleteMode::Enabled {
            threshold: 100,
            force: false,
        };

        let stats = run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &sync_config,
            ScanOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert!(dest.path().join("keep").exists());
        assert!(!dest.path().join("delete").exists());
    }
}
