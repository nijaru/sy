//! Synchronization policy translations and controller mappings.
//!
//! Converts high-level CLI and `SyncConfig` options into engine-level requests
//! (`ScanRequest`, `EntryMetadataRequest`, `ComparisonPolicy`, `DeletePolicy`,
//! `CompressionPolicy`), evaluates filtering and deletion scopes, and maps
//! controller results and errors back into `SyncStats` and `SyncError`.

use crate::cli::SymlinkMode;
use crate::engine::compression::CompressionDetection;
use crate::error::{Result, SyncError};
use crate::filter::FilterEngine;
use crate::sync::config::{DeleteMode, SyncConfig};
use crate::sync::scanner::ScanOptions;
use crate::sync::stats::SyncStats;
use futures::future;
use sy::engine::compression::CompressionPolicy;
use sy::engine::controller::{ControllerError, PreviewOp, SyncPreview, SyncSummary};
use sy::engine::delete_plan::{DeletePlanError, DeletePolicy};
use sy::engine::domain::{Entry, EntryKind, RelativePath, SyncOp};
use sy::engine::namespace::{NamespacePreflightError, NamespaceSemantics};
use sy::engine::planner::{ComparisonMode, ComparisonPolicy};
use sy::engine::reconcile::EntryStream;
use sy::engine::scan::{EntryMetadataRequest, ScanRequest};

pub(crate) fn finish_report(
    reporter: &sy::sync::output::SyncReporter,
    stats: &SyncStats,
    scan: std::time::Duration,
    transfer: std::time::Duration,
) {
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
        sy::sync::output::SyncTimings { scan, transfer },
    );
}

pub(crate) struct SourceFilterSelection {
    filter: FilterEngine,
    excluded_subtree: Option<RelativePath>,
}

impl SourceFilterSelection {
    pub(crate) fn new(filter: FilterEngine) -> Self {
        Self {
            filter,
            excluded_subtree: None,
        }
    }

    pub(crate) fn includes(&mut self, entry: &Entry) -> bool {
        if let Some(excluded) = self.excluded_subtree.as_ref() {
            if entry.path.as_path().starts_with(excluded.as_path()) {
                return false;
            }
            self.excluded_subtree = None;
        }

        let included = self
            .filter
            .should_include(entry.path.as_path(), entry.is_directory());
        if !included && entry.is_directory() {
            self.excluded_subtree = Some(entry.path.clone());
        }
        included
    }
}

pub(crate) fn filtered_source_stream(source: EntryStream, filter: FilterEngine) -> EntryStream {
    if filter.is_empty() {
        return source;
    }

    let mut selection = SourceFilterSelection::new(filter);
    source.filter_map(move |item| {
        let keep = match item.as_ref() {
            Ok(entry) => selection.includes(entry),
            Err(_) => true,
        };
        future::ready(keep.then_some(item))
    })
}

