use crate::cli::SymlinkMode;
use crate::compress::CompressionDetection;
use crate::error::{Result, SyncError};
use crate::filter::FilterEngine;
use crate::integrity::ChecksumType;
use crate::sync::scanner::ScanOptions;
use crate::sync::{DeleteMode, SyncConfig, SyncStats};
use futures::{future, StreamExt};
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;
use sy::endpoint::local_entry_scan::local_entry_stream;
use sy::engine::delete_plan::{DeletePlanError, DeletePolicy};
use sy::engine::domain::{Entry, EntryKind, RelativePath, SyncOp};
use sy::engine::planner::{ComparisonMode, ComparisonPolicy};
use sy::engine::reconcile::EntryStream;
use sy::engine::scan::{EntryMetadataRequest, ScanRequest};
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::protocol::Operation;
use sy::remote::hash::{hash_rooted_file, RemoteHashError};
use sy::remote::push::RemotePushExecutor;
use sy::remote::push_controller::{
    preflight_remote_push_scoped, preflight_remote_push_scoped_with_content, preview_remote_push,
    PreviewOp, RemotePushController, RemotePushControllerError, RemotePushPreview,
    RemotePushSummary,
};
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteHandle;
use sy::remote::ssh::SshRemoteSession;
use sy::rooted_fs::RootedFs;
use sy::transfer::delta::BasisIndexLimits;

pub(super) fn legacy_fallback_reason(config: &SyncConfig) -> Option<&'static str> {
    if config.bwlimit.is_some() {
        return Some("bandwidth limiting is not yet scheduler-integrated in v3");
    }
    if config.resume.only {
        return Some("resume-only control flow is not implemented by v3");
    }
    if config.cache {
        return Some("legacy directory-cache semantics are not part of the v3 adapter");
    }
    if config.verification.mode != ChecksumType::None
        || config.verification.verify_on_write
        || config.verification.checksum_db
        || config.verification.clear_checksum_db
        || config.verification.prune_checksum_db
    {
        return Some("legacy verification/checksum-database options are not mapped to v3");
    }
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
    if config.preserve.symlink_mode != SymlinkMode::Preserve {
        return Some("non-preserving symlink modes are not yet mapped to v3");
    }
    if config.remove_source_files {
        return Some("post-success source removal is not yet implemented by v3");
    }
    if config.backup.is_some() || config.backup_dir.is_some() {
        return Some("backup semantics are not yet implemented by v3");
    }
    if config.partial.is_some() || config.partial_dir.is_some() {
        return Some("legacy partial-file semantics are not mapped to transactional v3 writes");
    }
    if config.timeout.is_some() || config.contimeout.is_some() {
        return Some("CLI timeout overrides are not yet mapped to the v3 OpenSSH session");
    }
    if config.compress_level.is_some()
        || !matches!(config.compression_detection, CompressionDetection::Never)
    {
        return Some("compression policy is not yet integrated with the v3 transfer path");
    }
    if config.itemize_changes {
        return Some("itemized change output is not yet emitted from v3 plans");
    }
    if config.progress {
        return Some("legacy progress reporting is not yet mapped to v3 streams");
    }
    if config.json {
        return Some("legacy JSON event output is not yet emitted by the v3 adapter");
    }
    if config.perf {
        return Some("legacy performance metrics are not yet mapped to the v3 adapter");
    }
    if config.max_errors != 100 {
        return Some("custom max-error policy is not yet mapped to v3 fail-fast execution");
    }
    None
}

