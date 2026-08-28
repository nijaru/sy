//! Ordered reconciliation for the v0.5 local path.
//!
//! This is the last compatibility location for the reconciler before it moves
//! wholly under `engine/`. Source and destination scans are merge-joined in
//! bounded memory. Delete mode performs a complete no-mutation preflight and
//! records exact destination-only paths in an on-disk reverse journal.

#[path = "../engine/delete_journal.rs"]
mod delete_journal;

use crate::endpoint::io::hash_file_streaming;
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError};
use crate::sync::config::{DeleteMode, SyncConfig};
use crate::sync::executor::{BackupConfig, ExecuteConfig, TaskExecutor};
use crate::sync::scanner::{FileEntry, ScanOptions};
use crate::sync::stats::SyncStats;
use crate::sync::strategy::{SyncAction, SyncTask};
use delete_journal::{DeleteJournal, DeleteJournalReader, DeleteKind};
use futures::StreamExt;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
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

    let delete_plan = match config.delete {
        DeleteMode::Disabled => None,
        DeleteMode::Enabled { threshold, force } => {
            let plan =
                preflight_delete(source, dest, config, scan_options, threshold, force).await?;
            tracing::info!(
                source_entries = plan.source_entries,
                eligible_dest_entries = plan.eligible_dest_entries,
                delete_candidates = plan.delete_candidates,
                "delete preflight complete"
            );
            Some(plan)
        }
    };

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

    // Observe destination scan failures before deletion becomes possible. The
    // no-delete path does not otherwise need destination-only entries here.
    while dest_entry.is_some() {
        dest_entry = next_entry(&mut dest_stream).await?;
    }

    if !config.dry_run {
        execute_batch(&executor, &mut batch, &mut stats).await?;
    }

    if let Some(mut plan) = delete_plan {
        if config.dry_run {
            stats.files_deleted = plan.delete_candidates;
        } else {
            execute_delete_journal(dest, &mut plan.journal, &mut stats).await?;
        }
    }

    if config.cache && !config.dry_run {
        // Preserve the user-visible cache-file contract without restoring the
        // unsafe root-mtime shortcut from 0.4.
        crate::sync::dircache::DirectoryCache::new().save(dest.root())?;
    }

    stats.duration = started.elapsed();
    Ok(stats)
}

struct DeletePlan {
    source_entries: usize,
    eligible_dest_entries: usize,
    delete_candidates: usize,
    journal: DeleteJournalReader,
}

#[derive(Debug)]
struct CandidateDeleteDir {
    path: PathBuf,
    protected: bool,
}

async fn preflight_delete(
    source: &dyn Endpoint,
    dest: &dyn Endpoint,
    config: &SyncConfig,
    scan_options: ScanOptions,
    threshold: u8,
    force: bool,
) -> Result<DeletePlan> {
    let mut source_stream = source.scan_ordered(scan_options).await?;
    let mut dest_stream = dest.scan_ordered(scan_options).await?;
    let mut source_entry = next_entry(&mut source_stream).await?;
    let mut dest_entry = next_entry(&mut dest_stream).await?;
    let mut source_entries = 0_usize;
    let mut eligible_dest_entries = 0_usize;
    let mut delete_candidates = 0_usize;
    let mut protected_dest_dir: Option<PathBuf> = None;
    let mut candidate_dirs = Vec::<CandidateDeleteDir>::new();
    let mut journal = DeleteJournal::new().await?;

    while source_entry.is_some() || dest_entry.is_some() {
        match (source_entry.as_ref(), dest_entry.as_ref()) {
            (Some(source_entry_ref), Some(dest_entry_ref)) => match source_entry_ref
                .relative_path
                .as_path()
                .cmp(dest_entry_ref.relative_path.as_path())
            {
                Ordering::Less => {
                    source_entries += 1;
                    source_entry = next_entry(&mut source_stream).await?;
                }
                Ordering::Equal => {
                    close_candidate_dirs(dest_entry_ref.relative_path.as_path(), &mut candidate_dirs);
                    source_entries += 1;
                    if dest_delete_eligible(config, dest_entry_ref, &mut protected_dest_dir) {
                        eligible_dest_entries += 1;
                    }
                    // Any source-backed entry inside a destination-only candidate
                    // directory prevents that ancestor from being removed.
                    protect_candidate_dirs(&mut journal, &mut candidate_dirs).await?;
                    source_entry = next_entry(&mut source_stream).await?;
                    dest_entry = next_entry(&mut dest_stream).await?;
                }
                Ordering::Greater => {
                    close_candidate_dirs(dest_entry_ref.relative_path.as_path(), &mut candidate_dirs);
                    if dest_delete_eligible(config, dest_entry_ref, &mut protected_dest_dir) {
                        eligible_dest_entries += 1;
                        delete_candidates += 1;
                        append_delete_candidate(
                            &mut journal,
                            dest_entry_ref,
                            &mut candidate_dirs,
                        )
                        .await?;
                    } else {
                        protect_candidate_dirs(&mut journal, &mut candidate_dirs).await?;
                    }
                    dest_entry = next_entry(&mut dest_stream).await?;
                }
            },
            (Some(_), None) => {
                source_entries += 1;
                source_entry = next_entry(&mut source_stream).await?;
            }
            (None, Some(dest_entry_ref)) => {
                close_candidate_dirs(dest_entry_ref.relative_path.as_path(), &mut candidate_dirs);
                if dest_delete_eligible(config, dest_entry_ref, &mut protected_dest_dir) {
                    eligible_dest_entries += 1;
                    delete_candidates += 1;
                    append_delete_candidate(&mut journal, dest_entry_ref, &mut candidate_dirs)
                        .await?;
                } else {
                    protect_candidate_dirs(&mut journal, &mut candidate_dirs).await?;
                }
                dest_entry = next_entry(&mut dest_stream).await?;
            }
            (None, None) => break,
        }
    }

    enforce_delete_threshold(eligible_dest_entries, delete_candidates, threshold, force)?;
    let journal = journal.seal().await?;
    Ok(DeletePlan {
        source_entries,
        eligible_dest_entries,
        delete_candidates,
        journal,
    })
}