/// Content comparisons retain the scanned physical address and opened-file
/// identity, including explicit copy-links observations.
pub(crate) async fn observed_hash(
    endpoint: &dyn crate::endpoint::Endpoint,
    entry: &Entry,
    follow: bool,
) -> Result<[u8; 32]> {
    let rooted = endpoint
        .source_rooted_authority()
        .await?
        .ok_or_else(|| SyncError::Config("endpoint cannot bind an observed source hash".into()))?;
    if !follow {
        let fingerprint =
            sy::endpoint::existing::fingerprint(rooted.clone(), entry.clone(), Default::default())
                .await
                .map_err(map_io)?;
        // This helper compares local operator operands. Remote fingerprint RPCs
        // retain the peer's held-root semantics without this pathname contract.
        tokio::task::spawn_blocking(move || rooted.verify_root_path_blocking())
            .await
            .map_err(map_io)?
            .map_err(map_io)?;
        return Ok(fingerprint.content);
    }
    let file = endpoint
        .open_native_file_following(entry.path.as_path())
        .await?
        .ok_or_else(|| {
            SyncError::Config("endpoint cannot hash a followed source observation".into())
        })?;
    let entry = entry.clone();
    let expected = entry.identity.ok_or_else(|| SyncError::SourceChanged {
        path: entry.path.as_path().to_path_buf(),
    })?;
    let path = entry.path.clone();
    let hashing_root = rooted.clone();
    let digest = tokio::task::spawn_blocking(move || {
        let validate = |file: &std::fs::File| -> Result<()> {
            hashing_root.verify_root_path_blocking().map_err(map_io)?;
            if crate::endpoint::local_identity::metadata_identity(
                &file.metadata()?,
                EntryKind::File,
            ) != Some(expected)
            {
                return Err(SyncError::SourceChanged {
                    path: entry.path.as_path().to_path_buf(),
                });
            }
            Ok(())
        };
        let mut file = file;
        validate(&file)?;
        let digest =
            crate::endpoint::existing::hash_observed_bytes(&mut file, entry.size, &entry.path)
                .map_err(|error| match error {
                    crate::endpoint::existing::ExistingDestinationError::ObservationChanged(
                        path,
                    ) => SyncError::SourceChanged {
                        path: path.into_path_buf(),
                    },
                    crate::endpoint::existing::ExistingDestinationError::Io(error) => {
                        SyncError::Io(error)
                    }
                    error => map_io(error),
                })?;
        validate(&file)?;
        Ok::<_, SyncError>(digest)
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error)))??;
    // Check the followed name still identifies the held observation; a retarget
    // during comparison must not authorize an unchanged-file decision.
    let reopened = endpoint
        .open_native_file_following(path.as_path())
        .await?
        .ok_or_else(|| SyncError::SourceChanged {
            path: path.as_path().to_path_buf(),
        })?;
    tokio::task::spawn_blocking(move || {
        rooted.verify_root_path_blocking().map_err(map_io)?;
        if crate::endpoint::local_identity::metadata_identity(
            &reopened.metadata()?,
            EntryKind::File,
        ) != Some(expected)
        {
            return Err(SyncError::SourceChanged {
                path: path.into_path_buf(),
            });
        }
        Ok::<_, SyncError>(digest)
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error)))?
}

pub(crate) fn source_scan_request(config: &SyncConfig, scan_options: ScanOptions) -> ScanRequest {
    ScanRequest {
        respect_gitignore: scan_options.respect_gitignore,
        include_git_dir: scan_options.include_git_dir,
        // --copy-links: the walk yields target kinds.
        follow_symlinks: config.preserve.symlink_mode == SymlinkMode::Follow,
        max_depth: selection_max_depth(config, scan_options),
        metadata: metadata_request(config),
    }
}

pub(crate) fn destination_scan_request(config: &SyncConfig) -> ScanRequest {
    ScanRequest {
        respect_gitignore: false,
        include_git_dir: true,
        follow_symlinks: false,
        max_depth: None,
        metadata: metadata_request(config),
    }
}

pub(crate) fn metadata_request(config: &SyncConfig) -> EntryMetadataRequest {
    EntryMetadataRequest {
        unix_mode: true,
        symlink_target: true,
        identity: true,
        hardlink_group: config.preserve.hardlinks,
    }
}

pub(crate) fn selection_max_depth(config: &SyncConfig, scan_options: ScanOptions) -> Option<usize> {
    (config.dirs || scan_options.dirs_only).then_some(1)
}

pub(crate) fn entry_in_depth_scope(entry: &Entry, max_depth: Option<usize>) -> bool {
    max_depth.is_none_or(|depth| entry.path.as_path().components().count() <= depth)
}

pub(crate) fn entry_in_vcs_scope(entry: &Entry, include_git_dir: bool) -> bool {
    include_git_dir
        || !entry
            .path
            .as_path()
            .components()
            .any(|component| component.as_os_str() == std::ffi::OsStr::new(".git"))
}

/// Destination-only entries the source tree's ignore rules would exclude are
/// out of deletion scope.
/// --links=skip: symlinks are excluded from transfer selection but remain
/// in the reconciliation stream, so their destination counterparts keep
/// delete protection (selection must never narrow deletion scope).
pub(crate) fn entry_selected_by_symlink_mode(entry: &Entry, skip_symlinks: bool) -> bool {
    if skip_symlinks && entry.kind == sy::engine::domain::EntryKind::Symlink {
        return false;
    }
    true
}

