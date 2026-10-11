//! Backup/delete safety across the real local and v3 mutation adapters.
#![cfg(unix)]

use bytes::Bytes;
use std::path::Path;
use std::time::Duration;
use sy::engine::controller::SyncPlanExecutor;
use sy::engine::delete_plan::DeleteAction;
use sy::engine::domain::{EntryKind, RelativePath};
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::protocol::{Frame, FrameFlags, FrameKind, Operation};
use sy::remote::local_executor::LocalSyncExecutor;
use sy::remote::pull::RemotePullExecutor;
use sy::remote::push::{RemoteBackupPlan, RemotePushExecutor};
use sy::remote::router::RouterConfig;
use sy::remote::runtime::{ClientRemoteSession, IncomingRequest, ServerRemoteSession};
use sy::rooted_fs::RootedFs;
use sy::transfer::delta::BasisIndexLimits;

#[derive(Clone, Copy, Debug)]
enum Direction {
    Local,
    Push,
    Pull,
}

const DIRECTIONS: [Direction; 3] = [Direction::Local, Direction::Push, Direction::Pull];

fn relative(path: &str) -> RelativePath {
    RelativePath::new(path).unwrap()
}

async fn observed_action(root: &Path, path: &str) -> DeleteAction {
    let rooted = RootedFs::open(root.to_path_buf()).await.unwrap();
    let path = relative(path);
    let (kind, identity) = rooted.path_identity_blocking(&path).unwrap().unwrap();
    DeleteAction {
        path,
        kind,
        identity: Some(identity),
    }
}

async fn delete_with_backup(
    direction: Direction,
    destination: &Path,
    action: DeleteAction,
) -> anyhow::Result<()> {
    let source = tempfile::tempdir().unwrap();
    let scheduler = Scheduler::new(ResourceBudget::default()).unwrap();
    if matches!(direction, Direction::Local) {
        let executor = LocalSyncExecutor::new(
            sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
                .await
                .unwrap(),
            sy::endpoint::local::LocalEndpoint::new(destination.to_path_buf()),
            scheduler,
        )
        .with_backup(true, None, "~".into());
        return SyncPlanExecutor::execute_delete(&executor, action)
            .await
            .map_err(Into::into);
    }

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    let operation = if matches!(direction, Direction::Push) {
        Operation::Push
    } else {
        Operation::Pull
    };
    let root = if matches!(direction, Direction::Push) {
        destination
    } else {
        source.path()
    };
    let (client, server) = tokio::join!(
        ClientRemoteSession::connect(
            client_reader,
            client_writer,
            operation,
            root,
            RouterConfig::default()
        ),
        ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default()),
    );
    let client = client.unwrap();
    let mut server = server.unwrap();
    let task = tokio::spawn(async move {
        let handler = server.mutation_handler().unwrap();
        let sender = server.sender();
        while let Some(request) = server.next_request().await.unwrap() {
            let IncomingRequest::Mutation(incoming) = request else {
                panic!("expected mutation")
            };
            let stream = incoming.first.frame().stream_id();
            if handler.serve(incoming).await.is_err() {
                // Keep the peer alive after a real filesystem rejection. A
                // client must not rely on disconnect to prevent a later delete.
                sender
                    .send(
                        Frame::new(FrameKind::Error, FrameFlags::FINAL, stream, Bytes::new())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
            }
        }
    });
    let result = match direction {
        Direction::Push => {
            let executor = RemotePushExecutor::new(
                sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
                    .await
                    .unwrap(),
                client.request_handle(),
                scheduler,
                BasisIndexLimits::default(),
            )
            .with_backup(Some(RemoteBackupPlan {
                suffix: "~".into(),
                dir: None,
            }));
            executor.execute_delete(action).await.map_err(Into::into)
        }
        Direction::Pull => {
            let executor = RemotePullExecutor::new(
                destination.to_path_buf(),
                client.request_handle(),
                client.sender(),
                scheduler,
            )
            .with_backup_enabled(true)
            .with_backup_suffix("~".into());
            executor.execute_delete(action).await.map_err(Into::into)
        }
        Direction::Local => unreachable!(),
    };
    drop(client);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    result
}

