use super::policy::*;
use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::scanner::ScanOptions;
use crate::sync::{SyncConfig, SyncStats};
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;
use sy::endpoint::local_entry_scan::local_entry_stream;
use sy::engine::controller::{
    preflight_sync_scoped, preflight_sync_scoped_with_content, preview_sync, SyncController,
};
use sy::engine::namespace::NamespaceSemantics;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::protocol::Operation;
use sy::remote::hash::{hash_rooted_file, RemoteHashError};
use sy::remote::push::{RemoteBackupPlan, RemotePushExecutor};
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteHandle;
use sy::remote::ssh::{SshLaunchOptions, SshRemoteSession};
use sy::rooted_fs::RootedFs;
use sy::transfer::delta::BasisIndexLimits;

pub(super) async fn run(
    source_root: &Path,
    destination_root: &Path,
    host: &str,
    user: &Option<String>,
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Result<SyncStats> {
    // Refuse before connecting or allocating controller/session journals.
    sy::endpoint::local_entry_scan::validate_scratch_location(source_root.to_path_buf())
        .await
        .map_err(map_io)?;
    let started = Instant::now();
    // OpenSSH resolves this alias with the user's own ssh_config; only an
    // explicit `user@` from the command line overrides it.
    let target = sy::ssh::SshTarget::new(host, user.clone());
    let router_config = RouterConfig {
        // --bwlimit paces outbound file-content bytes in the router writer; all
        // other frame kinds bypass the limiter so control traffic never waits
        // behind bulk data.
        outbound_payload_limit: config.bwlimit,
        // --timeout aborts the session when no inbound frame arrives within
        // the configured window (rsync I/O timeout semantics).
        idle_timeout: config.timeout.map(std::time::Duration::from_secs),
        ..RouterConfig::default()
    };
    let launch = SshLaunchOptions {
        // --contimeout bounds the OpenSSH connection establishment.
        connect_timeout: config.contimeout,
    };
    let session = SshRemoteSession::connect_with_options(
        &target,
        Operation::Push,
        destination_root,
        router_config,
        launch,
    )
    .await
    .map_err(map_io)?;
    let remote = session.remote().request_handle();
    let destination_root = destination_root.to_path_buf();
    let mut stats =
        execute_with_handle(source_root, &destination_root, remote, config, scan_options).await?;
    stats.duration = started.elapsed();
    Ok(stats)
}

async fn execute_with_handle(
    source_root: &Path,
    destination_root: &Path,
    remote: ClientRemoteHandle,
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Result<SyncStats> {
    let reporter = std::sync::Arc::new(sy::sync::output::SyncReporter::new(
        config.itemize_changes,
        config.json,
        config.quiet,
        config.perf,
    ));
    let scan_started = std::time::Instant::now();
    let source_request = source_scan_request(config, scan_options);
    let destination = remote
        .scan(destination_scan_request(config))
        .await
        .map_err(map_io)?;
    reporter.start(source_root, destination_root);
    let source = filtered_source_stream(
        local_entry_stream(source_root.to_path_buf(), source_request),
        config.filter_engine.clone(),
    );
    let min_size = config.min_size;
    let max_size = config.max_size;
    let skip_symlinks = config.preserve.symlink_mode == SymlinkMode::Skip;
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
    // Preflight follows the destination root's negotiated name semantics; a
    // 3.0 peer (no root-scoped answer) falls back to the OS approximation.
    let namespace_semantics = remote
        .namespace_semantics()
        .unwrap_or_else(|| NamespaceSemantics::for_platform(remote.peer_platform()));
    let plan = if config.comparison.checksum {
        let source_rooted = RootedFs::open(source_root.to_path_buf())
            .await
            .map_err(map_io)?;
        let hash_remote = remote.clone();
        preflight_sync_scoped_with_content(
            source,
            destination,
            comparison_policy(config, namespace_semantics),
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
        preflight_sync_scoped(
            source,
            destination,
            comparison_policy(config, namespace_semantics),
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
        .map_err(map_controller_error)?
    };

    if config.dry_run {
        let diff_mode = config.diff_mode;
        let preview = preview_sync(plan, |item| {
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
    // --backup on v3: all backup locations must stay beneath the pinned
    // destination root. An absolute --backup-dir cannot be honored without
    // breaking root confinement, so it is rejected instead of silently
    // relocated.
    let backup_plan = if config.backup.is_some() {
        let dir = match &config.backup_dir {
            Some(dir) => {
                if dir.is_absolute() {
                    return Err(SyncError::Config(format!(
                        "--backup-dir must be relative to the remote destination root for v3 push, got {}",
                        dir.display()
                    )));
                }
                Some(sy::engine::domain::RelativePath::new(dir.clone()).map_err(map_io)?)
            }
            None => None,
        };
        Some(RemoteBackupPlan {
            suffix: config.suffix.clone(),
            dir,
        })
    } else {
        None
    };
    let executor = RemotePushExecutor::new(
        source_root.to_path_buf(),
        remote,
        scheduler,
        BasisIndexLimits::default(),
    )
    .with_remove_source_files(config.remove_source_files)
    .with_backup(backup_plan)
    .with_compression(compression_policy(config))
    .with_hardlinks(config.preserve.hardlinks)
    .with_xattrs(config.preserve.xattrs)
    .with_acls(config.preserve.acls)
    .with_bsd_flags(config.preserve.flags)
    .with_reporter(Some(reporter.clone()));
    let scan_elapsed = scan_started.elapsed();
    let transfer_started = std::time::Instant::now();
    let summary = SyncController::new(executor, max_in_flight)
        .execute(plan)
        .await
        .map_err(map_controller_error)?;
    let mut stats = summary_stats(summary)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::CompressionDetection;
    use crate::filter::FilterEngine;
    use crate::sync::DeleteMode;
    use futures::stream;
    use std::path::PathBuf;
    use sy::engine::controller::{preflight_sync, ControllerError};
    use sy::engine::delete_plan::{DeleteLimit, DeletePolicy};
    use sy::engine::domain::{Entry, RelativePath, Timestamp};
    use sy::engine::planner::ComparisonPolicy;
    use sy::engine::reconcile::{BoxError, EngineError, EntryStream, Side};
    use sy::remote::runtime::{ClientRemoteSession, IncomingRequest, ServerRemoteSession};
    use tempfile::TempDir;

    fn supported_config() -> SyncConfig {
        let mut config = SyncConfig::test_default();
        config.max_concurrent = 2;
        config.verification.mode = crate::integrity::ChecksumType::None;
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

    /// --remove-source-files over v3: transferred files move; quick-check
    /// unchanged files stay at source because quick-check parity alone cannot
    /// authorize source removal without strong content verification.
    /// The destination keeps every byte and directories stay.
    #[tokio::test]
    async fn remove_source_files_moves_committed_and_unchanged_entries() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::create_dir(source_root.path().join("keep-dir")).unwrap();
        std::fs::write(source_root.path().join("keep-dir/inner"), b"inner").unwrap();
        std::fs::write(source_root.path().join("created"), b"new").unwrap();
        // Different lengths force an Update under the quick check on any
        // filesystem mtime granularity.
        std::fs::write(source_root.path().join("updated"), b"updated-new-value").unwrap();
        // Unchanged file: identical size and mtime means quick comparison skips.
        let unchanged = source_root.path().join("unchanged");
        std::fs::write(&unchanged, b"same").unwrap();
        let dest_unchanged = destination_root.path().join("unchanged");
        std::fs::write(&dest_unchanged, b"same").unwrap();
        let metadata = std::fs::metadata(&unchanged).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&metadata);
        filetime::set_file_times(&dest_unchanged, mtime, mtime).unwrap();
        std::fs::write(destination_root.path().join("updated"), b"old").unwrap();

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
            // Destination scan, directory creation, three file transfers,
            // then observed directory finalization.
            for _ in 0..6 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        session.mutation_handler().serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Metadata(incoming) => {
                        session.metadata_handler().serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected remove-source v3 adapter request"),
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
        config.remove_source_files = true;
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 2, "created + inner");
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_skipped, 1);
        // Every moved byte is present at the destination.
        assert_eq!(
            std::fs::read(destination_root.path().join("created")).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("updated")).unwrap(),
            b"updated-new-value"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("unchanged")).unwrap(),
            b"same"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("keep-dir/inner")).unwrap(),
            b"inner"
        );
        // Transferred sources moved; quick-check unchanged file kept; empty directory remains.
        assert!(!source_root.path().join("created").exists());
        assert!(!source_root.path().join("updated").exists());
        assert!(
            source_root.path().join("unchanged").exists(),
            "quick-check equality alone must not authorize source removal"
        );
        assert!(!source_root.path().join("keep-dir/inner").exists());
        assert!(source_root.path().join("keep-dir").is_dir());
    }

    /// --remove-source-files with --checksum: unchanged files WITH verified content
    /// parity produce a VerifiedExistingDestinationReceipt and move.
    #[tokio::test]
    async fn remove_source_files_with_checksum_moves_content_verified_unchanged_entries() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::create_dir(source_root.path().join("keep-dir")).unwrap();
        std::fs::write(source_root.path().join("keep-dir/inner"), b"inner").unwrap();
        std::fs::write(source_root.path().join("created"), b"new").unwrap();
        std::fs::write(source_root.path().join("updated"), b"updated-new-value").unwrap();
        let unchanged = source_root.path().join("unchanged");
        std::fs::write(&unchanged, b"same").unwrap();
        let dest_unchanged = destination_root.path().join("unchanged");
        std::fs::write(&dest_unchanged, b"same").unwrap();
        let metadata = std::fs::metadata(&unchanged).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&metadata);
        filetime::set_file_times(&dest_unchanged, mtime, mtime).unwrap();
        std::fs::write(destination_root.path().join("updated"), b"old").unwrap();

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
            for _ in 0..8 {
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
                    IncomingRequest::File(incoming) => {
                        let file = file.clone();
                        tasks.spawn(async move {
                            file.serve(incoming)
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        });
                    }
                    IncomingRequest::Mutation(incoming) => {
                        let mutation = session.mutation_handler();
                        tasks.spawn(async move {
                            mutation
                                .serve(incoming)
                                .await
                                .map_err(|error| error.to_string())
                        });
                    }
                    IncomingRequest::Metadata(incoming) => {
                        let metadata = session.metadata_handler();
                        tasks.spawn(async move {
                            metadata
                                .serve(incoming)
                                .await
                                .map_err(|error| error.to_string())
                        });
                    }
                    _ => panic!("unexpected remove-source v3 adapter request"),
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
        config.remove_source_files = true;
        config.comparison.checksum = true;
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 2, "created + inner");
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_skipped, 1);

        assert_eq!(
            std::fs::read(destination_root.path().join("created")).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("updated")).unwrap(),
            b"updated-new-value"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("unchanged")).unwrap(),
            b"same"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("keep-dir/inner")).unwrap(),
            b"inner"
        );

        // Sources moved: content-verified unchanged file moved because it had a verified receipt.
        assert!(!source_root.path().join("created").exists());
        assert!(!source_root.path().join("updated").exists());
        assert!(!source_root.path().join("unchanged").exists());
        assert!(!source_root.path().join("keep-dir/inner").exists());
        assert!(source_root.path().join("keep-dir").is_dir());
    }

    /// --backup over v3: replaced and deleted destination files are preserved
    /// through server-side copies beneath the destination root; the suffixed
    /// backup replaces nothing (an existing backup is overwritten) and
    /// directory structure is preserved under --backup-dir.
    #[tokio::test]
    async fn backup_preserves_replaced_and_deleted_files_over_v3() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        // Different lengths force an Update under the quick check even on
        // filesystems whose mtime granularity could match the pair.
        std::fs::write(source_root.path().join("updated"), b"new-value-longer").unwrap();
        std::fs::write(destination_root.path().join("updated"), b"old-value").unwrap();
        std::fs::create_dir(destination_root.path().join("sub")).unwrap();
        std::fs::write(destination_root.path().join("sub/gone"), b"gone-content").unwrap();
        std::fs::write(source_root.path().join("fresh"), b"fresh").unwrap();

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
            // destination scan, backup copy (updated), two file transfers,
            // delete-backup copy, file remove, directory remove. The source
            // scan is local and produces no request.
            for _ in 0..7 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected backup v3 adapter request"),
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
        config.backup = Some("simple".to_string());
        config.delete = DeleteMode::Enabled {
            limit: DeleteLimit::Percentage(100),
            force: false,
        };
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(stats.files_updated, 1);
        // sub/gone plus the sub directory (its removal request is issued;
        // the server tolerates the non-empty survivor case).
        assert_eq!(stats.files_deleted, 2);
        // New destination contents.
        assert_eq!(
            std::fs::read(destination_root.path().join("updated")).unwrap(),
            b"new-value-longer"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("fresh")).unwrap(),
            b"fresh"
        );
        assert!(!destination_root.path().join("sub/gone").exists());
        // Backups preserved the old bytes with the suffix. The directory
        // holding a deletion's backup survives (it is no longer empty).
        assert_eq!(
            std::fs::read(destination_root.path().join("updated~")).unwrap(),
            b"old-value"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("sub/gone~")).unwrap(),
            b"gone-content"
        );
        assert!(destination_root.path().join("sub").is_dir());
    }

    /// -H/--preserve-hardlinks over v3: members of one scanned group share
    /// a single transferred representative; the rest become server-side
    /// links to it. One file's bytes move; both destination paths share one
    /// inode. With --remove-source-files, local source names are removed only
    /// after the whole group has published.
    #[tokio::test]
    async fn hardlink_group_shares_one_transfer_and_removes_sources_over_v3() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("first"), b"shared-bytes").unwrap();
        std::fs::hard_link(
            source_root.path().join("first"),
            source_root.path().join("second"),
        )
        .unwrap();

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
            // Destination scan, one file transfer (the representative),
            // one hardlink mutation. The source scan is local.
            for _ in 0..3 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        mutation.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected hardlink v3 adapter request"),
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
        config.preserve.hardlinks = true;
        config.remove_source_files = true;
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 2);
        assert_eq!(stats.bytes_transferred, b"shared-bytes".len() as u64);
        assert!(!source_root.path().join("first").exists());
        assert!(!source_root.path().join("second").exists());
        assert_eq!(
            std::fs::read(destination_root.path().join("first")).unwrap(),
            b"shared-bytes"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("second")).unwrap(),
            b"shared-bytes"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let first = std::fs::metadata(destination_root.path().join("first")).unwrap();
            let second = std::fs::metadata(destination_root.path().join("second")).unwrap();
            assert_eq!(first.ino(), second.ino());
        }
    }

    /// -X/--preserve-xattrs over v3: the local source's attributes are mirrored
    /// onto the remote destination after each committed mutation, and a second
    /// pass removes destination attributes the source no longer carries.
    #[cfg(unix)]
    #[tokio::test]
    async fn xattrs_are_mirrored_and_stale_values_removed_over_push() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let name = "user.sy-e2e";
        let stale = "user.sy-stale";
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"one").unwrap();
        xattr::set(&source_file, name, b"first").unwrap();
        xattr::set(&source_file, stale, b"gone").unwrap();

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
            // Two passes of destination scan + file transfer each; xattr
            // preservation rides the transfer stream into staging.
            for _ in 0..4 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected xattr v3 adapter request"),
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
        config.preserve.xattrs = true;

        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();
        let destination_file = destination_root.path().join("file");
        assert_eq!(
            xattr::get(&destination_file, name).unwrap(),
            Some(b"first".to_vec())
        );
        assert_eq!(
            xattr::get(&destination_file, stale).unwrap(),
            Some(b"gone".to_vec())
        );

        // Second pass: the source drops the stale attribute and changes
        // content, so the mirror must clear the destination's stale value.
        std::fs::write(&source_file, b"one-two").unwrap();
        xattr::remove(&source_file, stale).unwrap();
        xattr::set(&source_file, name, b"second").unwrap();
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(
            xattr::get(&destination_file, name).unwrap(),
            Some(b"second".to_vec())
        );
        assert!(xattr::get(&destination_file, stale).unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn xattrs_are_reconciled_for_quick_unchanged_files_and_directories_over_push() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let name = "user.sy-unchanged-push";
        let stale = "user.sy-stale-push";
        std::fs::create_dir(source_root.path().join("dir")).unwrap();
        std::fs::write(source_root.path().join("file"), b"same").unwrap();
        xattr::set(source_root.path().join("file"), name, b"source-file").unwrap();
        xattr::set(source_root.path().join("dir"), name, b"source-dir").unwrap();

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
            let xattr = session.xattr_handler();
            for _ in 0..7 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        session.mutation_handler().serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Metadata(incoming) => {
                        session.metadata_handler().serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Xattr(incoming) => xattr.serve(incoming).await.unwrap(),
                    _ => panic!("unexpected unchanged xattr push request"),
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
        config.preserve.xattrs = true;
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        let destination_file = destination_root.path().join("file");
        let destination_dir = destination_root.path().join("dir");
        xattr::set(&destination_file, name, b"wrong").unwrap();
        xattr::set(&destination_file, stale, b"remove").unwrap();
        xattr::remove(source_root.path().join("dir"), name).unwrap();
        xattr::set(&destination_dir, name, b"remove-dir").unwrap();
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(
            xattr::get(&destination_file, name).unwrap(),
            Some(b"source-file".to_vec())
        );
        assert!(xattr::get(&destination_file, stale).unwrap().is_none());
        assert!(xattr::get(&destination_dir, name).unwrap().is_none());
    }

    /// -A/--preserve-acls over v3: the local source's list is mirrored onto
    /// the remote destination after each committed mutation, and dropping the
    /// source list clears the destination.
    #[cfg(all(unix, feature = "acl"))]
    #[tokio::test]
    async fn acls_are_mirrored_and_cleared_over_push() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"one").unwrap();
        // Capture the untouched base list first so the second pass can
        // restore it exactly (empty on macOS, mode entries on Linux).
        let base_text = exacl::to_string(&exacl::getfacl(&source_file, None).unwrap()).unwrap();
        // Plant one named entry on top of the file's current list (on Linux
        // that keeps the required base entries, so one shape works on both
        // data-plane platforms).
        let uid = unsafe { libc::getuid() };
        let mut planted = exacl::getfacl(&source_file, None).unwrap();
        planted.push(exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        exacl::setfacl(&[&source_file], &planted, None).unwrap();
        let first_text = exacl::to_string(&exacl::getfacl(&source_file, None).unwrap()).unwrap();
        assert!(!first_text.is_empty());

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
            // Two passes of destination scan + file transfer each; ACL
            // preservation rides the transfer stream into staging.
            for _ in 0..4 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected acl v3 adapter request"),
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
        config.preserve.acls = true;

        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();
        let destination_file = destination_root.path().join("file");
        let mirrored = exacl::to_string(&exacl::getfacl(&destination_file, None).unwrap()).unwrap();
        assert_eq!(mirrored, first_text);

        // Second pass: the source drops the named entry and changes content,
        // so the mirror must clear the destination back to the base list.
        std::fs::write(&source_file, b"one-two").unwrap();
        let base = exacl::from_str(&base_text).unwrap();
        exacl::setfacl(&[&source_file], &base, None).unwrap();
        let second_text = exacl::to_string(&exacl::getfacl(&source_file, None).unwrap()).unwrap();
        assert_eq!(second_text, base_text);
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        let cleared = exacl::to_string(&exacl::getfacl(&destination_file, None).unwrap()).unwrap();
        assert_eq!(cleared, second_text);
        assert_ne!(cleared, first_text);
    }

    /// -F/--preserve-flags over v3: the local source's flag word is mirrored
    /// onto the remote destination after each committed mutation, and
    /// clearing the source word clears the destination (macOS only; other
    /// platforms refuse `-F` up front).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bsd_flags_are_mirrored_and_cleared_over_push() {
        use std::ffi::CString;
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::ffi::OsStrExt;

        fn set_flags(path: &std::path::Path, flags: u32) {
            let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `cpath` is a live NUL-terminated pathname borrowed for
            // the duration of the call.
            let ret = unsafe { libc::chflags(cpath.as_ptr(), flags) };
            assert_eq!(ret, 0);
        }

        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"one").unwrap();
        // UF_NODUMP is visible in st_flags and inert for the test itself.
        set_flags(&source_file, 0x1);
        assert_eq!(std::fs::metadata(&source_file).unwrap().st_flags(), 0x1);

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
            let flags_handler = session.bsd_flags_handler();
            // Two passes: destination scan + file transfer + flags mirror.
            for _ in 0..6 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::File(incoming) => {
                        file.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::BsdFlags(incoming) => {
                        flags_handler.serve(incoming).await.unwrap();
                    }
                    _ => panic!("unexpected bsd flags v3 adapter request"),
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
        config.preserve.flags = true;

        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();
        let destination_file = destination_root.path().join("file");
        assert_eq!(
            std::fs::metadata(&destination_file).unwrap().st_flags(),
            0x1
        );

        // Second pass: content change plus cleared flags must clear the
        // destination word.
        std::fs::write(&source_file, b"one-two").unwrap();
        set_flags(&source_file, 0);
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(std::fs::metadata(&destination_file).unwrap().st_flags(), 0);
    }

    /// -z/--compress transfers compressible file bytes as zstd-compressed
    /// Data chunks; the server decompresses, reconstructs, BLAKE3-verifies,
    /// and commits identical bytes.
    #[tokio::test]
    async fn compressed_transfer_round_trips_over_v3() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        // Highly compressible content spanning several chunks.
        let mut payload = Vec::new();
        for i in 0..4 {
            payload.extend_from_slice(format!("chunk {i}: ").repeat(20_000).as_bytes());
        }
        std::fs::write(source_root.path().join("compressible"), &payload).unwrap();

        // Destination scan, source scan (local), one file transfer.
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
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
                    _ => panic!("unexpected compression v3 adapter request"),
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
        config.compression_detection = CompressionDetection::Always;
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(
            std::fs::read(destination_root.path().join("compressible")).unwrap(),
            payload
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
        let source = EntryStream::new(stream::iter(source_items));
        let destination = EntryStream::new(stream::iter(vec![Ok::<Entry, BoxError>(file_entry(
            "remove",
        ))]));
        let delete_filter = filter.clone();

        let result = preflight_sync(
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
            Err(ControllerError::Reconcile(EngineError::Endpoint {
                side: Side::Source,
                ..
            }))
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
            session.request_handle(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        eprintln!("STATS: {stats:?}");
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
            destination_root.path(),
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