fn close_candidate_dirs(current: &Path, candidate_dirs: &mut Vec<CandidateDeleteDir>) {
    while candidate_dirs
        .last()
        .is_some_and(|candidate| !current.starts_with(&candidate.path))
    {
        candidate_dirs.pop();
    }
}

async fn append_delete_candidate(
    journal: &mut DeleteJournal,
    entry: &FileEntry,
    candidate_dirs: &mut Vec<CandidateDeleteDir>,
) -> Result<()> {
    if entry.is_dir {
        journal
            .append(&entry.relative_path, DeleteKind::Directory)
            .await?;
        candidate_dirs.push(CandidateDeleteDir {
            path: (*entry.relative_path).clone(),
            protected: false,
        });
    } else {
        journal
            .append(&entry.relative_path, DeleteKind::FileLike)
            .await?;
    }
    Ok(())
}

async fn protect_candidate_dirs(
    journal: &mut DeleteJournal,
    candidate_dirs: &mut [CandidateDeleteDir],
) -> Result<()> {
    for candidate in candidate_dirs {
        if !candidate.protected {
            journal
                .append(&candidate.path, DeleteKind::ProtectDirectory)
                .await?;
            candidate.protected = true;
        }
    }
    Ok(())
}

fn enforce_delete_threshold(
    eligible_dest_entries: usize,
    delete_candidates: usize,
    threshold: u8,
    force: bool,
) -> Result<()> {
    if force || eligible_dest_entries == 0 {
        return Ok(());
    }

    let percentage = delete_candidates as f64 / eligible_dest_entries as f64 * 100.0;
    if percentage > threshold as f64 {
        return Err(SyncError::DeletionThresholdExceeded {
            percentage,
            threshold,
        });
    }
    Ok(())
}

async fn execute_delete_journal(
    dest: &dyn Endpoint,
    journal: &mut DeleteJournalReader,
    stats: &mut SyncStats,
) -> Result<()> {
    // Protection records live only until their corresponding earlier directory
    // candidate is reached during reverse replay. The active set is therefore
    // proportional to directory nesting, not total tree size.
    let mut protected_dirs = Vec::<PathBuf>::new();

    while let Some(record) = journal.next().await? {
        match record.kind {
            DeleteKind::ProtectDirectory => {
                if !protected_dirs.iter().any(|path| path == &record.path) {
                    protected_dirs.push(record.path);
                }
            }
            DeleteKind::Directory => {
                if let Some(index) = protected_dirs.iter().rposition(|path| path == &record.path) {
                    protected_dirs.swap_remove(index);
                    continue;
                }

                // Directory removal is deliberately non-recursive. A concurrent
                // new/protected child therefore makes the operation fail safely
                // instead of widening deletion beyond the preflight plan.
                if dest.exists(&record.path).await? {
                    dest.remove(&record.path, false).await?;
                    stats.files_deleted += 1;
                }
            }
            DeleteKind::FileLike => {
                if dest.exists(&record.path).await? {
                    dest.remove(&record.path, false).await?;
                    stats.files_deleted += 1;
                }
            }
        }
    }

    Ok(())
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
    async fn cache_option_writes_safe_marker_only_after_success() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"content").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());
        let mut sync_config = config();
        sync_config.cache = true;

        run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &sync_config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        assert!(dest.path().join(".sy-dir-cache.json").exists());
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

    #[tokio::test]
    async fn delete_keeps_parent_with_excluded_descendant() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::create_dir(dest.path().join("extra")).unwrap();
        std::fs::write(dest.path().join("extra").join("keep.log"), b"keep").unwrap();
        std::fs::write(dest.path().join("extra").join("remove.tmp"), b"remove").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());
        let mut sync_config = config();
        sync_config.delete = DeleteMode::Enabled {
            threshold: 100,
            force: false,
        };
        sync_config.filter_engine.add_exclude("*.log").unwrap();

        run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &sync_config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        assert!(dest.path().join("extra").join("keep.log").exists());
        assert!(!dest.path().join("extra").join("remove.tmp").exists());
        assert!(dest.path().join("extra").exists());
    }
}
