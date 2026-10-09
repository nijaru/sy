#![cfg(unix)]

use futures::TryStreamExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use sy::endpoint::local_entry_scan::local_entry_stream;
use sy::engine::controller::SyncPlanExecutor;
use sy::engine::domain::{Entry, SyncOp};
use sy::engine::finalize_journal::FinalizeMetadata;
use sy::engine::planner::{
    plan_entry, ComparisonMode, ComparisonPolicy, ExecutionPolicy, PlanDecision,
};
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::protocol::Operation;
use sy::remote::local_executor::{lower_local_op, LocalSyncExecutor};
use sy::remote::pull::RemotePullExecutor;
use sy::remote::pull_lower::lower_pull_op;
use sy::remote::push::{lower_sync_op, RemotePushExecutor};
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteSession;
use sy::transfer::delta::BasisIndexLimits;

#[derive(Clone, Copy, Debug)]
enum Direction {
    Local,
    Push,
    Pull,
}

#[derive(Clone, Copy, Debug)]
enum Substitution {
    None,
    File,
    LeafSymlink,
    AncestorSymlink,
    AncestorDirectory,
}

fn metadata(path: &Path) -> (u32, i64, i64) {
    let meta = std::fs::metadata(path).unwrap();
    (meta.mode() & 0o7777, meta.mtime(), meta.mtime_nsec())
}

fn seed(path: &Path, mode: u32, seconds: i64) {
    std::fs::write(path, b"unchanged bytes").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 123)).unwrap();
}

async fn file_entry(root: &Path) -> Entry {
    let mut request = ScanRequest::default();
    request.metadata.unix_mode = true;
    let mut stream = local_entry_stream(root.to_path_buf(), request);
    while let Some(entry) = stream.try_next().await.unwrap() {
        if entry.is_file() {
            return entry;
        }
    }
    panic!("fixture file missing");
}

fn substitute(root: &Path, outside: &Path, race: Substitution) {
    match race {
        Substitution::None => {}
        Substitution::File => {
            // Keep the scanned inode alive to rule out inode-number reuse.
            std::fs::rename(root.join("dir/file"), root.join("held-file")).unwrap();
            seed(&root.join("dir/file"), 0o644, 1_650_000_000);
        }
        Substitution::LeafSymlink => {
            std::fs::rename(root.join("dir/file"), root.join("held-file")).unwrap();
            std::os::unix::fs::symlink(outside.join("file"), root.join("dir/file")).unwrap();
        }
        Substitution::AncestorSymlink => {
            std::fs::rename(root.join("dir"), root.join("held-dir")).unwrap();
            std::os::unix::fs::symlink(outside, root.join("dir")).unwrap();
        }
        Substitution::AncestorDirectory => {
            std::fs::rename(root.join("dir"), root.join("held-dir")).unwrap();
            std::fs::create_dir(root.join("dir")).unwrap();
            seed(&root.join("dir/file"), 0o644, 1_650_000_000);
        }
    }
}

/// Exercise actual lowerers/executors and the v3 server, not mocked chmod.
/// Same-kind substitutions catch dropped destination tokens; symlink ancestors
/// catch path-based setters even when the leaf itself is still a regular file.
#[test]
fn cross_platform_symlink_modes_are_ignored_without_losing_timestamp_preservation() {
    use sy::engine::domain::{EntryIdentity, RelativePath, Timestamp};
    use sy::remote::local_executor::LocalSyncAction;
    use sy::remote::pull::RemotePullAction;
    use sy::remote::push::RemotePushAction;

    let path = RelativePath::new("link").unwrap();
    let mut source = Entry::symlink(path.clone(), "target".into(), Timestamp::UNIX_EPOCH);
    let mut destination = source.clone();
    // The Mac source and Linux destination can report different link modes
    // even immediately after a successful creation. Neither mode is writable.
    source.unix_mode = Some(0o755);
    destination.unix_mode = Some(0o777);
    destination.identity = Some(EntryIdentity::from_bytes([1; 32]));
    let comparison = ComparisonPolicy {
        preserve_permissions: true,
        preserve_times: true,
        ..ComparisonPolicy::default()
    };
    assert!(matches!(
        plan_entry(
            source.clone(),
            Some(destination.clone()),
            path.clone(),
            comparison
        ),
        PlanDecision::Ready(SyncOp::Unchanged { .. })
    ));
    source.modified = Timestamp::new(2, 0).unwrap();
    let PlanDecision::Ready(operation @ SyncOp::Metadata { .. }) =
        plan_entry(source, Some(destination), path, comparison)
    else {
        panic!("timestamp drift must still request metadata preservation");
    };
    let policy = ExecutionPolicy {
        preserve_permissions: true,
        preserve_times: true,
    };
    let modified = Some(Timestamp::new(2, 0).unwrap());
    assert!(matches!(
        lower_local_op(operation.clone(), policy).unwrap().unwrap().into_action(),
        LocalSyncAction::ApplyMetadata { unix_mode: None, modified: actual, .. } if actual == modified
    ));
    assert!(matches!(
        lower_sync_op(operation.clone(), policy).unwrap().unwrap().into_action(),
        RemotePushAction::ApplyMetadata { unix_mode: None, modified: actual, .. } if actual == modified
    ));
    assert!(matches!(
        lower_pull_op(operation, policy).unwrap().unwrap().into_action(),
        RemotePullAction::ApplyMetadata { unix_mode: None, modified: actual, .. } if actual == modified
    ));
}

