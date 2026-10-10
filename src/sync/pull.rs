//! Pull execution: SSH source root -> local destination root.
//!
//! Mirrors `push` with the roles inverted. The remote session is opened with
//! `Operation::Pull`, so the server serves scans, content hashes, and
//! whole-file fetches, and the session router rejects every mutation request.
//! The local side owns reconciliation (the shared preflight controller),
//! transactional staging, metadata, and the delete journal.

use super::policy::*;
use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::scanner::ScanOptions;
use crate::sync::{SyncConfig, SyncStats};
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;
use sy::engine::controller::{
    preflight_sync_scoped_with_content, preview_sync, ControllerError, SyncController, SyncSummary,
};
use sy::engine::reconcile::OrderedReconciler;
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::protocol::Operation;
use sy::remote::pull::RemotePullExecutor;
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteHandle;
use sy::remote::ssh::{SshLaunchOptions, SshRemoteSession};

/// Pull-specific option refusals.
///
/// Refuses operations not yet supported over pull (server-side source removal,
/// follow-scans, or delete protection).
pub(super) fn pull_unsupported_reason(config: &SyncConfig) -> Option<&'static str> {
    if config.delete.is_enabled() {
        return Some("pull --delete is not yet implemented (remote ignore-scope delete protection is the remaining design)");
    }
    if config.remove_source_files {
        return Some("pull --remove-source-files is not yet implemented (server-side source removal needs a confined mutation design)");
    }
    if config.preserve.symlink_mode == SymlinkMode::Follow {
        return Some("pull --copy-links is not yet implemented (remote follow-scans need a confined walk design)");
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
    // OpenSSH resolves this alias with the user's own ssh_config; only an
    // explicit `user@` from the command line overrides it.
    let target = sy::ssh::SshTarget::new(host, user.clone());
    let router_config = RouterConfig {
        // --bwlimit paces the pull at the client's staging write (see
        // fetch_file); the router's outbound limiter stays off so control
        // frames never wait behind bulk data in the request direction.
        outbound_payload_limit: None,
        // --timeout aborts the session when no inbound frame arrives within
        // the window (rsync I/O timeout semantics), identical to push.
        idle_timeout: config.timeout.map(std::time::Duration::from_secs),
        ..RouterConfig::default()
    };
    let launch = SshLaunchOptions {
        connect_timeout: config.contimeout,
    };
    // The pull session root is the remote SOURCE root. The server refuses to
    // create a missing pull root, which is the data-safe direction: a typo'd
    // source path fails loudly instead of scanning an empty new tree.
    let session = SshRemoteSession::connect_with_options(
        &target,
        Operation::Pull,
        source_root,
        router_config,
        launch,
    )
    .await
    .map_err(map_io)?;
    let remote = session.remote().request_handle();
    let sender = session.remote().sender();
    // The display source keeps the host qualifier so output surfaces match
    // what the user typed; the scan itself goes through the session root.
    let mut display_source = std::ffi::OsString::from(format!("{host}:"));
    display_source.push(source_root);
    let display_source = std::path::PathBuf::from(display_source);
    let result = execute_with_handle(
        &display_source,
        destination_root,
        remote,
        sender,
        config,
        scan_options,
    )
    .await;
    let mut stats = match result {
        Ok(stats) => {
            session.finish().await.map_err(map_io)?;
            stats
        }
        Err(error) => {
            if let Err(cleanup) = session.abort().await {
                tracing::warn!(%cleanup, "failed to clean up SSH pull");
            }
            return Err(error);
        }
    };
    stats.duration = started.elapsed();
    Ok(stats)
}

