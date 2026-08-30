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
use sy::engine::domain::{Entry, RelativePath};
use sy::engine::planner::{ComparisonMode, ComparisonPolicy};
use sy::engine::reconcile::EntryStream;
use sy::engine::scan::{EntryMetadataRequest, ScanRequest};
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::protocol::Operation;
use sy::remote::push::RemotePushExecutor;
use sy::remote::push_controller::{
    preflight_remote_push, RemotePushController, RemotePushControllerError, RemotePushSummary,
};
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteHandle;
use sy::remote::ssh::SshRemoteSession;
use sy::transfer::delta::BasisIndexLimits;

pub(super) fn legacy_fallback_reason(
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Option<&'static str> {
    if config.dry_run || config.diff_mode {
        return Some("dry-run/diff semantics are not yet mapped to v3");
    }
    if config.comparison.checksum {
        return Some("checksum comparison is not yet implemented in v3 preflight");
    }
    if config.min_size.is_some() || config.max_size.is_some() {
        return Some("size selection is not yet mapped to the v3 scanner");
    }
    if scan_options.respect_gitignore
        || !scan_options.include_git_dir
        || scan_options.dirs_only
        || config.dirs
    {
        return Some("non-default scan selection is not yet mapped to v3 deletion scope");
    }
    if config.trash {
        return Some("trash deletion is not yet implemented by v3");
    }
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
    if config.existing {
        return Some("existing-only selection is not yet implemented by v3");
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
    if config.delete.is_enabled()
        && config
            .max_delete
            .as_deref()
            .is_some_and(|value| !value.ends_with('%'))
    {
        return Some("absolute delete limits are not yet represented by the v3 delete policy");
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
    let scan_request = scan_request(config, scan_options);
    let destination = remote.scan(scan_request).await.map_err(map_io)?;
    let source = filtered_source_stream(
        local_entry_stream(source_root.to_path_buf(), scan_request),
        config.filter_engine.clone(),
    );
    let delete_filter = config.filter_engine.clone();
    let plan = preflight_remote_push(
        source,
        destination,
        comparison_policy(config),
        delete_policy(&config.delete),
        move |entry| {
            delete_filter.should_include(entry.path.as_path(), entry.is_directory())
        },
    )
    .await
    .map_err(map_controller_error)?;

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

fn scan_request(config: &SyncConfig, scan_options: ScanOptions) -> ScanRequest {
    ScanRequest {
        respect_gitignore: scan_options.respect_gitignore,
        include_git_dir: scan_options.include_git_dir,
        max_depth: scan_options.dirs_only.then_some(1),
        metadata: EntryMetadataRequest {
            unix_mode: true,
            symlink_target: true,
            identity: true,
            hardlink_group: config.preserve.hardlinks,
        },
    }
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
        update_only: config.comparison.update_only,
        preserve_permissions: config.preserve.permissions,
        preserve_times: config.preserve.times,
    }
}

fn delete_policy(mode: &DeleteMode) -> Option<DeletePolicy> {
    match mode {
        DeleteMode::Disabled => None,
        DeleteMode::Enabled { threshold, force } => Some(DeletePolicy {
            threshold: *threshold,
            force: *force,
        }),
    }
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
    use sy::engine::domain::Timestamp;
    use sy::engine::reconcile::{BoxError, EngineError, Side};
    use sy::remote::runtime::{ClientRemoteSession, IncomingRequest, ServerRemoteSession};
    use tempfile::TempDir;

    fn supported_config() -> SyncConfig {
        let mut config = SyncConfig::test_default();
        config.max_concurrent = 2;
        config.max_errors = 100;
        config.verification.mode = ChecksumType::None;
        config
    }

    fn file_entry(value: &str) -> Entry {
        let mut entry = Entry::file(
            RelativePath::new(PathBuf::from(value)).unwrap(),
            1,
            Timestamp::UNIX_EPOCH,
        );
        entry.unix_mode = Some(0o644);
        entry
    }

    #[test]
    fn supported_policy_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.comparison.size_only = true;
        config.comparison.update_only = true;
        config.preserve.permissions = true;
        config.preserve.times = true;
        assert_eq!(
            legacy_fallback_reason(&config, ScanOptions::default()),
            None
        );

        let policy = comparison_policy(&config);
        assert_eq!(policy.mode, ComparisonMode::SizeOnly);
        assert!(policy.update_only);
        assert!(policy.preserve_permissions);
        assert!(policy.preserve_times);
    }

    #[test]
    fn filter_selection_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        config.filter_engine.add_exclude("*.tmp").unwrap();
        assert_eq!(
            legacy_fallback_reason(&config, ScanOptions::default()),
            None
        );
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
                threshold: 100,
                force: false,
            }),
            move |entry| {
                delete_filter.should_include(entry.path.as_path(), entry.is_directory())
            },
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
            threshold: 100,
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
}
