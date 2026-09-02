//! Compatibility execution bridge for the v0.5 local engine.
//!
//! Ordered scanning, merge reconciliation, and semantic comparison live under
//! `engine/`. This module only adapts the resulting `SyncOp`s to the legacy task
//! executor while that executor is being replaced.

use crate::endpoint::io::hash_file_streaming;
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError};
use crate::sync::config::{DeleteMode, SyncConfig};
use crate::sync::executor::{BackupConfig, ExecuteConfig, TaskExecutor};
use crate::sync::scanner::{FileEntry, ScanOptions};
use crate::sync::stats::SyncStats;
use crate::sync::strategy::{SyncAction, SyncTask};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sy::engine::delete_journal::{DeleteJournal, DeleteJournalReader, DeleteKind};
use sy::engine::delete_plan::{enforce_delete_policy, DeleteLimit, DeletePlanError, DeletePolicy};
use sy::engine::domain::{Entry, EntryKind, SyncOp, Timestamp};
use sy::engine::planner::{
    finish_content_comparison, plan_entry, ComparisonMode, ComparisonPolicy, PlanDecision,
};
use sy::engine::reconcile::{EngineError, EntryStream, OrderedReconciler, ReconcileItem};
use sy::engine::scan::{EntryMetadataRequest, ScanRequest};

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
        tokio::fs::create_dir_all(dest.root()).await?;
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
        DeleteMode::Enabled { limit, force } => {
            let plan = preflight_delete(source, dest, config, scan_options, limit, force).await?;

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
    )?
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
        rate_limiter: config.bwlimit.map(|limit| {
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::sync::ratelimit::RateLimiter::new(limit),
            ))
        }),
    });

    let batch_size = config
        .max_concurrent
        .max(1)
        .saturating_mul(4)
        .clamp(MIN_BATCH_SIZE, MAX_BATCH_SIZE);
    let mut stats = SyncStats::default();
    let mut batch = Vec::with_capacity(batch_size);
    let scan_request = scan_request(config, scan_options);
    let source_stream = crate::endpoint::local_entry_scan::local_entry_stream(
        source.root().to_path_buf(),
        scan_request,
    );
    let dest_stream = destination_stream(dest.root(), scan_request).await?;
    let mut reconciler = OrderedReconciler::new(source_stream, dest_stream);
    let comparison = comparison_policy(config);

    while let Some(item) = reconciler.next().await.map_err(map_engine_error)? {
        let source_entry = match item {
            ReconcileItem::SourceOnly(source_entry) => {
                stats.files_scanned += 1;
                if !source_entry_selected(config, &source_entry) {
                    continue;
                }
                if config.existing {
                    stats.files_skipped += 1;
                    continue;
                }
                source_entry
            }
            ReconcileItem::Matched {
                source: source_entry,
                destination,
            } => {
                stats.files_scanned += 1;
                if !source_entry_selected(config, &source_entry) {
                    continue;
                }

                let operation =
                    plan_matched(source, dest, comparison, source_entry, destination).await?;
                queue_operation(
                    source, config, operation, &mut batch, batch_size, &executor, &mut stats,
                )
                .await?;
                continue;
            }
            ReconcileItem::DestinationOnly(_) => continue,
        };

        let operation = match plan_entry(source_entry, None, comparison) {
            PlanDecision::Ready(operation) => operation,
            PlanDecision::NeedContentComparison { .. } => {
                return Err(SyncError::Config(
                    "content comparison requested for a missing destination".to_string(),
                ));
            }
        };
        queue_operation(
            source, config, operation, &mut batch, batch_size, &executor, &mut stats,
        )
        .await?;
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
        crate::sync::dircache::DirectoryCache::new().save(dest.root())?;
    }

    stats.duration = started.elapsed();
    Ok(stats)
}

async fn plan_matched(
    source_endpoint: &dyn Endpoint,
    dest_endpoint: &dyn Endpoint,
    policy: ComparisonPolicy,
    source: Entry,
    destination: Entry,
) -> Result<SyncOp> {
    match plan_entry(source, Some(destination), policy) {
        PlanDecision::Ready(operation) => Ok(operation),
        PlanDecision::NeedContentComparison {
            source,
            destination,
        } => {
            let source_hash = hash_file_streaming(source_endpoint, source.path.as_path()).await?;
            let destination_hash =
                hash_file_streaming(dest_endpoint, destination.path.as_path()).await?;
            Ok(finish_content_comparison(
                source,
                destination,
                source_hash == destination_hash,
                policy,
            ))
        }
    }
}