#[tokio::test]
async fn metadata_only_mutations_require_the_scanned_destination_in_all_directions() {
    for direction in [Direction::Local, Direction::Push, Direction::Pull] {
        for race in [
            Substitution::None,
            Substitution::File,
            Substitution::LeafSymlink,
            Substitution::AncestorSymlink,
            Substitution::AncestorDirectory,
        ] {
            let source = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::create_dir(source.path().join("dir")).unwrap();
            std::fs::create_dir(destination.path().join("dir")).unwrap();
            seed(&source.path().join("dir/file"), 0o600, 1_600_000_000);
            seed(&destination.path().join("dir/file"), 0o644, 1_650_000_000);
            seed(&outside.path().join("file"), 0o640, 1_700_000_000);
            let outside_before = metadata(&outside.path().join("file"));
            let source_before = metadata(&source.path().join("dir/file"));
            let source_entry = file_entry(source.path()).await;
            let destination_entry = file_entry(destination.path()).await;
            let decision = plan_entry(
                source_entry,
                Some(destination_entry),
                sy::engine::domain::RelativePath::new("dir/file").unwrap(),
                ComparisonPolicy {
                    mode: ComparisonMode::SizeOnly,
                    preserve_permissions: true,
                    preserve_times: true,
                    ..ComparisonPolicy::default()
                },
            );
            let PlanDecision::Ready(op @ SyncOp::Metadata { .. }) = decision else {
                panic!("fixture must reconcile to metadata-only work");
            };
            let policy = ExecutionPolicy {
                preserve_permissions: true,
                preserve_times: true,
            };
            let scheduler = Scheduler::new(ResourceBudget::default()).unwrap();
            let result = match direction {
                Direction::Local => {
                    let executor = LocalSyncExecutor::new(
                        source.path().to_path_buf(),
                        destination.path().to_path_buf(),
                        scheduler,
                    );
                    let work = lower_local_op(op, policy).unwrap().unwrap();
                    substitute(destination.path(), outside.path(), race);
                    executor
                        .execute(work)
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                }
                Direction::Push | Direction::Pull => {
                    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                    let (client_reader, client_writer) = tokio::io::split(client_io);
                    let (server_reader, server_writer) = tokio::io::split(server_io);
                    let server = tokio::spawn(sy::remote::serve::serve_transport(
                        server_reader,
                        server_writer,
                        RouterConfig::default(),
                    ));
                    let (operation, root) = match direction {
                        Direction::Push => (Operation::Push, destination.path()),
                        Direction::Pull => (Operation::Pull, source.path()),
                        Direction::Local => unreachable!(),
                    };
                    let client = ClientRemoteSession::connect(
                        client_reader,
                        client_writer,
                        operation,
                        root,
                        RouterConfig::default(),
                    )
                    .await
                    .unwrap();
                    let result = match direction {
                        Direction::Push => {
                            let executor = RemotePushExecutor::new(
                                source.path().to_path_buf(),
                                client.request_handle(),
                                scheduler,
                                BasisIndexLimits::default(),
                            );
                            let work = lower_sync_op(op, policy).unwrap().unwrap();
                            substitute(destination.path(), outside.path(), race);
                            executor
                                .execute(work)
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        }
                        Direction::Pull => {
                            let executor = RemotePullExecutor::new(
                                destination.path().to_path_buf(),
                                client.request_handle(),
                                client.sender(),
                                scheduler,
                            );
                            let work = lower_pull_op(op, policy).unwrap().unwrap();
                            substitute(destination.path(), outside.path(), race);
                            executor
                                .execute(work)
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        }
                        Direction::Local => unreachable!(),
                    };
                    server.abort();
                    let _ = server.await;
                    result
                }
            };
            assert_eq!(
                result.is_ok(),
                matches!(race, Substitution::None),
                "{direction:?}/{race:?}: {result:?}"
            );
            assert_eq!(
                metadata(&outside.path().join("file")),
                outside_before,
                "{direction:?}/{race:?}"
            );
            assert_eq!(metadata(&source.path().join("dir/file")), source_before);
            if matches!(race, Substitution::None) {
                assert_eq!(
                    metadata(&destination.path().join("dir/file")),
                    source_before
                );
            } else if matches!(race, Substitution::File | Substitution::AncestorDirectory) {
                assert_eq!(
                    metadata(&destination.path().join("dir/file")),
                    (0o644, 1_650_000_000, 123)
                );
            }
        }
    }
}