pub(crate) fn entry_not_source_ignored(
    scope: &std::sync::Arc<std::sync::Mutex<sy::engine::ignore_scope::SourceIgnoreScope>>,
    entry: &Entry,
) -> bool {
    let Ok(mut scope) = scope.lock() else {
        // A poisoned cache lock means we cannot prove the entry is safe to
        // delete; protecting it is the only conservative choice.
        return false;
    };
    !scope.protects(entry)
}

pub(crate) fn entry_in_size_scope(
    entry: &Entry,
    min_size: Option<u64>,
    max_size: Option<u64>,
) -> bool {
    if entry.is_directory() {
        return true;
    }

    min_size.is_none_or(|min| entry.size >= min) && max_size.is_none_or(|max| entry.size <= max)
}

/// Map CLI compression flags onto the per-transfer policy.
pub(crate) fn compression_policy(config: &SyncConfig) -> Option<CompressionPolicy> {
    match config.compression_detection {
        CompressionDetection::Never => None,
        CompressionDetection::Auto | CompressionDetection::Extension => {
            Some(CompressionPolicy::Auto)
        }
        CompressionDetection::Always => Some(CompressionPolicy::Always),
    }
}

pub(crate) fn comparison_policy(
    config: &SyncConfig,
    namespace_semantics: NamespaceSemantics,
) -> ComparisonPolicy {
    let mode = if config.comparison.checksum {
        ComparisonMode::Checksum
    } else if config.comparison.ignore_times {
        ComparisonMode::Always
    } else if config.comparison.size_only {
        ComparisonMode::SizeOnly
    } else {
        ComparisonMode::Quick
    };
    ComparisonPolicy {
        mode,
        ignore_existing: config.comparison.ignore_existing,
        existing_only: config.existing,
        update_only: config.comparison.update_only,
        preserve_permissions: config.preserve.permissions,
        preserve_times: config.preserve.times,
        namespace_semantics,
    }
}

pub(crate) fn delete_policy(mode: &DeleteMode) -> Option<DeletePolicy> {
    match mode {
        DeleteMode::Disabled => None,
        DeleteMode::Enabled { limit, force } => Some(DeletePolicy {
            limit: *limit,
            force: *force,
        }),
    }
}

/// Emit one `--diff` dry-run detail line for a planned operation or deletion,
/// mirroring the per-file format (`Would create: path (size)`).
pub(crate) fn emit_diff_line(item: PreviewOp<'_>) {
    match item {
        PreviewOp::Operation(SyncOp::Create { source, .. }) => match source.kind {
            EntryKind::File => tracing::info!(
                "Would create: {} ({})",
                source.path,
                crate::resource::format_bytes(source.size)
            ),
            _ => tracing::info!("Would create: {}", source.path),
        },
        PreviewOp::Operation(SyncOp::Update { source, .. }) => match source.kind {
            EntryKind::File => tracing::info!(
                "Would update: {} ({}, using delta sync)",
                source.path,
                crate::resource::format_bytes(source.size)
            ),
            _ => tracing::info!("Would update: {}", source.path),
        },
        PreviewOp::Operation(SyncOp::Replace { source, .. }) => {
            tracing::info!("Would replace: {}", source.path)
        }
        PreviewOp::Operation(SyncOp::Metadata { source, .. }) => {
            tracing::info!("Would update metadata: {}", source.path)
        }
        PreviewOp::Operation(SyncOp::Skip { source, .. } | SyncOp::Unchanged { source, .. }) => {
            tracing::info!("Would skip: {}", source.path)
        }
        PreviewOp::Delete(delete) => tracing::info!(
            "Would delete: {}{}",
            delete.path,
            if delete.is_directory {
                " (directory)"
            } else {
                ""
            }
        ),
    }
}

pub(crate) fn preview_stats(preview: SyncPreview) -> Result<SyncStats> {
    Ok(SyncStats {
        files_scanned: preview.planned_operations,
        files_created: preview.files_created,
        files_updated: preview.files_updated,
        files_skipped: to_usize(preview.files_skipped, "preview skipped entries")?,
        files_deleted: to_usize(preview.delete_candidates, "preview deleted entries")?,
        bytes_would_add: preview.bytes_to_create,
        bytes_would_change: preview.bytes_to_update,
        dirs_created: preview.dirs_created,
        symlinks_created: preview.symlinks_created,
        ..SyncStats::default()
    })
}