pub(super) async fn run(
    source_root: &Path,
    destination_root: &Path,
    host: &str,
    user: &Option<String>,
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Result<SyncStats> {
    let started = Instant::now();
    let ssh_config = resolve_v3_ssh_config(host, user)?;
    let session = SshRemoteSession::connect(
        &ssh_config,
        Operation::Push,
        destination_root,
        RouterConfig::default(),
    )
    .await
    .map_err(map_io)?;
    let remote = session.remote().request_handle();
    let mut stats = execute_with_handle(source_root, remote, config, scan_options).await?;
    stats.duration = started.elapsed();
    Ok(stats)
}

fn resolve_v3_ssh_config(host: &str, user: &Option<String>) -> Result<sy::ssh::config::SshConfig> {
    if let Some(user) = user {
        Ok(sy::ssh::config::SshConfig {
            hostname: host.to_string(),
            user: user.clone(),
            ..Default::default()
        })
    } else {
        sy::ssh::config::parse_ssh_config(host)
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))
    }
}

struct SourceFilterSelection {
    filter: FilterEngine,
    excluded_subtree: Option<RelativePath>,
}

impl SourceFilterSelection {
    fn new(filter: FilterEngine) -> Self {
        Self {
            filter,
            excluded_subtree: None,
        }
    }