#[test]
fn metadata_lowerers_reject_missing_observations_instead_of_statting_later() {
    let mut source = Entry::file(
        sy::engine::domain::RelativePath::new("file").unwrap(),
        1,
        sy::engine::domain::Timestamp::new(1, 0).unwrap(),
    );
    source.unix_mode = Some(0o600);
    let mut destination = source.clone();
    destination.unix_mode = Some(0o644);
    let op = SyncOp::Metadata {
        source,
        destination,
    };
    let policy = ExecutionPolicy {
        preserve_permissions: true,
        preserve_times: true,
    };
    assert!(lower_local_op(op.clone(), policy).is_err());
    assert!(lower_sync_op(op.clone(), policy).is_err());
    assert!(lower_pull_op(op, policy).is_err());
}

#[tokio::test]
async fn local_directory_creation_and_finalize_refuse_swapped_ancestors() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(source.path().join("dir")).unwrap();
    std::fs::create_dir(destination.path().join("dir")).unwrap();
    std::fs::create_dir(outside.path().join("child")).unwrap();
    let before = metadata(&outside.path().join("child"));
    let executor = LocalSyncExecutor::new(
        source.path().to_path_buf(),
        destination.path().to_path_buf(),
        Scheduler::new(ResourceBudget::default()).unwrap(),
    );
    std::fs::rename(
        destination.path().join("dir"),
        destination.path().join("held-dir"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), destination.path().join("dir")).unwrap();
    std::fs::create_dir(source.path().join("dir/new")).unwrap();
    let entries = local_entry_stream(source.path().to_path_buf(), ScanRequest::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let observed = entries
        .into_iter()
        .find(|entry| entry.path.as_path() == Path::new("dir/new"))
        .unwrap();
    let work = lower_local_op(
        SyncOp::Create {
            destination_path: observed.path.clone(),
            source: observed,
        },
        ExecutionPolicy::default(),
    )
    .unwrap()
    .unwrap();
    assert!(executor.execute(work).await.is_err());
    assert!(!outside.path().join("new").exists());
    assert!(SyncPlanExecutor::execute_finalize(
        &executor,
        FinalizeMetadata {
            path: sy::engine::domain::RelativePath::new("dir/child").unwrap(),
            kind: sy::engine::domain::EntryKind::Directory,
            source_identity: sy::engine::domain::EntryIdentity::from_bytes([1; 32]),
            target: sy::engine::finalize_journal::DirectoryTarget::Observed(
                sy::engine::domain::EntryIdentity::from_bytes([2; 32])
            ),
            preserve_source: false,
            unix_mode: Some(0o600),
            modified: Some(sy::engine::domain::Timestamp::new(1, 0).unwrap()),
        }
    )
    .await
    .is_err());
    assert_eq!(metadata(&outside.path().join("child")), before);
}