async fn queue_operation(
    source_endpoint: &dyn Endpoint,
    config: &SyncConfig,
    operation: SyncOp,
    batch: &mut Vec<SyncTask>,
    batch_size: usize,
    executor: &TaskExecutor<'_>,
    stats: &mut SyncStats,
) -> Result<()> {
    let Some(task) = compatibility_task(source_endpoint, config, operation).await? else {
        stats.files_skipped += 1;
        return Ok(());
    };

    if config.dry_run {
        record_planned_task(&task, config, stats);
        return Ok(());
    }

    batch.push(task);
    if batch.len() >= batch_size {
        execute_batch(executor, batch, stats).await?;
    }
    Ok(())
}

async fn compatibility_task(
    source_endpoint: &dyn Endpoint,
    config: &SyncConfig,
    operation: SyncOp,
) -> Result<Option<SyncTask>> {
    let (source, action) = match operation {
        SyncOp::Create { source } => (source, SyncAction::Create),
        SyncOp::Update { source, .. } => (source, SyncAction::Update),
        SyncOp::Replace {
            source,
            destination,
        } => {
            if source.kind == EntryKind::Directory || destination.kind == EntryKind::Directory {
                return Err(SyncError::Config(format!(
                    "directory type replacement is not yet transactional at {}",
                    source.path
                )));
            }
            (source, SyncAction::Update)
        }
        SyncOp::Skip { .. } => return Ok(None),
        SyncOp::Metadata { source, .. } => {
            return Err(SyncError::Config(format!(
                "metadata-only operation reached legacy executor boundary at {}",
                source.path
            )))
        }
    };

    let dest_path = source.path.as_path().to_path_buf();
    let source = compatibility_entry(source_endpoint, config, source).await?;
    Ok(Some(SyncTask {
        source: Some(Arc::new(source)),
        dest_path,
        action,
        source_checksum: None,
        dest_checksum: None,
    }))
}

async fn compatibility_entry(
    source_endpoint: &dyn Endpoint,
    config: &SyncConfig,
    entry: Entry,
) -> Result<FileEntry> {
    let relative_path = entry.path.as_path().to_path_buf();
    let absolute_path = source_endpoint.root().join(&relative_path);
    let (inode, nlink) =
        hardlink_compatibility_metadata(config, &absolute_path, entry.kind).await?;
    let mode = entry.unix_mode.unwrap_or_else(|| {
        if entry.kind == EntryKind::Directory {
            0o755
        } else {
            0o644
        }
    });

    Ok(FileEntry {
        path: Arc::new(absolute_path),
        relative_path: Arc::new(relative_path),
        size: entry.size,
        modified: system_time(entry.modified)?,
        mode,
        is_dir: entry.kind == EntryKind::Directory,
        is_symlink: entry.kind == EntryKind::Symlink,
        symlink_target: entry.symlink_target.map(Arc::new),
        is_sparse: false,
        allocated_size: entry.size,
        xattrs: None,
        inode,
        nlink,
        acls: None,
        bsd_flags: None,
    })
}

#[cfg(unix)]
async fn hardlink_compatibility_metadata(
    config: &SyncConfig,
    path: &Path,
    kind: EntryKind,
) -> Result<(Option<u64>, u64)> {
    use std::os::unix::fs::MetadataExt;

    if !config.preserve.hardlinks || kind != EntryKind::File {
        return Ok((None, 1));
    }

    let metadata = tokio::fs::symlink_metadata(path).await?;
    Ok((Some(metadata.ino()), metadata.nlink()))
}

#[cfg(not(unix))]
async fn hardlink_compatibility_metadata(
    _config: &SyncConfig,
    _path: &Path,
    _kind: EntryKind,
) -> Result<(Option<u64>, u64)> {
    Ok((None, 1))
}

fn system_time(timestamp: Timestamp) -> Result<SystemTime> {
    let seconds = timestamp.seconds();
    let nanoseconds = timestamp.nanoseconds();

    let value = if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(seconds as u64, nanoseconds))
    } else {
        let magnitude = seconds.unsigned_abs();
        let before_epoch = if nanoseconds == 0 {
            Duration::new(magnitude, 0)
        } else {
            Duration::new(magnitude - 1, 1_000_000_000 - nanoseconds)
        };
        UNIX_EPOCH.checked_sub(before_epoch)
    };

    value.ok_or_else(|| {
        SyncError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "entry timestamp is outside SystemTime range",
        ))
    })
}