    fn includes(&mut self, entry: &Entry) -> bool {
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

fn filtered_source_stream(source: EntryStream, filter: FilterEngine) -> EntryStream {
    if filter.is_empty() {
        return source;
    }

    let mut selection = SourceFilterSelection::new(filter);
    Box::pin(source.filter_map(move |item| {
        let keep = match item.as_ref() {
            Ok(entry) => selection.includes(entry),
            Err(_) => true,
        };
        future::ready(keep.then_some(item))
    }))
}

async fn execute_with_handle(
    source_root: &Path,
    remote: ClientRemoteHandle,
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Result<SyncStats> {
    let source_request = source_scan_request(config, scan_options);
    let destination = remote
        .scan(destination_scan_request(config))
        .await
        .map_err(map_io)?;
    let source = filtered_source_stream(
        local_entry_stream(source_root.to_path_buf(), source_request),
        config.filter_engine.clone(),
    );
    let min_size = config.min_size;
    let max_size = config.max_size;
    let delete_filter = config.filter_engine.clone();
    let max_depth = selection_max_depth(config, scan_options);
    let include_git_dir = scan_options.include_git_dir;
    // Source-derived ignore scope: destination-only paths the source rules
    // would ignore are protected from deletion instead of being filtered
    // out of the destination scan (see `engine::ignore_scope`).
    let ignore_scope = std::sync::Arc::new(std::sync::Mutex::new(
        sy::engine::ignore_scope::SourceIgnoreScope::new(
            source_root,
            scan_options.respect_gitignore,
        ),
    ));
    let plan = if config.comparison.checksum {
        let source_rooted = RootedFs::open(source_root.to_path_buf())
            .await
            .map_err(map_io)?;
        let hash_remote = remote.clone();
        preflight_remote_push_scoped_with_content(
            source,
            destination,
            comparison_policy(config),
            delete_policy(&config.delete),
            move |entry| entry_in_size_scope(entry, min_size, max_size),
            move |entry| {
                delete_filter.should_include(entry.path.as_path(), entry.is_directory())
                    && entry_in_depth_scope(entry, max_depth)
                    && entry_in_vcs_scope(entry, include_git_dir)
                    && entry_not_source_ignored(&ignore_scope, entry)
            },
            move |source, destination| {
                let source_rooted = source_rooted.clone();
                let hash_remote = hash_remote.clone();
                async move {
                    let source_identity = source
                        .identity
                        .ok_or(RemoteHashError::MissingBasisIdentity)?;
                    let source_hash = hash_rooted_file(
                        source_rooted,
                        source.path.clone(),
                        source.size,
                        source_identity,
                    );
                    let destination_hash = hash_remote.content_hash(&destination);
                    let (source_hash, destination_hash) =
                        tokio::try_join!(source_hash, destination_hash)?;
                    Ok(source_hash == destination_hash)
                }
            },
        )
        .await
        .map_err(map_controller_error)?
    } else {
        preflight_remote_push_scoped(
            source,
            destination,
            comparison_policy(config),
            delete_policy(&config.delete),
            move |entry| entry_in_size_scope(entry, min_size, max_size),
            move |entry| {
                delete_filter.should_include(entry.path.as_path(), entry.is_directory())
                    && entry_in_depth_scope(entry, max_depth)
                    && entry_in_vcs_scope(entry, include_git_dir)
                    && entry_not_source_ignored(&ignore_scope, entry)
            },
        )
        .await
        .map_err(map_controller_error)?
    };

    if config.dry_run {
        let diff_mode = config.diff_mode;
        let preview = preview_remote_push(plan, |item| {
            if diff_mode {
                emit_diff_line(item);
            }
        })
        .await
        .map_err(map_controller_error)?;
        return preview_stats(preview);
    }

    let max_in_flight = NonZeroUsize::new(config.max_concurrent).ok_or_else(|| {
        SyncError::Config("parallel transfer count must be greater than zero".to_string())
    })?;
    let active_files = u32::try_from(config.max_concurrent).map_err(|_| {
        SyncError::Config("parallel transfer count exceeds v3 scheduler range".to_string())
    })?;
    let budget = ResourceBudget {
        active_files,
        ..ResourceBudget::default()
    };
    let scheduler = Scheduler::new(budget).map_err(map_io)?;
    let executor = RemotePushExecutor::new(
        source_root.to_path_buf(),
        remote,
        scheduler,
        BasisIndexLimits::default(),
    );
    let summary = RemotePushController::new(executor, max_in_flight)
        .execute(plan)
        .await
        .map_err(map_controller_error)?;
    summary_stats(summary)
}

fn source_scan_request(config: &SyncConfig, scan_options: ScanOptions) -> ScanRequest {
    ScanRequest {
        respect_gitignore: scan_options.respect_gitignore,
        include_git_dir: scan_options.include_git_dir,
        max_depth: selection_max_depth(config, scan_options),
        metadata: metadata_request(config),
    }
}

fn destination_scan_request(config: &SyncConfig) -> ScanRequest {
    ScanRequest {
        respect_gitignore: false,
        include_git_dir: true,
        max_depth: None,
        metadata: metadata_request(config),
    }
}

fn metadata_request(config: &SyncConfig) -> EntryMetadataRequest {
    EntryMetadataRequest {
        unix_mode: true,
        symlink_target: true,
        identity: true,
        hardlink_group: config.preserve.hardlinks,
    }
}

fn selection_max_depth(config: &SyncConfig, scan_options: ScanOptions) -> Option<usize> {
    (config.dirs || scan_options.dirs_only).then_some(1)
}

fn entry_in_depth_scope(entry: &Entry, max_depth: Option<usize>) -> bool {
    max_depth.is_none_or(|depth| entry.path.as_path().components().count() <= depth)
}

fn entry_in_vcs_scope(entry: &Entry, include_git_dir: bool) -> bool {
    include_git_dir
        || !entry
            .path
            .as_path()
            .components()
            .any(|component| component.as_os_str() == std::ffi::OsStr::new(".git"))
}

/// Destination-only entries the source tree's ignore rules would exclude are
/// out of deletion scope. The lock is uncontended: one preflight task owns
/// the scope; the mutex exists because scope state is cached across calls
/// while the closure is `FnMut`.
fn entry_not_source_ignored(
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

fn entry_in_size_scope(entry: &Entry, min_size: Option<u64>, max_size: Option<u64>) -> bool {
    if entry.is_directory() {
        return true;
    }

    min_size.is_none_or(|min| entry.size >= min) && max_size.is_none_or(|max| entry.size <= max)
}

fn comparison_policy(config: &SyncConfig) -> ComparisonPolicy {
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
    }
}

fn delete_policy(mode: &DeleteMode) -> Option<DeletePolicy> {
    match mode {
        DeleteMode::Disabled => None,
        DeleteMode::Enabled { limit, force } => Some(DeletePolicy {
            limit: *limit,
            force: *force,
        }),
    }
}

/// Emit one `--diff` dry-run detail line for a planned operation or deletion,
/// mirroring the legacy per-file format (`Would create: path (size)`).
///
/// `tracing::info!` keeps the channel consistent with the rest of the sync
/// log: default verbosity hides it (WARN floor), `-v` shows it, and `--quiet`
/// or `--json` silences it at the subscriber.
fn emit_diff_line(item: PreviewOp<'_>) {
    match item {
        PreviewOp::Operation(SyncOp::Create { source }) => match source.kind {
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
        PreviewOp::Operation(SyncOp::Skip { path, .. }) => {
            tracing::info!("Would skip: {}", path)
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

fn preview_stats(preview: RemotePushPreview) -> Result<SyncStats> {
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

fn summary_stats(summary: RemotePushSummary) -> Result<SyncStats> {
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

fn to_usize(value: u64, counter: &'static str) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        SyncError::Config(format!("v3 {counter} counter exceeds platform usize range"))
    })
}

fn map_controller_error(error: RemotePushControllerError) -> SyncError {
    if let RemotePushControllerError::DeletePlan(DeletePlanError::ThresholdExceeded {
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
    if let RemotePushControllerError::DeletePlan(DeletePlanError::CountExceeded {
        delete_candidates,
        limit,
    }) = &error
    {
        return SyncError::DeletionCountExceeded {
            delete_candidates: *delete_candidates,
            limit: *limit,
        };
    }
    map_io(error)
}

fn map_io(error: impl std::fmt::Display) -> SyncError {
    SyncError::Io(std::io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::path::PathBuf;
    use sy::engine::delete_plan::DeleteLimit;
    use sy::engine::domain::Timestamp;
    use sy::engine::reconcile::{BoxError, EngineError, Side};
    use sy::remote::push_controller::preflight_remote_push;
    use sy::remote::runtime::{ClientRemoteSession, IncomingRequest, ServerRemoteSession};
    use tempfile::TempDir;

    fn supported_config() -> SyncConfig {
        let mut config = SyncConfig::test_default();
        config.max_concurrent = 2;
        config.max_errors = 100;
        config.verification.mode = ChecksumType::None;
        config
    }

    fn sized_file_entry(value: &str, size: u64) -> Entry {
        let mut entry = Entry::file(
            RelativePath::new(PathBuf::from(value)).unwrap(),
            size,
            Timestamp::UNIX_EPOCH,
        );
        entry.unix_mode = Some(0o644);
        entry
    }

    fn file_entry(value: &str) -> Entry {
        sized_file_entry(value, 1)
    }

    #[test]
    fn supported_policy_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.comparison.size_only = true;
        config.comparison.update_only = true;
        config.preserve.permissions = true;
        config.preserve.times = true;
        assert_eq!(legacy_fallback_reason(&config), None);

        let policy = comparison_policy(&config);
        assert_eq!(policy.mode, ComparisonMode::SizeOnly);
        assert!(policy.update_only);
        assert!(policy.preserve_permissions);
        assert!(policy.preserve_times);
    }

    #[test]
    fn absolute_delete_limit_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Count(1),
            force: false,
        };
        assert_eq!(legacy_fallback_reason(&config), None);
        assert_eq!(
            delete_policy(&config.delete),
            Some(DeletePolicy {
                limit: DeleteLimit::Count(1),
                force: false,
            })
        );
    }

    #[test]
    fn filter_selection_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.filter_engine.add_exclude("*.tmp").unwrap();
        assert_eq!(legacy_fallback_reason(&config), None);
    }

    #[test]
    fn dirs_selection_uses_shallow_source_and_complete_destination_scan() {
        let mut config = supported_config();
        config.dirs = true;
        let scan_options = ScanOptions {
            dirs_only: true,
            ..ScanOptions::default()
        };

        assert_eq!(legacy_fallback_reason(&config), None);
        assert_eq!(
            source_scan_request(&config, scan_options).max_depth,
            Some(1)
        );
        assert_eq!(destination_scan_request(&config).max_depth, None);
    }

    #[test]
    fn exclude_vcs_selection_maps_to_v3_without_fallback() {
        let config = supported_config();

        assert_eq!(legacy_fallback_reason(&config), None);
        assert!(!entry_in_vcs_scope(&file_entry(".git/config"), false));
        assert!(entry_in_vcs_scope(&file_entry("src/.gitkeep"), false));
    }

    #[test]
    fn size_selection_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.min_size = Some(3);
        config.max_size = Some(10);

        assert_eq!(legacy_fallback_reason(&config), None);
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
    fn dry_run_and_diff_mode_map_to_v3_without_fallback() {
        let mut config = supported_config();
        config.dry_run = true;
        assert_eq!(legacy_fallback_reason(&config), None);

        config.diff_mode = true;
        assert_eq!(legacy_fallback_reason(&config), None);
    }

    #[test]
    fn checksum_comparison_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.comparison.checksum = true;

        assert_eq!(legacy_fallback_reason(&config), None);
        assert_eq!(comparison_policy(&config).mode, ComparisonMode::Checksum);
    }

    #[test]
    fn existing_only_selection_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.existing = true;

        assert_eq!(legacy_fallback_reason(&config), None);
        assert!(comparison_policy(&config).existing_only);
    }

    #[tokio::test]
    async fn filtered_source_error_aborts_preflight_before_delete_plan() {
        let mut filter = FilterEngine::new();
        filter.add_exclude("*.tmp").unwrap();
        let source_items: Vec<std::result::Result<Entry, BoxError>> = vec![
            Ok(file_entry("skip.tmp")),
            Err(Box::new(std::io::Error::other("source scan failed"))),
        ];
        let source: EntryStream = Box::pin(stream::iter(source_items));
        let destination: EntryStream = Box::pin(stream::iter(vec![Ok::<Entry, BoxError>(
            file_entry("remove"),
        )]));
        let delete_filter = filter.clone();

        let result = preflight_remote_push(
            filtered_source_stream(source, filter),
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                limit: DeleteLimit::Percentage(100),
                force: false,
            }),
            move |entry| delete_filter.should_include(entry.path.as_path(), entry.is_directory()),
        )
        .await;

        assert!(matches!(
            result,
            Err(RemotePushControllerError::Reconcile(
                EngineError::Endpoint {
                    side: Side::Source,
                    ..
                }
            ))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn absolute_delete_limit_rejects_before_mutation_requests() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(destination_root.path().join("first"), b"first").unwrap();
        std::fs::write(destination_root.path().join("second"), b"second").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            match session.next_request().await.unwrap().unwrap() {
                IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                _ => panic!("absolute delete limit emitted a mutation-capable request before preflight rejection"),
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Count(1),
            force: false,
        };
        let error = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap_err();

        server.await.unwrap();
        assert!(matches!(
            error,
            SyncError::DeletionCountExceeded {
                delete_candidates: 2,
                limit: 1,
            }
        ));
        assert!(destination_root.path().join("first").exists());
        assert!(destination_root.path().join("second").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn absolute_delete_limit_allows_exact_boundary() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(destination_root.path().join("first"), b"first").unwrap();
        std::fs::write(destination_root.path().join("second"), b"second").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let mutation = session.mutation_handler();
            let mut mutations = 0_usize;
            for _ in 0..3 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::Mutation(incoming) => {
                        mutations += 1;
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected absolute delete limit request"),
                }
            }
            mutations
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Count(2),
            force: false,
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(server.await.unwrap(), 2);
        assert_eq!(stats.files_deleted, 2);
        assert!(!destination_root.path().join("first").exists());
        assert!(!destination_root.path().join("second").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gitignore_scoped_delete_protects_ignored_destination_entries() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        // Source is a repository whose rules ignore build artifacts. `.git`
        // is a worktree pointer file: repository detection sees it, the
        // walker's `.git` name filter excludes it, and no repository
        // internals enter the transfer plan.
        std::fs::create_dir(source_root.path().join(".git")).unwrap();
        std::fs::write(source_root.path().join(".gitignore"), b"*.log\n").unwrap();
        std::fs::write(source_root.path().join("keep.txt"), b"keep").unwrap();

        let ignored_dir = destination_root.path().join("cache");
        std::fs::create_dir(&ignored_dir).unwrap();
        std::fs::write(ignored_dir.join("stale.log"), b"log").unwrap();
        std::fs::write(destination_root.path().join("stray.log"), b"log").unwrap();
        std::fs::write(destination_root.path().join("remove-me.txt"), b"x").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let file = session.file_handler();
            let mutation = session.mutation_handler();
            let mut mutations = 0_usize;
            for _ in 0..4 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        mutations += 1;
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected gitignore-scoped v3 adapter request kind"),
                }
            }
            mutations
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };
        let scan_options = ScanOptions {
            respect_gitignore: true,
            include_git_dir: false,
            ..ScanOptions::default()
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            scan_options,
        )
        .await
        .unwrap();

        // One deletion (remove-me.txt) plus creates (.gitignore, keep.txt).
        assert_eq!(server.await.unwrap(), 1);
        assert_eq!(stats.files_deleted, 1);
        assert!(
            ignored_dir.join("stale.log").exists(),
            "ignored destination subtree must survive deletion"
        );
        assert!(
            destination_root.path().join("stray.log").exists(),
            "ignored destination file must survive deletion"
        );
        assert!(!destination_root.path().join("remove-me.txt").exists());
        assert_eq!(stats.files_created, 2, ".gitignore and keep.txt");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gitignore_delete_limit_denominator_excludes_ignored_entries() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::create_dir(source_root.path().join(".git")).unwrap();
        std::fs::write(source_root.path().join(".gitignore"), b"*.log\n").unwrap();

        // A matched pair (eligible but never a candidate), two ignored
        // strays, and one deletable file. With ignored entries excluded,
        // one candidate over two eligible entries is 50% and passes; without
        // exclusion three candidates over four eligible entries is 75% and
        // the same limit rejects. The matched entry makes the denominator
        // observable.
        std::fs::write(source_root.path().join("keep.txt"), b"keep").unwrap();
        std::fs::write(destination_root.path().join("keep.txt"), b"keep").unwrap();
        // Identical mtimes make the matched pair a quick-compare skip, so
        // the request sequence stays deterministic: scan, create, delete.
        let matched_mtime = std::time::SystemTime::UNIX_EPOCH;
        let times = std::fs::FileTimes::new().set_modified(matched_mtime);
        std::fs::File::options()
            .write(true)
            .open(source_root.path().join("keep.txt"))
            .unwrap()
            .set_times(times)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(destination_root.path().join("keep.txt"))
            .unwrap()
            .set_times(times)
            .unwrap();
        std::fs::write(destination_root.path().join("a.log"), b"a").unwrap();
        std::fs::write(destination_root.path().join("b.log"), b"b").unwrap();
        std::fs::write(destination_root.path().join("c.txt"), b"c").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let file = session.file_handler();
            let mutation = session.mutation_handler();
            let mut mutations = 0_usize;
            for _ in 0..3 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        mutations += 1;
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected gitignore limit v3 adapter request"),
                }
            }
            mutations
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.delete = DeleteMode::Enabled {
            // 50% of 1 eligible candidate: the lone deletable file passes;
            // without ignored-entry exclusion the denominator would be 3
            // and the same deletion would exceed the threshold.
            limit: DeleteLimit::Percentage(50),
            force: false,
        };
        let scan_options = ScanOptions {
            respect_gitignore: true,
            include_git_dir: false,
            ..ScanOptions::default()
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            scan_options,
        )
        .await
        .unwrap();

        // Requests: scan, create .gitignore, delete c.txt.
        assert_eq!(server.await.unwrap(), 1);
        assert_eq!(stats.files_deleted, 1);
        assert!(!destination_root.path().join("c.txt").exists());
        assert!(destination_root.path().join("a.log").exists());
        assert!(destination_root.path().join("b.log").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn filtered_delete_preserves_excluded_subtree_and_removes_in_scope_entry() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let excluded = destination_root.path().join("excluded");
        std::fs::create_dir(&excluded).unwrap();
        std::fs::write(excluded.join("keep"), b"keep").unwrap();
        std::fs::write(destination_root.path().join("remove"), b"remove").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let mutation = session.mutation_handler();
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::Mutation(incoming) => {
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected filtered v3 adapter request"),
                }
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.filter_engine.add_exclude("excluded/").unwrap();
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert!(excluded.join("keep").exists());
        assert!(!destination_root.path().join("remove").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dirs_delete_preserves_deeper_destination_subtree() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let protected = destination_root.path().join("protected");
        std::fs::create_dir(&protected).unwrap();
        std::fs::write(protected.join("keep"), b"keep").unwrap();
        std::fs::write(destination_root.path().join("remove"), b"remove").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let mutation = session.mutation_handler();
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::Mutation(incoming) => {
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected dirs-only v3 adapter request"),
                }
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.dirs = true;
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };
        let scan_options = ScanOptions {
            dirs_only: true,
            ..ScanOptions::default()
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            scan_options,
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert!(protected.join("keep").exists());
        assert!(!destination_root.path().join("remove").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exclude_vcs_delete_preserves_git_subtree() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let git_dir = destination_root.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("config"), b"keep").unwrap();
        std::fs::write(destination_root.path().join("remove"), b"remove").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let mutation = session.mutation_handler();
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::Mutation(incoming) => {
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected exclude-vcs v3 adapter request"),
                }
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };
        let scan_options = ScanOptions {
            include_git_dir: false,
            ..ScanOptions::default()
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            scan_options,
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert!(git_dir.join("config").exists());
        assert!(!destination_root.path().join("remove").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn size_selection_skips_semantic_work_without_hiding_source_from_delete() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("large"), b"hello world!").unwrap();
        std::fs::write(source_root.path().join("small"), b"hi").unwrap();
        std::fs::write(destination_root.path().join("small"), b"OLD!").unwrap();
        std::fs::write(destination_root.path().join("remove"), b"x").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let file = session.file_handler();
            let mutation = session.mutation_handler();
            for _ in 0..3 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected size-filtered v3 adapter request"),
                }
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.min_size = Some(10);
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(
            std::fs::read(destination_root.path().join("large")).unwrap(),
            b"hello world!"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("small")).unwrap(),
            b"OLD!"
        );
        assert!(!destination_root.path().join("remove").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_only_skips_missing_source_entries_and_updates_matches() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("matched"), b"new").unwrap();
        std::fs::write(source_root.path().join("missing"), b"missing").unwrap();
        std::fs::write(destination_root.path().join("matched"), b"older").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let file = session.file_handler();
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected existing-only v3 adapter request"),
                }
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.existing = true;
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 0);
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_skipped, 1);
        assert!(!destination_root.path().join("missing").exists());
        assert_eq!(
            std::fs::read(destination_root.path().join("matched")).unwrap(),
            b"new"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dry_run_previews_remote_push_without_mutation_requests() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("create"), b"create").unwrap();
        std::fs::write(source_root.path().join("update"), b"new-value").unwrap();
        std::fs::write(destination_root.path().join("remove"), b"remove").unwrap();
        std::fs::write(destination_root.path().join("update"), b"old").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            match session.next_request().await.unwrap().unwrap() {
                IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                _ => panic!("dry-run emitted a mutation-capable v3 request"),
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.dry_run = true;
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.bytes_would_add, 6);
        assert_eq!(stats.bytes_would_change, 9);
        assert!(!destination_root.path().join("create").exists());
        assert_eq!(
            std::fs::read(destination_root.path().join("update")).unwrap(),
            b"old"
        );
        assert!(destination_root.path().join("remove").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checksum_mode_hashes_matches_and_transfers_only_changed_content() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("equal"), b"same").unwrap();
        std::fs::write(destination_root.path().join("equal"), b"same").unwrap();
        std::fs::write(source_root.path().join("changed"), b"new!").unwrap();
        std::fs::write(destination_root.path().join("changed"), b"old!").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let hash = session.hash_handler();
            let file = session.file_handler();
            let mut tasks = tokio::task::JoinSet::new();
            let mut hashes = 0;
            let mut files = 0;
            for _ in 0..4 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => {
                        let scan = scan.clone();
                        tasks.spawn(async move {
                            scan.serve(incoming)
                                .await
                                .map_err(|error| error.to_string())
                        });
                    }
                    IncomingRequest::Hash(incoming) => {
                        hashes += 1;
                        let hash = hash.clone();
                        tasks.spawn(async move {
                            hash.serve(incoming)
                                .await
                                .map_err(|error| error.to_string())
                        });
                    }
                    IncomingRequest::File(incoming) => {
                        files += 1;
                        let file = file.clone();
                        tasks.spawn(async move {
                            file.serve(incoming)
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        });
                    }
                    _ => panic!("unexpected checksum v3 adapter request"),
                }
            }
            while let Some(joined) = tasks.join_next().await {
                joined.unwrap().unwrap();
            }
            (hashes, files)
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.comparison.checksum = true;
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(server.await.unwrap(), (2, 1));
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(
            std::fs::read(destination_root.path().join("equal")).unwrap(),
            b"same"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("changed")).unwrap(),
            b"new!"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checksum_dry_run_hashes_but_emits_no_mutation_request() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("changed"), b"new!").unwrap();
        std::fs::write(destination_root.path().join("changed"), b"old!").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let hash = session.hash_handler();
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => {
                        let scan = scan.clone();
                        tasks.spawn(async move {
                            scan.serve(incoming)
                                .await
                                .map_err(|error| error.to_string())
                        });
                    }
                    IncomingRequest::Hash(incoming) => {
                        let hash = hash.clone();
                        tasks.spawn(async move {
                            hash.serve(incoming)
                                .await
                                .map_err(|error| error.to_string())
                        });
                    }
                    _ => panic!("checksum dry-run emitted a mutation-capable request"),
                }
            }
            while let Some(joined) = tasks.join_next().await {
                joined.unwrap().unwrap();
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.comparison.checksum = true;
        config.dry_run = true;
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.bytes_would_change, 4);
        assert_eq!(
            std::fs::read(destination_root.path().join("changed")).unwrap(),
            b"old!"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn adapter_executes_real_v3_runtime_and_maps_stats() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("new"), b"new").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let file = session.file_handler();
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected v3 adapter request"),
                }
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let config = supported_config();
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_created, 1);
        assert_eq!(stats.files_updated, 0);
        assert_eq!(stats.bytes_transferred, 3);
        assert_eq!(
            std::fs::read(destination_root.path().join("new")).unwrap(),
            b"new"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diff_mode_dry_run_emits_per_operation_detail() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("create"), b"create-me").unwrap();
        std::fs::write(source_root.path().join("update"), b"new-value").unwrap();
        std::fs::write(destination_root.path().join("remove"), b"remove-me").unwrap();
        std::fs::write(destination_root.path().join("update"), b"old").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            // Diff-mode dry-run performs the scan preflight only.
            match session.next_request().await.unwrap().unwrap() {
                IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                _ => panic!("diff-mode dry-run must not emit mutation requests"),
            }
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.dry_run = true;
        config.diff_mode = true;
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };

        // Diff detail lines go through tracing::info!; the structured preview
        // stats are asserted directly, and the emitted lines are covered by
        // the CLI dry-run diff end-to-end test where a real subscriber is
        // initialized from CLI flags.
        let stats = execute_with_handle(
            source_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.bytes_would_add, 9);
        assert_eq!(stats.bytes_would_change, 9);
        assert!(!destination_root.path().join("create").exists());
        assert!(destination_root.path().join("remove").exists());
    }
}