async fn execute_with_handle(
    source_root: &Path,
    destination_root: &Path,
    remote: ClientRemoteHandle,
    sender: sy::remote::router::RouterSender,
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
    let executor = RemotePullExecutor::new(
        destination_root.to_path_buf(),
        remote.clone(),
        sender,
        scheduler,
    )
    .with_backup_enabled(config.backup.is_some())
    .with_backup(pull_backup_dir(config, destination_root))
    .with_backup_suffix(config.suffix.clone())
    .with_reporter(Some(reporter.clone()))
    .with_compression(compression_policy(config))
    .with_hardlinks(config.preserve.hardlinks)
    .with_xattrs(config.preserve.xattrs)
    .with_acls(config.preserve.acls)
    .with_bsd_flags(config.preserve.flags)
    .with_rate_limiter(rate_limiter);
    reporter.start(source_root, destination_root);
    // Pin local destination authority before admitting the remote producer.
    // Its complete scan never inherits source selection rules.
    let mut destination = executor
        .destination_entries(destination_scan_request(config), config.dry_run)
        .await
        .map_err(map_io)?;
    let source = match remote.scan(source_scan_request(config, scan_options)).await {
        Ok(source) => source,
        Err(error) => {
            let operation = ControllerError::backend("source scan preparation", error);
            let error = match destination.close().await {
                Ok(()) => operation,
                Err(source) => ControllerError::PreflightDrain {
                    operation: Box::new(operation),
                    drain: sy::engine::reconcile::EngineError::Endpoint {
                        side: sy::engine::reconcile::Side::Destination,
                        source,
                    },
                },
            };
            return Err(map_controller_error(error));
        }
    };
    let source = filtered_source_stream(source, config.filter_engine.clone());
    let min_size = config.min_size;
    let max_size = config.max_size;
    let skip_symlinks = config.preserve.symlink_mode == SymlinkMode::Skip;

    // The remote source walk is no-follow under root confinement, so no
    // follow selection exists; symlinks reconcile by target. Delete stays
    // disabled on v3 pull until the server-side ignore-scope design lands.
    let mut plan = preflight_sync_scoped_with_content(
        OrderedReconciler::new(source, destination),
        comparison_policy(
            config,
            crate::fs_util::namespace_semantics(destination_root),
        ),
        delete_policy(&config.delete),
        move |entry| {
            entry_in_size_scope(entry, min_size, max_size)
                && entry_selected_by_symlink_mode(entry, skip_symlinks)
        },
        |_entry| false,
        |source, destination| {
            let remote = remote.clone();
            let executor = &executor;
            async move {
                // Sequential awaits retain ordinary-error ownership of native
                // hashing work instead of canceling one side of a try_join.
                let source_hash = remote.content_hash(&source).await.map_err(|error| {
                    ControllerError::backend("source content comparison", error)
                })?;
                let destination_hash = executor
                    .destination_content_hash(destination)
                    .await
                    .map_err(|error| {
                        ControllerError::backend("destination content comparison", error)
                    })?;
                Ok(source_hash == destination_hash)
            }
        },
    )
    .await
    .map_err(map_controller_error)?;

    if config.preserve.hardlinks {
        plan.validate_hardlink_bytes(|commitment| {
            let remote = remote.clone();
            let executor = &executor;
            async move {
                match commitment {
                    sy::engine::hardlink_preflight::ByteCommitment::Source(entry) => {
                        remote.content_hash(&entry).await.map_err(|error| {
                            ControllerError::backend("source hardlink byte commitment", error)
                        })
                    }
                    sy::engine::hardlink_preflight::ByteCommitment::Destination(entry) => executor
                        .destination_content_hash(entry)
                        .await
                        .map_err(|error| {
                            ControllerError::backend("destination hardlink byte commitment", error)
                        }),
                }
            }
        })
        .await
        .map_err(map_controller_error)?;
    }

    if config.dry_run {
        let diff_mode = config.diff_mode;
        let preview = preview_sync(plan, |item| {
            if diff_mode {
                emit_diff_line(item);
            }
        })
        .await
        .map_err(map_controller_error)?;
        let mut stats = preview_stats(preview)?;
        stats.duration = scan_started.elapsed();
        finish_report(&reporter, &stats, stats.duration, std::time::Duration::ZERO);
        return Ok(stats);
    }

    let scan_elapsed = scan_started.elapsed();
    let transfer_started = Instant::now();
    let summary: SyncSummary = SyncController::new(executor, max_in_flight)
        .execute(plan)
        .await
        .map_err(map_controller_error)?;
    let mut stats = summary_stats(summary)?;
    stats.duration = scan_elapsed + transfer_started.elapsed();
    finish_report(&reporter, &stats, scan_elapsed, transfer_started.elapsed());
    Ok(stats)
}

/// --backup on v3 pull: the destination is local, so backups follow the
/// local engine's rules — beside the file (suffix) or under a
/// `--backup-dir` (absolute or destination-anchored, tree shape
/// preserved).
fn pull_backup_dir(config: &SyncConfig, destination_root: &Path) -> Option<std::path::PathBuf> {
    config.backup.as_ref()?;
    let dir = config.backup_dir.as_ref()?;
    // The destination is local: an absolute --backup-dir is honored as-is
    // (no root confinement to break, mirroring the local engine's rule);
    // a relative one is anchored to the destination root.
    if dir.is_absolute() {
        Some(dir.clone())
    } else {
        Some(destination_root.join(dir))
    }
}