fn comparison_policy(config: &SyncConfig) -> ComparisonPolicy {
    let mode = if config.comparison.checksum {
        ComparisonMode::Checksum
    } else if config.comparison.size_only {
        ComparisonMode::SizeOnly
    } else if config.comparison.ignore_times {
        ComparisonMode::Always
    } else {
        ComparisonMode::Quick
    };

    ComparisonPolicy {
        mode,
        ignore_existing: config.comparison.ignore_existing,
        existing_only: config.existing,
        update_only: config.comparison.update_only,
        // The compatibility executor does not yet expose a metadata-only task.
        // Keep the new planner honest by not requesting semantics it cannot apply.
        preserve_permissions: false,
        preserve_times: false,
    }
}

fn scan_request(config: &SyncConfig, options: ScanOptions) -> ScanRequest {
    ScanRequest {
        respect_gitignore: options.respect_gitignore,
        include_git_dir: options.include_git_dir,
        max_depth: options.dirs_only.then_some(1),
        metadata: EntryMetadataRequest {
            unix_mode: config.preserve.permissions,
            symlink_target: true,
            identity: true,
            hardlink_group: config.preserve.hardlinks,
        },
    }
}

fn delete_scan_request(options: ScanOptions) -> ScanRequest {
    ScanRequest {
        respect_gitignore: options.respect_gitignore,
        include_git_dir: options.include_git_dir,
        max_depth: options.dirs_only.then_some(1),
        metadata: EntryMetadataRequest {
            unix_mode: false,
            symlink_target: false,
            identity: false,
            hardlink_group: false,
        },
    }
}

/// Destination-side delete scans are always complete: gitignore selection is
/// never destination-filtered, and ignore narrowing happens in
/// `dest_delete_eligible` through the source-derived scope.
fn complete_delete_scan_request() -> ScanRequest {
    ScanRequest {
        respect_gitignore: false,
        include_git_dir: true,
        max_depth: None,
        metadata: EntryMetadataRequest {
            unix_mode: false,
            symlink_target: false,
            identity: false,
            hardlink_group: false,
        },
    }
}

async fn destination_stream(root: &Path, request: ScanRequest) -> Result<EntryStream> {
    if !tokio::fs::try_exists(root).await? {
        return Ok(Box::pin(futures::stream::empty()));
    }
    Ok(crate::endpoint::local_entry_scan::local_entry_stream(
        root.to_path_buf(),
        request,
    ))
}

