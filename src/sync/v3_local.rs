//! v3 local execution: local source root -> local destination root (item 6).
//!
//! The same controller pipeline as the remote directions — shared preflight
//! (one reconcile pass, single-pass `DeleteTracker`), bounded concurrent
//! execution, reverse-order deletes, journal-owned finalize — with
//! `LocalSyncExecutor` on both sides. Native fast paths (clonefile,
//! reflink+patch, sparse copy) live in the transfer layer and are preserved
//! untouched.
//!
//! The legacy reconcile/TaskExecutor path remains for the preservation
//! cluster (xattrs/hardlinks/ACLs/flags/group/owner/devices/keep-dirlinks)
//! until preservation lands once on this executor; every other CLI-accepted
//! local sync routes here.

use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::scanner::ScanOptions;
use crate::sync::v3_push::{
    comparison_policy, delete_policy, entry_in_depth_scope, entry_in_size_scope,
    entry_in_vcs_scope, entry_not_source_ignored, entry_selected_by_symlink_mode, map_io,
    preview_stats, summary_stats,
};
use crate::sync::{SyncConfig, SyncStats};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Instant;
use sy::engine::domain::Entry;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::remote::local_executor::LocalSyncExecutor;
use sy::remote::push_controller::{
    preflight_remote_push_scoped, preflight_remote_push_scoped_with_content, preview_remote_push,
    RemotePushController, RemotePushControllerError, RemotePushSummary,
};

/// Whether a local sync must use the legacy TaskExecutor path.
///
/// Only the preservation cluster remains there; it is deliberately deferred
/// so preservation lands once on this executor rather than twice.
pub(super) fn legacy_fallback_reason(config: &SyncConfig) -> Option<&'static str> {
    if config.preserve.xattrs
        || config.preserve.hardlinks
        || config.preserve.acls
        || config.preserve.flags
        || config.preserve.group
        || config.preserve.owner
        || config.preserve.devices
        || config.preserve.keep_dirlinks
    {
        return Some("requested preservation semantics exceed current v3 mode/mtime support");
    }
    None
}

