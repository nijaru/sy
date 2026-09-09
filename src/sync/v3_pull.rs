//! v3 pull execution: SSH source root -> local destination root.
//!
//! Mirrors `v3_push` with the roles inverted. The remote session is opened
//! with `Operation::Pull`, so the server serves scans, content hashes, and
//! whole-file fetches, and the session router rejects every mutation request.
//! The local side owns reconciliation (the shared preflight controller),
//! transactional staging, metadata, and the delete journal once deletes land
//! (see `legacy_fallback_reason`).
//!
//! Policy helpers are shared with `v3_push` so both directions make
//! identical decisions for identical CLI flags.

use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::scanner::ScanOptions;
use crate::sync::v3_push::{
    comparison_policy, compression_policy, delete_policy, map_controller_error, map_io,
    preview_stats, source_scan_request, summary_stats,
};
use crate::sync::{SyncConfig, SyncStats};
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Instant;
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::protocol::Operation;
use sy::remote::pull::RemotePullExecutor;
use sy::remote::push_controller::{
    preflight_remote_push_scoped, preview_remote_push, RemotePushController, RemotePushSummary,
};
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteHandle;
use sy::remote::ssh::{SshLaunchOptions, SshRemoteSession};

/// Whether a pull must fall back to the legacy v2 stack.
///
/// The preservation cluster stays deferred to item 6 (executor unification)
/// so it lands once, not twice. Pull-specific refusals (delete,
/// remove-source-files, copy-links) are handled by CLI validation before a
/// connection is opened; this returns None for every pull the CLI accepts.
pub(super) fn legacy_fallback_reason(config: &SyncConfig) -> Option<&'static str> {
    if config.delete.is_enabled() {
        return Some("v3 pull --delete is not yet implemented (remote ignore-scope delete protection is the remaining design)");
    }
    if config.remove_source_files {
        return Some("v3 pull --remove-source-files is not yet implemented (server-side source removal needs a confined mutation design)");
    }
    if config.preserve.symlink_mode == SymlinkMode::Follow {
        return Some("v3 pull --copy-links is not yet implemented (remote follow-scans need a confined walk design)");
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
    None
}

pub(super) async fn run(
    source_root: &str,
    destination_root: &Path,
    host: &str,
    user: &Option<String>,
    config: &SyncConfig,
    scan_options: ScanOptions,
) -> Result<SyncStats> {
    let started = Instant::now();
    let ssh_config = crate::sync::v3_push::resolve_v3_ssh_config(host, user)?;
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
    let source_root_path = std::path::PathBuf::from(source_root);
    let session = SshRemoteSession::connect_with_options(
        &ssh_config,
        Operation::Pull,
        &source_root_path,
        router_config,
        launch,
    )
    .await
    .map_err(map_io)?;
    let remote = session.remote().request_handle();
    let sender = session.remote().sender();
    // The display source keeps the host qualifier so output surfaces match
    // what the user typed; the scan itself goes through the session root.
    let display_source = format!("{host}:{source_root}");
    let mut stats = execute_with_handle(
        &display_source,
        destination_root,
        remote,
        sender,
        config,
        scan_options,
    )
    .await?;
    stats.duration = started.elapsed();
    Ok(stats)
}

async fn execute_with_handle(
    source_root: &str,
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
    // Source scan runs remotely (gitignore honored where the files live);
    // destination scan is local and complete so reconciliation sees every
    // destination entry.
    let source = remote
        .scan(source_scan_request(config, scan_options))
        .await
        .map_err(map_io)?;
    reporter.start(Path::new(source_root), destination_root);
    // A first pull into a fresh directory is the common case; the local
    // engine creates a missing destination root, and the pull must match.
    // Dry-run never mutates, so it reports against the tree as-is.
    if !destination_root.exists() && !config.dry_run {
        std::fs::create_dir_all(destination_root).map_err(map_io)?;
    }
    let destination = sy::endpoint::local_entry_scan::local_entry_stream(
        destination_root.to_path_buf(),
        destination_scan_request(config),
    );

    // The remote source walk is no-follow under root confinement, so no
    // follow selection exists; symlinks reconcile by target. Delete stays
    // disabled on v3 pull until the server-side ignore-scope design lands.
    let plan = preflight_remote_push_scoped(
        source,
        destination,
        comparison_policy(config),
        delete_policy(&config.delete),
        |_entry| true,
        |_entry| false,
    )
    .await
    .map_err(map_controller_error)?;

    if config.dry_run {
        let diff_mode = config.diff_mode;
        let preview = preview_remote_push(plan, |item| {
            if diff_mode {
                crate::sync::v3_push::emit_diff_line(item);
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
    let executor =
        RemotePullExecutor::new(destination_root.to_path_buf(), remote, sender, scheduler)
            .with_backup_enabled(config.backup.is_some())
            .with_backup(pull_backup_dir(config, destination_root))
            .with_backup_suffix(config.suffix.clone())
            .with_reporter(Some(reporter.clone()))
            .with_compression(compression_policy(config))
            .with_rate_limiter(rate_limiter);

    let scan_elapsed = scan_started.elapsed();
    let transfer_started = Instant::now();
    let summary: RemotePushSummary = RemotePushController::new(executor, max_in_flight)
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
        metadata: crate::sync::v3_push::metadata_request(config),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sync::scanner::ScanOptions;
    use sy::protocol::Operation;
    use sy::remote::runtime::{IncomingRequest, ServerRemoteSession};
    use tempfile::TempDir;

    fn supported_config() -> crate::sync::SyncConfig {
        let mut config = crate::sync::SyncConfig::test_default();
        config.max_concurrent = 2;
        config.verification.mode = crate::integrity::ChecksumType::None;
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
            let rooted = session.scan_handler_rooted();
            let sender = session.sender();
            let peer = session.client().platform.os;
            // One scan + two fetches, bounded exactly: the plan transfers
            // both files (the scan request count must stay exact so a
            // protocol regression cannot hide behind extra round-trips).
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
        let config = supported_config();
        let stats = execute_with_handle(
            &source_root.path().to_string_lossy(),
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
            let rooted = session.scan_handler_rooted();
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
        config.compression_detection = crate::compress::CompressionDetection::Always;
        let stats = execute_with_handle(
            &source_root.path().to_string_lossy(),
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

    /// Every CLI-accepted pull maps onto v3 without legacy fallback; the
    /// remaining refusals (delete, remove-source, copy-links, preservation)
    /// are rejected at validation before a connection opens, so the legacy
    /// stack is unreachable from the CLI pull surface.
    #[test]
    fn accepted_pull_maps_to_v3_without_fallback() {
        let mut config = supported_config();
        assert_eq!(legacy_fallback_reason(&config), None);
        config.itemize_changes = true;
        config.json = true;
        config.perf = true;
        config.backup = Some(String::new());
        config.timeout = Some(30);
        assert_eq!(legacy_fallback_reason(&config), None);

        // The refused cluster keeps its reasons for defense-in-depth: if
        // validation ever leaks one through, the session still refuses.
        config.delete = crate::sync::DeleteMode::Enabled {
            limit: sy::engine::delete_plan::DeleteLimit::Unlimited,
            force: false,
        };
        assert!(legacy_fallback_reason(&config).is_some());
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
            let rooted = session.scan_handler_rooted();
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
            &source_root.path().to_string_lossy(),
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