fn destination_scan_request(config: &SyncConfig) -> ScanRequest {
    ScanRequest {
        // The local destination walk ignores nothing by default: every
        // destination entry must reach reconciliation so --delete (once
        // enabled) and skip semantics see the true tree.
        respect_gitignore: false,
        include_git_dir: true,
        follow_symlinks: false,
        max_depth: None,
        metadata: metadata_request(config),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hardlink_preflight_byte_commitments() {
        super::super::hardlink_tests::assert_byte_commitments(
            |source, destination, config| async move {
                let (client, server) = super::super::hardlink_tests::remote_session(
                    sy::protocol::Operation::Pull,
                    &source,
                )
                .await;
                let result = execute_with_handle(
                    &source,
                    &destination,
                    client.request_handle(),
                    client.sender(),
                    &config,
                    ScanOptions::default(),
                )
                .await;
                super::super::hardlink_tests::finish_session(client, server).await;
                result
            },
        )
        .await;
    }
    use crate::sync::scanner::ScanOptions;
    use sy::protocol::Operation;
    use sy::remote::runtime::{IncomingRequest, ServerRemoteSession};
    use tempfile::TempDir;

    fn supported_config() -> crate::sync::SyncConfig {
        let mut config = crate::sync::SyncConfig::test_default();
        config.max_concurrent = 2;
        config.verification.mode = crate::sync::config::ChecksumType::None;
        config
    }

    /// The full pull pipeline over an in-memory duplex: remote scan + whole-
    /// file fetch streams reconstruct the source tree into the local
    /// destination with staged, verified, atomic commits.
    #[tokio::test]
    async fn pull_executes_real_v3_runtime_and_maps_stats() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("new"), b"new").unwrap();
        std::fs::create_dir(source_root.path().join("sub")).unwrap();
        std::fs::write(source_root.path().join("sub/deep.txt"), b"deep").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            // Scan, two fetches, and observed directory preparation/final read.
            for _ in 0..5 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => {
                        scan.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::FileFetch(incoming) => {
                        sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    IncomingRequest::Metadata(incoming) => {
                        session
                            .metadata_handler()
                            .unwrap()
                            .serve(incoming)
                            .await
                            .unwrap();
                    }
                    _other => panic!("unexpected v3 pull request variant"),
                }
            }
            // Keep transport owned in the join result until local finalization
            // completes. Returning the last response is not session completion.
            session
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let config = supported_config();
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 2);
        assert_eq!(stats.bytes_transferred, 3 + 4);
        assert_eq!(
            std::fs::read(destination_root.path().join("new")).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(destination_root.path().join("sub/deep.txt")).unwrap(),
            b"deep"
        );
    }

    #[tokio::test]
    async fn pull_preserves_a_concurrent_destination_edit() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("file"), b"source content").unwrap();
        std::fs::write(destination_root.path().join("file"), b"old").unwrap();
        let raced_destination = destination_root.path().join("file");
        let server_raced_destination = raced_destination.clone();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::FileFetch(incoming) => {
                        // The client has already opened staging and pinned the
                        // scanned destination expectation before requesting bytes.
                        std::fs::write(&server_raced_destination, b"concurrent edit").unwrap();
                        let result = sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await;
                        assert!(matches!(
                            result,
                            Err(sy::remote::transfer::RemoteTransferError::FetchCancelled { .. })
                        ));
                    }
                    _other => panic!("unexpected v3 pull request variant"),
                }
            }
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let error = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &supported_config(),
            ScanOptions::default(),
        )
        .await
        .unwrap_err();

        server.await.unwrap();
        assert!(error
            .to_string()
            .contains("Destination changed during transfer"));
        assert_eq!(
            std::fs::read(&raced_destination).unwrap(),
            b"concurrent edit"
        );
        assert_eq!(
            std::fs::read_dir(destination_root.path()).unwrap().count(),
            1
        );
    }

    /// -X/--preserve-xattrs over v3 pull: the remote source's attributes are
    /// read through the same held source file as its bytes, then applied to
    /// local staging before commit.
    #[tokio::test]
    async fn xattrs_are_read_from_remote_source_and_mirrored_over_pull() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let name = "user.sy-e2e";
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"pull-bytes").unwrap();
        xattr::set(&source_file, name, b"from-remote").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            // Remote scan, then one fetch carrying both file bytes and the
            // requested xattrs on the same stream.
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::FileFetch(incoming) => {
                        sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    _other => panic!("unexpected v3 pull request variant"),
                }
            }
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.preserve.xattrs = true;
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(
            xattr::get(destination_root.path().join("file"), name).unwrap(),
            Some(b"from-remote".to_vec())
        );
    }

    #[tokio::test]
    async fn xattrs_are_reconciled_for_quick_unchanged_files_and_directories_over_pull() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let name = "user.sy-unchanged-pull";
        let stale = "user.sy-stale-pull";
        std::fs::create_dir(source_root.path().join("dir")).unwrap();
        std::fs::write(source_root.path().join("file"), b"same").unwrap();
        xattr::set(source_root.path().join("file"), name, b"source-file").unwrap();
        xattr::set(source_root.path().join("dir"), name, b"source-dir").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            for _ in 0..7 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::FileFetch(incoming) => {
                        sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    IncomingRequest::Metadata(incoming) => {
                        session
                            .metadata_handler()
                            .unwrap()
                            .serve(incoming)
                            .await
                            .unwrap();
                    }
                    _other => panic!("unexpected unchanged xattr pull request variant"),
                }
            }
            session // Retain transport until local directory finalization.
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.preserve.xattrs = true;
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
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
            session.sender(),
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

    /// -A/--preserve-acls over v3 pull: the remote source's ACL is read
    /// through the same held source file as its bytes, then applied to local
    /// staging before commit.
    #[cfg(all(unix, feature = "acl"))]
    #[tokio::test]
    async fn acls_are_read_from_remote_source_and_mirrored_over_pull() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"pull-bytes").unwrap();
        let uid = unsafe { libc::getuid() };
        let mut planted = exacl::getfacl(&source_file, None).unwrap();
        planted.push(exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        exacl::setfacl(&[&source_file], &planted, None).unwrap();
        let source_text = exacl::to_string(&exacl::getfacl(&source_file, None).unwrap()).unwrap();
        assert!(!source_text.is_empty());

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            // Remote scan, then one fetch carrying both file bytes and the
            // requested ACL on the same stream.
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::FileFetch(incoming) => {
                        sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    _other => panic!("unexpected v3 pull request variant"),
                }
            }
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.preserve.acls = true;
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        let mirrored =
            exacl::to_string(&exacl::getfacl(destination_root.path().join("file"), None).unwrap())
                .unwrap();
        assert_eq!(mirrored, source_text);
    }

    /// -F/--preserve-flags over v3 pull: the remote source's flag word is
    /// read through the pinned root and mirrored onto the local destination
    /// after the staged commit (macOS only).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bsd_flags_are_read_from_remote_source_and_mirrored_over_pull() {
        use std::ffi::CString;
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::ffi::OsStrExt;

        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        let source_file = source_root.path().join("file");
        std::fs::write(&source_file, b"pull-bytes").unwrap();
        let cpath = CString::new(source_file.as_os_str().as_bytes()).unwrap();
        // SAFETY: `cpath` is a live NUL-terminated pathname borrowed for
        // the duration of the call. UF_NODUMP is inert for the test itself.
        assert_eq!(unsafe { libc::chflags(cpath.as_ptr(), 0x1) }, 0);

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            let flags_handler = session.bsd_flags_handler().unwrap();
            // Remote scan, then the flags read and the whole-file fetch:
            // exactly three requests, so an extra round-trip regression
            // cannot hide.
            for _ in 0..3 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::BsdFlags(incoming) => {
                        flags_handler.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::FileFetch(incoming) => {
                        sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    _other => panic!("unexpected v3 pull request variant"),
                }
            }
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.preserve.flags = true;
        execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(
            std::fs::metadata(destination_root.path().join("file"))
                .unwrap()
                .st_flags(),
            0x1
        );
    }

    /// --compress pulls compressed chunks end-to-end: the fetch request sets
    /// the compression bit, the server streams zstd frames, and the staged
    /// bytes still verify against the server-reported digest.
    #[tokio::test]
    async fn compressed_pull_round_trips_over_v3() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        // Repetitive content compresses well; several chunks exercise the
        // per-chunk decision more than a single small block would.
        let content = "sy v3 pull compression round trip\n".repeat(2048);
        std::fs::write(source_root.path().join("bulk.txt"), content.as_bytes()).unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            for _ in 0..2 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                    IncomingRequest::FileFetch(incoming) => {
                        sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    _other => panic!("unexpected v3 pull request variant"),
                }
            }
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.compression_detection = crate::engine::compression::CompressionDetection::Always;
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(
            std::fs::read(destination_root.path().join("bulk.txt")).unwrap(),
            content.as_bytes()
        );
    }

    /// -H/--preserve-hardlinks on pull: one fetch moves the representative's
    /// bytes; the other member links to it locally. The server sees exactly
    /// scan + one fetch + member validation — no second fetch or mutation (the destination is
    /// local, so linking never crosses the wire).
    #[tokio::test]
    async fn hardlink_group_fetches_once_and_links_locally() {
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
        let (complete, completed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            // One scan, one fetch, and validation of the later member.
            for _ in 0..3 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => {
                        scan.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::FileFetch(incoming) => {
                        sy::remote::fetch::serve_incoming_file_fetch(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    IncomingRequest::Hash(incoming) => {
                        sy::remote::hash::serve_incoming_hash_rooted(
                            rooted.clone(),
                            incoming,
                            &sender,
                            peer,
                        )
                        .await
                        .unwrap();
                    }
                    _other => panic!("unexpected v3 pull request variant"),
                }
            }
            // Closing transport before the admitted local link completes is
            // cancellation, not a successful remote-session lifecycle.
            completed.await.unwrap();
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut config = supported_config();
        config.preserve.hardlinks = true;
        let stats = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &config,
            ScanOptions::default(),
        )
        .await
        .unwrap();

        complete.send(()).unwrap();
        server.await.unwrap();
        assert_eq!(stats.files_created, 2);
        assert_eq!(stats.bytes_transferred, b"shared-bytes".len() as u64);
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

    /// Every CLI-accepted pull maps onto v3 without legacy fallback; the
    /// remaining refusals (delete, remove-source, copy-links, preservation)
    /// are rejected at validation before a connection opens, so the legacy
    /// stack is unreachable from the CLI pull surface.
    #[test]
    fn accepted_pull_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        assert_eq!(pull_unsupported_reason(&config), None);
        config.itemize_changes = true;
        config.json = true;
        config.perf = true;
        config.backup = Some(String::new());
        config.timeout = Some(30);
        assert_eq!(pull_unsupported_reason(&config), None);

        // -X/-A/-F route to v3; only pull-specific refusals remain.
        config.preserve.xattrs = true;
        assert_eq!(pull_unsupported_reason(&config), None);
        config.preserve.acls = true;
        assert_eq!(pull_unsupported_reason(&config), None);
        config.preserve.acls = false;
        config.preserve.flags = true;
        assert_eq!(pull_unsupported_reason(&config), None);
        config.preserve.flags = false;

        // The refused cluster keeps its reasons for defense-in-depth: if
        // validation ever leaks one through, the session still refuses.
        config.delete = crate::sync::DeleteMode::Enabled {
            limit: sy::engine::delete_plan::DeleteLimit::Unlimited,
            force: false,
        };
        assert!(pull_unsupported_reason(&config).is_some());
    }

    /// A source modified between the scan and the fetch must fail loudly:
    /// the server validates the scanned identity before streaming, so mixed
    /// bytes can never commit. The pull aborts rather than staging garbage.
    #[tokio::test]
    async fn changed_source_fails_fetch_instead_of_committing() {
        let source_root = TempDir::new().unwrap();
        let destination_root = TempDir::new().unwrap();
        std::fs::write(source_root.path().join("raced"), b"before-scan").unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let source_root_path = source_root.path().to_path_buf();
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, Default::default())
                    .await
                    .unwrap();
            let scan = session.scan_handler();
            let rooted = session.scan_handler_rooted().unwrap();
            let sender = session.sender();
            let peer = session.client().platform.os;
            match session.next_request().await.unwrap().unwrap() {
                IncomingRequest::Scan(incoming) => scan.serve(incoming).await.unwrap(),
                _other => panic!("unexpected v3 pull request variant"),
            }
            // Replace the source after the scan: the next fetch must be
            // rejected with SourceChanged by identity validation.
            std::fs::write(source_root_path.join("raced"), b"after-scan-contents").unwrap();
            match session.next_request().await.unwrap().unwrap() {
                IncomingRequest::FileFetch(incoming) => {
                    let error = sy::remote::fetch::serve_incoming_file_fetch(
                        rooted.clone(),
                        incoming,
                        &sender,
                        peer,
                    )
                    .await
                    .unwrap_err();
                    assert!(
                        error.to_string().contains("changed since scan"),
                        "expected SourceChanged, got: {error}"
                    );
                }
                _other => panic!("unexpected v3 pull request variant"),
            }
        });

        let session = sy::remote::runtime::ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let config = supported_config();
        let result = execute_with_handle(
            source_root.path(),
            destination_root.path(),
            session.request_handle(),
            session.sender(),
            &config,
            ScanOptions::default(),
        )
        .await;

        assert!(result.is_err(), "pull must abort on changed source");
        // Nothing committed: the destination stays empty.
        assert!(std::fs::read_dir(destination_root.path())
            .unwrap()
            .next()
            .is_none());
        server.await.unwrap();
    }
}