pub(crate) fn summary_stats(summary: SyncSummary) -> Result<SyncStats> {
    Ok(SyncStats {
        files_scanned: summary.planned_operations,
        files_created: summary.files_created,
        files_updated: summary.files_updated,
        files_skipped: to_usize(summary.files_skipped, "skipped entries")?,
        files_deleted: to_usize(summary.deleted_entries, "deleted entries")?,
        bytes_transferred: summary.literal_bytes,
        files_delta_synced: to_usize(summary.delta_files, "delta files")?,
        delta_bytes_saved: summary.reused_bytes,
        dirs_created: summary.dirs_created,
        symlinks_created: summary.symlinks_created,
        ..SyncStats::default()
    })
}

pub(crate) fn to_usize(value: u64, counter: &'static str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| SyncError::Config(format!("{counter} counter exceeds platform usize range")))
}

pub(crate) fn map_controller_error(error: ControllerError) -> SyncError {
    if let ControllerError::DeletePlan(DeletePlanError::ThresholdExceeded {
        eligible_destination_entries,
        delete_candidates,
        threshold,
    }) = &error
    {
        let percentage = if *eligible_destination_entries == 0 {
            0.0
        } else {
            (*delete_candidates as f64 * 100.0) / *eligible_destination_entries as f64
        };
        return SyncError::DeletionThresholdExceeded {
            percentage,
            threshold: *threshold,
        };
    }
    if let ControllerError::DeletePlan(DeletePlanError::CountExceeded {
        delete_candidates,
        limit,
    }) = &error
    {
        return SyncError::DeletionCountExceeded {
            delete_candidates: *delete_candidates,
            limit: *limit,
        };
    }
    if let ControllerError::Namespace(error) = &error {
        match error {
            NamespacePreflightError::Collision(collision) => {
                return SyncError::NamespaceCollision {
                    existing: collision.existing.as_path().to_path_buf(),
                    colliding: collision.colliding.as_path().to_path_buf(),
                };
            }
            NamespacePreflightError::Ambiguity(ambiguity) => {
                return SyncError::NamespaceAmbiguity {
                    existing: ambiguity.existing.as_path().to_path_buf(),
                    colliding: ambiguity.colliding.as_path().to_path_buf(),
                };
            }
            // Scratch I/O and corrupt records stay contextual I/O errors.
            _ => {}
        }
    }
    if let ControllerError::UnsupportedTypeTransition {
        path,
        source_kind,
        destination_kind,
    } = &error
    {
        return SyncError::UnsupportedTypeTransition {
            path: path.as_path().to_path_buf(),
            source_kind: format!("{source_kind:?}"),
            destination_kind: format!("{destination_kind:?}"),
        };
    }
    if let ControllerError::NonEmptyDirectoryReplacementRequiresDelete {
        path,
        discarded_entries,
    } = &error
    {
        return SyncError::NonEmptyDirectoryReplacementRequiresDelete {
            path: path.as_path().to_path_buf(),
            discarded_entries: *discarded_entries,
        };
    }
    if let ControllerError::CannotReplaceDirectoryWithProtectedDescendant { path, descendant } =
        &error
    {
        return SyncError::CannotReplaceDirectoryWithProtectedDescendant {
            path: path.as_path().to_path_buf(),
            descendant: descendant.as_path().to_path_buf(),
        };
    }
    map_io(error)
}