#[tokio::test]
async fn backup_failure_prevents_deletion() {
    for direction in DIRECTIONS {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"precious destination").unwrap();
        // Refusing to replace a nonempty backup directory is deterministic,
        // including when the tests run with elevated privileges.
        std::fs::create_dir(root.path().join("file~")).unwrap();
        std::fs::write(root.path().join("file~/child"), b"existing backup").unwrap();
        let action = observed_action(root.path(), "file").await;
        assert!(
            delete_with_backup(direction, root.path(), action)
                .await
                .is_err(),
            "{direction:?}"
        );
        assert_eq!(
            std::fs::read(root.path().join("file")).unwrap(),
            b"precious destination",
            "{direction:?}"
        );
        assert_eq!(
            std::fs::read(root.path().join("file~/child")).unwrap(),
            b"existing backup"
        );
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            2,
            "staging leaked"
        );
    }
}

#[tokio::test]
async fn changed_identity_preserves_destination_and_existing_backup() {
    for direction in DIRECTIONS {
        for replacement in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("file");
            std::fs::write(&path, b"scanned").unwrap();
            std::fs::write(root.path().join("file~"), b"previous backup").unwrap();
            let action = observed_action(root.path(), "file").await;
            if replacement {
                std::fs::remove_file(&path).unwrap();
            }
            std::fs::write(&path, b"unrelated new contents").unwrap();
            assert!(
                delete_with_backup(direction, root.path(), action)
                    .await
                    .is_err(),
                "{direction:?}, replacement={replacement}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"unrelated new contents",
                "{direction:?}"
            );
            assert_eq!(
                std::fs::read(root.path().join("file~")).unwrap(),
                b"previous backup",
                "{direction:?}"
            );
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
        }
    }
}

#[tokio::test]
async fn regular_file_replaced_by_symlink_is_not_backed_up_or_deleted() {
    for direction in DIRECTIONS {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        std::fs::write(&target, b"outside contents").unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"scanned").unwrap();
        std::fs::write(root.path().join("file~"), b"previous backup").unwrap();
        let action = observed_action(root.path(), "file").await;
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(
            delete_with_backup(direction, root.path(), action)
                .await
                .is_err(),
            "{direction:?}"
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"outside contents");
        assert_eq!(
            std::fs::read(root.path().join("file~")).unwrap(),
            b"previous backup"
        );
    }
}

#[tokio::test]
async fn regular_deletion_publishes_backup_before_removal() {
    for direction in DIRECTIONS {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"precious destination").unwrap();
        std::fs::write(root.path().join("file~"), b"old backup").unwrap();
        let action = observed_action(root.path(), "file").await;
        delete_with_backup(direction, root.path(), action)
            .await
            .unwrap();
        assert!(!root.path().join("file").exists(), "{direction:?}");
        assert_eq!(
            std::fs::read(root.path().join("file~")).unwrap(),
            b"precious destination",
            "{direction:?}"
        );
    }
}

#[tokio::test]
async fn symlink_deletion_does_not_back_up_or_follow_targets() {
    for direction in DIRECTIONS {
        for dangling in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("target");
            if !dangling {
                std::fs::write(&target, b"outside contents").unwrap();
            }
            std::os::unix::fs::symlink(&target, root.path().join("link")).unwrap();
            std::fs::write(root.path().join("link~"), b"previous backup").unwrap();
            let action = observed_action(root.path(), "link").await;
            assert_eq!(action.kind, EntryKind::Symlink);
            delete_with_backup(direction, root.path(), action)
                .await
                .unwrap();
            assert!(
                std::fs::symlink_metadata(root.path().join("link")).is_err(),
                "{direction:?}"
            );
            assert_eq!(
                std::fs::read(root.path().join("link~")).unwrap(),
                b"previous backup"
            );
            if !dangling {
                assert_eq!(std::fs::read(&target).unwrap(), b"outside contents");
            }
        }
    }
}

#[tokio::test]
async fn local_delete_cannot_follow_a_raced_ancestor_outside_the_root() {
    for backup in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("parent")).unwrap();
        std::fs::write(root.path().join("parent/file"), b"keep").unwrap();
        let action = observed_action(root.path(), "parent/file").await;
        let moved = outside.path().join("moved");
        std::fs::rename(root.path().join("parent"), &moved).unwrap();
        std::os::unix::fs::symlink(&moved, root.path().join("parent")).unwrap();
        let source = tempfile::tempdir().unwrap();
        let executor = LocalSyncExecutor::new(
            sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
                .await
                .unwrap(),
            sy::endpoint::local::LocalEndpoint::new(root.path().to_path_buf()),
            Scheduler::new(ResourceBudget::default()).unwrap(),
        )
        .with_backup(backup, None, "~".into());
        assert!(SyncPlanExecutor::execute_delete(&executor, action)
            .await
            .is_err());
        assert_eq!(std::fs::read(moved.join("file")).unwrap(), b"keep");
        assert!(!moved.join("file~").exists());
    }
}
