//! Pull source-completion checks and native mutation cutoff/drain contracts.
use super::*;
use crate::engine::scheduler::ResourceBudget;
use crate::protocol::Operation;
use crate::remote::router::{RouterConfig, RouterError};
use crate::remote::runtime::ClientRemoteSession;
use crate::rooted_fs::{PublicationPause, PublicationPausePoint, RootedFsError};
use futures::TryStreamExt;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

/// A source race after admission cannot be converted into successful stale
/// preservation, even though an admitted destination mutation may finish.
#[tokio::test]
async fn metadata_completion_rechecks_source_after_admitted_native_work() {
    use std::os::unix::fs::PermissionsExt;
    let source_root = tempfile::tempdir().unwrap();
    let destination_root = tempfile::tempdir().unwrap();
    let source_path = source_root.path().join("file");
    let destination_path = destination_root.path().join("file");
    std::fs::write(&source_path, b"new").unwrap();
    std::fs::write(&destination_path, b"old").unwrap();
    std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&destination_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut request = crate::engine::scan::ScanRequest::default();
    request.metadata.unix_mode = true;
    let mut entries =
        crate::endpoint::local_entry_scan::local_entry_stream(source_root.path().into(), request);
    let source = entries.try_next().await.unwrap().unwrap();
    entries.close().await.unwrap();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server_io);
    let server = tokio::spawn(crate::remote::serve::serve_transport(
        reader,
        writer,
        RouterConfig::default(),
    ));
    let (reader, writer) = tokio::io::split(client_io);
    let client = ClientRemoteSession::connect(
        reader,
        writer,
        Operation::Pull,
        source_root.path(),
        RouterConfig::default(),
    )
    .await
    .unwrap();
    let executor = RemotePullExecutor::new(
        destination_root.path().into(),
        client.request_handle(),
        client.sender(),
        Scheduler::new(ResourceBudget {
            metadata_ops: 1,
            cpu_tasks: 1,
            ..ResourceBudget::default()
        })
        .unwrap(),
    );
    let rooted = executor.metadata_authority().await.unwrap().clone();
    let expected = rooted
        .path_identity_blocking(&source.path)
        .unwrap()
        .unwrap()
        .1;
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    rooted.pause_mutation_at(
        0,
        PublicationPause {
            point: PublicationPausePoint::AfterAdmission,
            reached: reached_tx,
            resume: resume_rx,
        },
    );
    let mut destination = source.clone();
    destination.identity = Some(expected);
    let action = RemotePullAction::ApplyMetadata {
        source,
        destination,
        unix_mode: Some(0o600),
        modified: None,
    };
    let resources = crate::remote::pull_lower::action_resources(&action);
    let worker =
        tokio::spawn(async move { executor.execute(WorkItem::new(action, resources)).await });
    let reached = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
    // Release and join before assertions, including when reaching the native
    // boundary failed. No admitted worker is left behind by a test assertion.
    let changed = std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o620));
    let resumed = resume_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(5), worker).await;
    drop(client);
    let _closed = server.await.unwrap();
    assert!(matches!(reached, Ok(Ok(()))));
    changed.unwrap();
    resumed.unwrap();
    assert!(result.unwrap().unwrap().is_err());
    assert_eq!(std::fs::read(&source_path).unwrap(), b"new");
    assert_eq!(
        std::fs::metadata(&source_path).unwrap().mode() & 0o777,
        0o620
    );
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"old");
    // Post-mutation failure is not a rollback claim.
    assert_eq!(
        std::fs::metadata(&destination_path).unwrap().mode() & 0o777,
        0o600
    );
}

#[derive(Debug, Clone, Copy)]
enum Mutation {
    Directory,
    Symlink,
    Delete,
    Metadata,
    Finalize,
    Backup,
    ExternalBackup,
    Fetch,
    Hardlink,
}