pub(super) async fn run(
    source_root: &Path,
    destination_root: &Path,
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Result<SyncStats> {
    let reporter = std::sync::Arc::new(sy::sync::output::SyncReporter::new(
        config.itemize_changes,
        config.json,
        config.quiet,
        config.perf,
    ));
    let scan_started = Instant::now();
    reporter.start(source_root, destination_root);

    // A fresh destination root is created BEFORE its scan so a first sync
    // into a new directory sees an empty tree instead of a scan error,
    // matching the legacy local path's ordering. Dry-run never mutates.
    if !destination_root.exists() && !config.dry_run {
        std::fs::create_dir_all(destination_root).map_err(map_io)?;
    }

    // Both scans are local. The source walk honors the ignore rules where
    // the files live; the destination scan stays COMPLETE (gitignore never
    // narrows what reconciliation sees) — the same boundary as every other
    // direction.
    let source = crate::sync::v3_push::filtered_source_stream(
        sy::endpoint::local_entry_scan::local_entry_stream(
            source_root.to_path_buf(),
            crate::sync::v3_push::source_scan_request(config, scan_options),
        ),
        config.filter_engine.clone(),
    );
    let destination = sy::endpoint::local_entry_scan::local_entry_stream(
        destination_root.to_path_buf(),
        crate::sync::v3_push::destination_scan_request(config),
    );

    let min_size = config.min_size;
    let max_size = config.max_size;
    let skip_symlinks = config.preserve.symlink_mode == SymlinkMode::Skip;
    let delete_filter = config.filter_engine.clone();
    let max_depth = crate::sync::v3_push::selection_max_depth(config, scan_options);
    let include_git_dir = scan_options.include_git_dir;
    // Source-derived ignore scope: destination-only paths the source rules
    // would ignore are protected from deletion (see `engine::ignore_scope`).
    let ignore_scope = std::sync::Arc::new(std::sync::Mutex::new(
        sy::engine::ignore_scope::SourceIgnoreScope::new(
            source_root,
            scan_options.respect_gitignore,
        ),
    ));

    let plan = if config.comparison.checksum {
        let source_root_owned = source_root.to_path_buf();
        let destination_root_owned = destination_root.to_path_buf();
        preflight_remote_push_scoped_with_content(
            source,
            destination,
            comparison_policy(config),
            delete_policy(&config.delete),
            move |entry| {
                entry_in_size_scope(entry, min_size, max_size)
                    && entry_selected_by_symlink_mode(entry, skip_symlinks)
            },
            move |entry| {
                delete_filter.should_include(entry.path.as_path(), entry.is_directory())
                    && entry_in_depth_scope(entry, max_depth)
                    && entry_in_vcs_scope(entry, include_git_dir)
                    && entry_not_source_ignored(&ignore_scope, entry)
            },
            move |source: Entry, destination: Entry| {
                let source_root = source_root_owned.clone();
                let destination_root = destination_root_owned.clone();
                async move {
                    let source_endpoint = sy::endpoint::local::LocalEndpoint::new(source_root);
                    let destination_endpoint =
                        sy::endpoint::local::LocalEndpoint::new(destination_root);
                    // Checksum comparison hashes both sides through the
                    // same bounded streaming hasher the local verify path
                    // uses; sizes are already equal (the planner only asks
                    // for content comparison on size matches).
                    let source_hash = sy::endpoint::io::hash_file_streaming(
                        &source_endpoint,
                        source.path.as_path(),
                    )
                    .await
                    .map_err(|error| RemotePushControllerError::Worker(error.to_string()))?;
                    let destination_hash = sy::endpoint::io::hash_file_streaming(
                        &destination_endpoint,
                        destination.path.as_path(),
                    )
                    .await
                    .map_err(|error| RemotePushControllerError::Worker(error.to_string()))?;
                    Ok(source_hash == destination_hash)
                }
            },
        )
        .await
        .map_err(crate::sync::v3_push::map_controller_error)?
    } else {
        preflight_remote_push_scoped(
            source,
            destination,
            comparison_policy(config),
            delete_policy(&config.delete),
            move |entry| {
                entry_in_size_scope(entry, min_size, max_size)
                    && entry_selected_by_symlink_mode(entry, skip_symlinks)
            },
            move |entry| {
                delete_filter.should_include(entry.path.as_path(), entry.is_directory())
                    && entry_in_depth_scope(entry, max_depth)
                    && entry_in_vcs_scope(entry, include_git_dir)
                    && entry_not_source_ignored(&ignore_scope, entry)
            },
        )
        .await
        .map_err(crate::sync::v3_push::map_controller_error)?
    };

    if config.dry_run {
        let diff_mode = config.diff_mode;
        let preview = preview_remote_push(plan, |item| {
            if diff_mode {
                crate::sync::v3_push::emit_diff_line(item);
            }
        })
        .await
        .map_err(crate::sync::v3_push::map_controller_error)?;
        return preview_stats(preview);
    }

    let max_in_flight = NonZeroUsize::new(config.max_concurrent).ok_or_else(|| {
        SyncError::Config("parallel transfer count must be greater than zero".to_string())
    })?;
    let active_files = u32::try_from(config.max_concurrent).map_err(|_| {
        SyncError::Config("parallel transfer count exceeds v3 scheduler range".to_string())
    })?;
    let scheduler = Scheduler::new(ResourceBudget {
        active_files,
        ..ResourceBudget::default()
    })
    .map_err(map_io)?;
    let rate_limiter = config.bwlimit.map(|limit| {
        std::sync::Arc::new(std::sync::Mutex::new(
            sy::sync::ratelimit::RateLimiter::new(limit),
        ))
    });
    let executor = LocalSyncExecutor::new(
        source_root.to_path_buf(),
        destination_root.to_path_buf(),
        scheduler,
    )
    .with_backup(
        config.backup.is_some(),
        backup_dir(config, destination_root),
        config.suffix.clone(),
    )
    .with_follow_symlinks(config.preserve.symlink_mode == SymlinkMode::Follow)
    .with_rate_limiter(rate_limiter)
    .with_remove_source_files(config.remove_source_files)
    .with_verify_on_write(config.verification.verify_on_write)
    .with_reporter(Some(reporter.clone()));

    let scan_elapsed = scan_started.elapsed();
    let transfer_started = Instant::now();
    let summary: RemotePushSummary = RemotePushController::new(executor, max_in_flight)
        .execute(plan)
        .await
        .map_err(crate::sync::v3_push::map_controller_error)?;
    let mut stats = summary_stats(summary)?;
    // --verify's staged verification is fail-fast on this path: a successful
    // run verified every transferred file.
    if config.verification.verify_on_write {
        stats.files_verified = usize::try_from(summary.files_transferred).map_err(|_| {
            SyncError::Config("v3 verified counter exceeds platform usize".to_string())
        })?;
    }
    stats.duration = scan_elapsed + transfer_started.elapsed();
    reporter.finish(
        &sy::sync::output::SummaryCounts {
            files_created: stats.files_created,
            files_updated: stats.files_updated,
            files_skipped: stats.files_skipped,
            files_deleted: stats.files_deleted,
            bytes_transferred: stats.bytes_transferred,
            duration_secs: stats.duration.as_secs_f64(),
            files_verified: stats.files_verified as u64,
            verification_failures: stats.verification_failures,
        },
        sy::sync::output::SyncTimings {
            scan: scan_elapsed,
            transfer: transfer_started.elapsed(),
        },
    );
    Ok(stats)
}

/// --backup-dir is local: absolute honored as-is (the local engine's rule);
/// relative anchored to the destination root.
fn backup_dir(config: &SyncConfig, destination_root: &Path) -> Option<PathBuf> {
    let dir = config.backup_dir.as_ref()?;
    if dir.is_absolute() {
        Some(dir.clone())
    } else {
        Some(destination_root.join(dir))
    }
}