pub(crate) fn map_io(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> SyncError {
    SyncError::Io(std::io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use sy::engine::delete_plan::DeleteLimit;
    use sy::engine::domain::Timestamp;

    fn test_config() -> SyncConfig {
        let mut config = SyncConfig::test_default();
        config.max_concurrent = 2;
        config
    }

    fn sized_file_entry(value: &str, size: u64) -> Entry {
        let mut entry = Entry::file(
            RelativePath::new(PathBuf::from(value)).expect("valid path"),
            size,
            Timestamp::UNIX_EPOCH,
        );
        entry.unix_mode = Some(0o644);
        entry
    }

    fn file_entry(value: &str) -> Entry {
        sized_file_entry(value, 1)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn observed_hash_keeps_original_source_root_despite_identical_aliases() {
        use futures::StreamExt;
        use sy::endpoint::source_root::SourceRoot;
        use sy::rooted_fs::RootedFsError;
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("source");
        let replacement = parent.path().join("replacement");
        let held = parent.path().join("held");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(original.join("file"), b"same inode and bytes").unwrap();
        std::fs::hard_link(original.join("file"), replacement.join("file")).unwrap();
        let source = SourceRoot::open(original.clone()).await.unwrap();
        let endpoint = source.endpoint();
        let entries: Vec<_> = source.entries(Default::default()).collect().await;
        let entry = entries.into_iter().next().unwrap().unwrap();
        std::fs::rename(&original, &held).unwrap();
        std::fs::rename(&replacement, &original).unwrap();
        for follow in [false, true] {
            let error = observed_hash(&endpoint, &entry, follow).await.unwrap_err();
            let SyncError::Io(error) = error else {
                panic!("unexpected hash failure: {error:?}")
            };
            let cause = error.get_ref().unwrap();
            assert!(
                matches!(
                    cause.downcast_ref::<RootedFsError>(),
                    Some(RootedFsError::RootChanged(_))
                ) || matches!(
                    cause.downcast_ref::<sy::endpoint::existing::ExistingDestinationError>(),
                    Some(sy::endpoint::existing::ExistingDestinationError::Rooted(
                        RootedFsError::RootChanged(_)
                    ))
                )
            );
        }
        for path in [original.join("file"), held.join("file")] {
            assert_eq!(std::fs::read(&path).unwrap(), b"same inode and bytes");
            assert_eq!(
                crate::endpoint::local_identity::metadata_identity(
                    &std::fs::metadata(path).unwrap(),
                    EntryKind::File
                ),
                entry.identity
            );
        }
    }

    #[test]
    fn supported_policy_maps_to_comparison_policy() {
        let mut config = test_config();
        config.comparison.size_only = true;
        config.comparison.update_only = true;
        config.preserve.permissions = true;
        config.preserve.times = true;

        let policy = comparison_policy(
            &config,
            sy::engine::namespace::NamespaceSemantics::BYTE_EXACT,
        );
        assert_eq!(policy.mode, ComparisonMode::SizeOnly);
        assert!(policy.update_only);
        assert!(policy.preserve_permissions);
        assert!(policy.preserve_times);
    }

    #[test]
    fn absolute_delete_limit_maps_to_policy() {
        let mut config = test_config();
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Count(1),
            force: false,
        };
        assert_eq!(
            delete_policy(&config.delete),
            Some(DeletePolicy {
                limit: DeleteLimit::Count(1),
                force: false,
            })
        );
    }

    #[test]
    fn dirs_selection_uses_shallow_source_and_complete_destination_scan() {
        let mut config = test_config();
        config.dirs = true;
        let scan_options = ScanOptions {
            dirs_only: true,
            ..ScanOptions::default()
        };

        assert_eq!(
            source_scan_request(&config, scan_options).max_depth,
            Some(1)
        );
        assert_eq!(destination_scan_request(&config).max_depth, None);
    }

    #[test]
    fn exclude_vcs_selection_maps_to_policy() {
        assert!(!entry_in_vcs_scope(&file_entry(".git/config"), false));
        assert!(entry_in_vcs_scope(&file_entry("src/.gitkeep"), false));
    }

    #[test]
    fn size_selection_maps_to_policy() {
        let mut config = test_config();
        config.min_size = Some(3);
        config.max_size = Some(10);

        assert!(!entry_in_size_scope(
            &sized_file_entry("small", 2),
            config.min_size,
            config.max_size
        ));
        assert!(entry_in_size_scope(
            &sized_file_entry("kept", 5),
            config.min_size,
            config.max_size
        ));
        assert!(!entry_in_size_scope(
            &sized_file_entry("large", 11),
            config.min_size,
            config.max_size
        ));
    }

    #[test]
    fn checksum_comparison_maps_to_policy() {
        let mut config = test_config();
        config.comparison.checksum = true;

        assert_eq!(
            comparison_policy(
                &config,
                sy::engine::namespace::NamespaceSemantics::BYTE_EXACT
            )
            .mode,
            ComparisonMode::Checksum
        );
    }

    #[test]
    fn existing_only_selection_maps_to_policy() {
        let mut config = test_config();
        config.existing = true;

        assert!(
            comparison_policy(
                &config,
                sy::engine::namespace::NamespaceSemantics::BYTE_EXACT
            )
            .existing_only
        );
    }
}