#[tokio::test]
async fn queued_pull_mutations_obey_client_cutoff_without_blocking_router() {
    for mutation in [
        Mutation::Directory,
        Mutation::Symlink,
        Mutation::Delete,
        Mutation::Metadata,
        Mutation::Finalize,
        Mutation::Backup,
        Mutation::ExternalBackup,
        Mutation::Fetch,
        Mutation::Hardlink,
    ] {
        for admitted in [false, true] {
            let source_root = tempfile::tempdir().unwrap();
            let destination_root = tempfile::tempdir().unwrap();
            let external_backup = tempfile::tempdir().unwrap();
            std::fs::write(source_root.path().join("file"), b"new bytes").unwrap();
            std::fs::create_dir(source_root.path().join("directory")).unwrap();
            std::fs::write(destination_root.path().join("file"), b"old bytes").unwrap();
            std::fs::create_dir(destination_root.path().join("directory")).unwrap();
            let mut request = crate::engine::scan::ScanRequest::default();
            request.metadata.unix_mode = true;
            if matches!(mutation, Mutation::Hardlink) {
                std::fs::hard_link(
                    source_root.path().join("file"),
                    source_root.path().join("member"),
                )
                .unwrap();
                std::fs::write(destination_root.path().join("representative"), b"new bytes")
                    .unwrap();
                request.metadata.hardlink_group = true;
            }
            let source_entries: Vec<Entry> = crate::endpoint::local_entry_scan::local_entry_stream(
                source_root.path().to_path_buf(),
                request,
            )
            .try_collect()
            .await
            .unwrap();
            let source = source_entries
                .iter()
                .find(|entry| entry.is_file())
                .unwrap()
                .clone();
            let directory = source_entries
                .iter()
                .find(|entry| entry.is_directory())
                .unwrap()
                .clone();
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (client_reader, client_writer) = tokio::io::split(client_io);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let server = tokio::spawn(crate::remote::serve::serve_transport(
                server_reader,
                server_writer,
                RouterConfig::default(),
            ));
            let client = ClientRemoteSession::connect(
                client_reader,
                client_writer,
                Operation::Pull,
                source_root.path(),
                RouterConfig::default(),
            )
            .await
            .unwrap();
            let sender = client.sender();
            let mut executor = RemotePullExecutor::new(
                destination_root.path().to_path_buf(),
                client.request_handle(),
                sender.clone(),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            );
            if matches!(mutation, Mutation::Backup | Mutation::ExternalBackup) {
                executor = executor
                    .with_backup_enabled(true)
                    .with_backup_suffix("~".into());
                if matches!(mutation, Mutation::ExternalBackup) {
                    executor =
                        executor.with_backup(Some(external_backup.path().join("new-directory")));
                }
            }
            if matches!(mutation, Mutation::Hardlink) {
                executor = executor.with_hardlinks(true);
                executor
                    .hardlink_groups
                    .lock()
                    .await
                    .insert(
                        *source.hardlink_group.unwrap().as_bytes(),
                        HardlinkRepresentative {
                            path: RelativePath::new("representative").unwrap(),
                            publication: crate::endpoint::local_identity::metadata_identity(
                                &std::fs::metadata(destination_root.path().join("representative"))
                                    .unwrap(),
                                EntryKind::File,
                            )
                            .unwrap(),
                            unix_mode: Some(0o600),
                            modified: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let rooted = executor.metadata_authority().await.unwrap().clone();
            let file = RelativePath::new("file").unwrap();
            let before = rooted.path_identity_blocking(&file).unwrap().unwrap().1;
            let directory_id = rooted
                .path_identity_blocking(&directory.path)
                .unwrap()
                .unwrap()
                .1;
            // Existing-directory creation is a no-op, so use a genuinely new leaf.
            if matches!(mutation, Mutation::Directory) {
                std::fs::remove_dir(destination_root.path().join("directory")).unwrap();
            }
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            rooted.pause_mutation_at(
                usize::from(matches!(mutation, Mutation::Hardlink)),
                PublicationPause {
                    point: if admitted {
                        PublicationPausePoint::AfterAdmission
                    } else {
                        PublicationPausePoint::BeforeAdmission
                    },
                    reached: reached_tx,
                    resume: resume_rx,
                },
            );
            let worker =
                tokio::spawn(async move {
                    match mutation {
                        Mutation::Delete | Mutation::Backup | Mutation::ExternalBackup => {
                            executor
                                .execute_delete(crate::engine::delete_plan::DeleteAction {
                                    path: file,
                                    kind: EntryKind::File,
                                    identity: Some(before),
                                })
                                .await
                        }
                        Mutation::Finalize => executor
                            .execute_finalize(crate::engine::finalize_journal::FinalizeMetadata {
                                path: directory.path,
                                kind: EntryKind::Directory,
                                source_identity: directory.identity.unwrap(),
                                target: crate::engine::finalize_journal::DirectoryTarget::Observed(
                                    directory_id,
                                ),
                                preserve_source: false,
                                unix_mode: Some(0o700),
                                modified: Some(Timestamp::UNIX_EPOCH),
                            })
                            .await,
                        _ => {
                            let action = match mutation {
                                Mutation::Directory => RemotePullAction::CreateDirectory {
                                    destination_path: directory.path.clone(),
                                    source: directory,
                                },
                                Mutation::Symlink => {
                                    let mut destination = source.clone();
                                    destination.identity = Some(before);
                                    RemotePullAction::ReplaceSymlink {
                                        destination_path: file.clone(),
                                        source: Entry::symlink(
                                            file,
                                            "target".into(),
                                            Timestamp::UNIX_EPOCH,
                                        ),
                                        destination: Some(destination),
                                        modified: None,
                                    }
                                }
                                Mutation::Metadata => {
                                    let mut destination = source.clone();
                                    destination.identity = Some(before);
                                    RemotePullAction::ApplyMetadata {
                                        source,
                                        destination,
                                        unix_mode: Some(0o600),
                                        modified: Some(Timestamp::UNIX_EPOCH),
                                    }
                                }
                                Mutation::Fetch | Mutation::Hardlink => {
                                    let mut destination = source.clone();
                                    destination.identity = Some(before);
                                    RemotePullAction::FetchFile {
                                        destination_path: source.path.clone(),
                                        source,
                                        destination: Some(destination),
                                        metadata: PullTransferMetadata {
                                            unix_mode: Some(0o600),
                                            modified: None,
                                        },
                                    }
                                }
                                _ => unreachable!(),
                            };
                            executor
                                .execute(WorkItem::new(
                                    action,
                                    ResourceRequest {
                                        metadata_ops: 1,
                                        ..Default::default()
                                    },
                                ))
                                .await
                                .map(|_| ())
                        }
                    }
                });
            let reached = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
            sender.fail(Arc::new(RouterError::WriterClosed));
            let closed = tokio::time::timeout(Duration::from_secs(5), sender.closed()).await;
            let private = std::fs::read(destination_root.path().join("file"))
                .is_ok_and(|bytes| bytes == b"old bytes");
            let owned = !worker.is_finished();
            let _ = resume_tx.send(());
            let result = tokio::time::timeout(Duration::from_secs(5), worker)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(reached, Ok(Ok(()))),
                "{mutation:?}/{admitted}: gate not reached"
            );
            assert!(
                closed.is_ok(),
                "{mutation:?}: router blocked behind native work"
            );
            assert!(
                private && owned,
                "{mutation:?}: premature native effect or worker loss"
            );
            if !admitted {
                assert!(
                    matches!(
                        result,
                        Err(RemotePullError::Rooted(RootedFsError::CommitCancelled))
                            | Err(RemotePullError::Endpoint(_))
                    ),
                    "{mutation:?}: {result:?}"
                );
                assert_eq!(
                    rooted
                        .path_identity_blocking(&RelativePath::new("file").unwrap())
                        .unwrap()
                        .unwrap()
                        .1,
                    before
                );
                assert_eq!(
                    std::fs::read(destination_root.path().join("file")).unwrap(),
                    b"old bytes"
                );
                assert!(!destination_root.path().join("file~").exists());
                assert!(!external_backup.path().join("new-directory").exists());
                if matches!(mutation, Mutation::Directory) {
                    // Initial root creation is authority too, not just mkdir
                    // beneath an already pinned root.
                    let absent_root = destination_root.path().join("absent/nested");
                    let fresh = RemotePullExecutor::new(
                        absent_root.clone(),
                        client.request_handle(),
                        client.sender(),
                        Scheduler::new(ResourceBudget::default()).unwrap(),
                    );
                    assert!(matches!(
                        fresh.destination_entries(Default::default(), false).await,
                        Err(RemotePullError::Rooted(RootedFsError::CommitCancelled))
                    ));
                    assert!(!destination_root.path().join("absent").exists());
                }
                assert_eq!(
                    destination_root.path().join("directory").exists(),
                    !matches!(mutation, Mutation::Directory)
                );
            } else {
                match mutation {
                    Mutation::Directory => {
                        assert!(result.is_ok(), "{result:?}");
                        assert!(destination_root.path().join("directory").is_dir());
                    }
                    Mutation::Symlink => {
                        assert!(result.is_ok(), "{result:?}");
                        assert_eq!(
                            std::fs::read_link(destination_root.path().join("file")).unwrap(),
                            Path::new("target")
                        );
                    }
                    Mutation::Delete => {
                        assert!(result.is_ok(), "{result:?}");
                        assert!(!destination_root.path().join("file").exists());
                    }
                    Mutation::Metadata | Mutation::Finalize => {
                        if matches!(mutation, Mutation::Metadata) {
                            // Native admission won, but the closed transport
                            // cannot confirm the source observation at completion.
                            assert!(
                                matches!(
                                    result,
                                    Err(RemotePullError::Remote(RemoteSessionError::Metadata(
                                        crate::remote::runtime::metadata::RemoteMetadataError::Router(_)
                                    )))
                                ),
                                "{result:?}"
                            );
                        } else {
                            assert!(result.is_ok(), "{result:?}");
                        }
                        let (path, mode) = if matches!(mutation, Mutation::Metadata) {
                            ("file", 0o600)
                        } else {
                            ("directory", 0o700)
                        };
                        let stat = std::fs::metadata(destination_root.path().join(path)).unwrap();
                        assert_eq!(stat.mode() & 0o7777, mode);
                        assert_eq!(stat.mtime(), 0);
                    }
                    Mutation::Backup => {
                        assert!(matches!(
                            result,
                            Err(RemotePullError::Rooted(RootedFsError::CommitCancelled))
                        ));
                        assert_eq!(
                            std::fs::read(destination_root.path().join("file~")).unwrap(),
                            b"old bytes"
                        );
                        assert_eq!(
                            std::fs::read(destination_root.path().join("file")).unwrap(),
                            b"old bytes"
                        );
                    }
                    Mutation::ExternalBackup => {
                        assert!(result.is_err());
                        assert!(external_backup.path().join("new-directory").is_dir());
                        assert_eq!(
                            std::fs::read_dir(external_backup.path().join("new-directory"))
                                .unwrap()
                                .count(),
                            0
                        );
                    }
                    Mutation::Fetch => {
                        assert!(matches!(
                            result,
                            Err(RemotePullError::CommittedAckFailed { .. })
                        ));
                        assert_eq!(
                            std::fs::read(destination_root.path().join("file")).unwrap(),
                            b"new bytes"
                        );
                    }
                    Mutation::Hardlink => {
                        assert!(result.is_ok(), "{result:?}");
                        assert_eq!(
                            std::fs::metadata(destination_root.path().join("file"))
                                .unwrap()
                                .ino(),
                            std::fs::metadata(destination_root.path().join("representative"))
                                .unwrap()
                                .ino()
                        );
                    }
                }
            }
            if matches!(mutation, Mutation::Hardlink) {
                assert_eq!(
                    std::fs::metadata(destination_root.path().join("representative"))
                        .unwrap()
                        .nlink(),
                    if admitted { 2 } else { 1 }
                );
            }
            assert!(
                std::fs::read_dir(destination_root.path())
                    .unwrap()
                    .all(|entry| {
                        let name = entry.unwrap().file_name();
                        name == "file"
                            || name == "directory"
                            || name == "file~"
                            || name == "representative"
                    }),
                "{mutation:?}: private staging leaked"
            );
            assert_eq!(
                std::fs::read(source_root.path().join("file")).unwrap(),
                b"new bytes"
            );
            drop(client);
            server.abort();
            let _ = server.await;
        }
    }
}