fn source_entry_selected(config: &SyncConfig, entry: &Entry) -> bool {
    if !entry.is_directory() {
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
        .should_exclude(entry.path.as_path(), entry.is_directory())
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
    limit: DeleteLimit,
    force: bool,
) -> Result<DeletePlan> {
    // The destination scan stays complete; every narrowing happens in
    // `dest_delete_eligible` through the filter engine and the source-derived
    // ignore scope. Filtering the destination walk would hide entries from
    // protection accounting (see `engine::ignore_scope`).
    let request = delete_scan_request(scan_options);
    let source_stream =
        crate::endpoint::local_entry_scan::local_entry_stream(source.root().to_path_buf(), request);
    let dest_stream = destination_stream(dest.root(), complete_delete_scan_request()).await?;
    let mut ignore_scope = sy::engine::ignore_scope::SourceIgnoreScope::new(
        source.root(),
        scan_options.respect_gitignore,
    );
    let mut reconciler = OrderedReconciler::new(source_stream, dest_stream);
    let mut source_entries = 0_usize;
    let mut eligible_dest_entries = 0_usize;
    let mut delete_candidates = 0_usize;
    let mut protected_dest_dir: Option<PathBuf> = None;
    let mut candidate_dirs = Vec::<CandidateDeleteDir>::new();
    let mut journal = DeleteJournal::new().await?;

    while let Some(item) = reconciler.next().await.map_err(map_engine_error)? {
        match item {
            ReconcileItem::SourceOnly(_) => {
                source_entries += 1;
            }
            ReconcileItem::Matched {
                source: _,
                destination,
            } => {
                close_candidate_dirs(destination.path.as_path(), &mut candidate_dirs);
                source_entries += 1;
                if dest_delete_eligible(
                    config,
                    &mut ignore_scope,
                    &destination,
                    &mut protected_dest_dir,
                ) {
                    eligible_dest_entries += 1;
                }
                protect_candidate_dirs(&mut journal, &mut candidate_dirs, &mut delete_candidates)
                    .await?;
            }
            ReconcileItem::DestinationOnly(destination) => {
                close_candidate_dirs(destination.path.as_path(), &mut candidate_dirs);
                if dest_delete_eligible(
                    config,
                    &mut ignore_scope,
                    &destination,
                    &mut protected_dest_dir,
                ) {
                    eligible_dest_entries += 1;
                    delete_candidates += 1;
                    append_delete_candidate(&mut journal, &destination, &mut candidate_dirs)
                        .await?;
                } else {
                    protect_candidate_dirs(
                        &mut journal,
                        &mut candidate_dirs,
                        &mut delete_candidates,
                    )
                    .await?;
                }
            }
        }
    }

    let eligible = u64::try_from(eligible_dest_entries)
        .map_err(|_| SyncError::Config("eligible delete count exceeds u64 range".to_string()))?;
    let candidates = u64::try_from(delete_candidates)
        .map_err(|_| SyncError::Config("delete candidate count exceeds u64 range".to_string()))?;
    enforce_delete_policy(DeletePolicy { limit, force }, eligible, candidates)
        .map_err(map_delete_plan_error)?;
    let journal = journal.seal().await?;
    Ok(DeletePlan {
        source_entries,
        eligible_dest_entries,
        delete_candidates,
        journal,
    })
}

fn dest_delete_eligible(
    config: &SyncConfig,
    ignore_scope: &mut sy::engine::ignore_scope::SourceIgnoreScope,
    entry: &Entry,
    protected_dir: &mut Option<PathBuf>,
) -> bool {
    if let Some(directory) = protected_dir.as_ref() {
        if entry.path.as_path().starts_with(directory) {
            return false;
        }
        *protected_dir = None;
    }

    if config
        .filter_engine
        .should_exclude(entry.path.as_path(), entry.is_directory())
    {
        if entry.is_directory() {
            *protected_dir = Some(entry.path.as_path().to_path_buf());
        }
        return false;
    }

    // Source-derived ignore rules protect destination-only entries the
    // source walk would have omitted (e.g. `--gitignore` build artifacts).
    if ignore_scope.protects(entry) {
        if entry.is_directory() {
            *protected_dir = Some(entry.path.as_path().to_path_buf());
        }
        return false;
    }

    true
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
    entry: &Entry,
    candidate_dirs: &mut Vec<CandidateDeleteDir>,
) -> Result<()> {
    if entry.is_directory() {
        journal
            .append(entry.path.as_path(), DeleteKind::Directory)
            .await?;
        candidate_dirs.push(CandidateDeleteDir {
            path: entry.path.as_path().to_path_buf(),
            protected: false,
        });
    } else {
        journal
            .append(entry.path.as_path(), DeleteKind::FileLike)
            .await?;
    }
    Ok(())
}

async fn protect_candidate_dirs(
    journal: &mut DeleteJournal,
    candidate_dirs: &mut [CandidateDeleteDir],
    delete_candidates: &mut usize,
) -> Result<()> {
    for candidate in candidate_dirs {
        if candidate.protected {
            continue;
        }
        journal
            .append(&candidate.path, DeleteKind::ProtectDirectory)
            .await?;
        candidate.protected = true;
        *delete_candidates = delete_candidates
            .checked_sub(1)
            .ok_or_else(|| SyncError::Config("delete candidate count underflow".to_string()))?;
    }
    Ok(())
}

fn map_delete_plan_error(error: DeletePlanError) -> SyncError {
    match error {
        DeletePlanError::ThresholdExceeded {
            eligible_destination_entries,
            delete_candidates,
            threshold,
        } => {
            let percentage = if eligible_destination_entries == 0 {
                0.0
            } else {
                delete_candidates as f64 * 100.0 / eligible_destination_entries as f64
            };
            SyncError::DeletionThresholdExceeded {
                percentage,
                threshold,
            }
        }
        DeletePlanError::CountExceeded {
            delete_candidates,
            limit,
        } => SyncError::DeletionCountExceeded {
            delete_candidates,
            limit,
        },
        other => SyncError::Io(std::io::Error::other(other.to_string())),
    }
}

async fn execute_delete_journal(
    dest: &dyn Endpoint,
    journal: &mut DeleteJournalReader,
    stats: &mut SyncStats,
) -> Result<()> {
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

fn record_planned_task(task: &SyncTask, config: &SyncConfig, stats: &mut SyncStats) {
    match task.action {
        SyncAction::Skip => stats.files_skipped += 1,
        SyncAction::Create => {
            stats.files_created += 1;
            if let Some(source) = &task.source {
                if !source.is_dir {
                    stats.bytes_would_add += source.size;
                }
                if config.diff_mode {
                    if source.is_dir {
                        tracing::info!("Would create: {}", task.dest_path.display());
                    } else {
                        tracing::info!(
                            "Would create: {} ({})",
                            task.dest_path.display(),
                            crate::resource::format_bytes(source.size)
                        );
                    }
                }
            }
        }
        SyncAction::Update => {
            stats.files_updated += 1;
            if let Some(source) = &task.source {
                if !source.is_dir {
                    stats.bytes_would_change += source.size;
                }
                if config.diff_mode {
                    if source.is_dir {
                        tracing::info!("Would update: {}", task.dest_path.display());
                    } else {
                        tracing::info!(
                            "Would update: {} ({}, using delta sync)",
                            task.dest_path.display(),
                            crate::resource::format_bytes(source.size)
                        );
                    }
                }
            }
        }
        SyncAction::Delete => {
            stats.files_deleted += 1;
            if config.diff_mode {
                tracing::info!("Would delete: {}", task.dest_path.display());
            }
        }
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

fn map_engine_error(error: EngineError) -> SyncError {
    SyncError::Io(std::io::Error::other(error))
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
    async fn engine_reconciler_creates_and_skips() {
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

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_atomically_replaces_regular_file() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::os::unix::fs::symlink("target", source.path().join("entry")).unwrap();
        std::fs::write(dest.path().join("entry"), b"old").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());

        run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &config(),
            ScanOptions::default(),
        )
        .await
        .unwrap();

        assert!(std::fs::symlink_metadata(dest.path().join("entry"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(dest.path().join("entry")).unwrap(),
            Path::new("target")
        );
    }

    #[tokio::test]
    async fn directory_type_transition_remains_rejected() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::create_dir(source.path().join("entry")).unwrap();
        std::fs::write(dest.path().join("entry"), b"old").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());

        let error = run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &config(),
            ScanOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("directory type replacement"));
        assert_eq!(std::fs::read(dest.path().join("entry")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn delete_preflight_removes_only_destination_only_entries() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("keep"), b"same").unwrap();
        std::fs::write(dest.path().join("keep"), b"same").unwrap();
        std::fs::write(dest.path().join("remove"), b"old").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());
        let mut cfg = config();
        cfg.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };

        run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &cfg,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        assert!(dest.path().join("keep").exists());
        assert!(!dest.path().join("remove").exists());
    }

    #[tokio::test]
    async fn protected_candidate_parent_is_not_counted_against_delete_limit() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let excluded = dest.path().join("parent/excluded");
        std::fs::create_dir_all(&excluded).unwrap();
        std::fs::write(excluded.join("keep"), b"keep").unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());
        let mut cfg = config();
        cfg.filter_engine.add_exclude("parent/excluded/").unwrap();
        cfg.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(0),
            force: false,
        };

        let stats = run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &cfg,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(stats.files_deleted, 0);
        assert!(excluded.join("keep").exists());
        assert!(dest.path().join("parent").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hardlink_preservation_uses_exact_legacy_inode_only_at_executor_boundary() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("first"), b"content").unwrap();
        std::fs::hard_link(source.path().join("first"), source.path().join("second")).unwrap();
        let source_endpoint = LocalEndpoint::new(source.path().to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest.path().to_path_buf());
        let mut cfg = config();
        cfg.preserve.hardlinks = true;

        run_local_sync(
            &source_endpoint,
            &dest_endpoint,
            &cfg,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        use std::os::unix::fs::MetadataExt;
        let first = std::fs::metadata(dest.path().join("first")).unwrap();
        let second = std::fs::metadata(dest.path().join("second")).unwrap();
        assert_eq!(first.ino(), second.ino());
    }
}
