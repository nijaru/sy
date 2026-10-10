//! Local operator-path continuity is independent of negotiated remote roots.
use super::*;
use crate::engine::controller::SyncPlanExecutor;
use crate::engine::scan::ScanRequest;
use crate::protocol::Operation;
use crate::remote::runtime::ClientRemoteSession;
use crate::rooted_fs::{PublicationPause, PublicationPausePoint};
use futures::TryStreamExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

#[tokio::test]
async fn pull_metadata_refuses_replaced_local_root_and_reports_admitted_effects() {
    for admitted in [false, true] {
        let source_root = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("destination");
        let replacement = parent.path().join("replacement");
        let retained = parent.path().join("original");
        for directory in [&root, &replacement] {
            std::fs::create_dir(directory).unwrap();
            std::fs::write(directory.join("renamed"), b"old").unwrap();
            std::fs::set_permissions(
                directory.join("renamed"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        let source_path = source_root.path().join("source");
        std::fs::write(&source_path, b"new").unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let source_before = std::fs::metadata(&source_path).unwrap();
        let request = ScanRequest {
            metadata: crate::engine::scan::EntryMetadataRequest {
                unix_mode: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut sources = crate::endpoint::local_entry_scan::local_entry_stream(
            source_root.path().into(),
            request,
        );
        let source = sources.try_next().await.unwrap().unwrap();
        sources.close().await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(server_io);
        let server = tokio::spawn(crate::remote::serve::serve_transport(
            reader,
            writer,
            Default::default(),
        ));
        let (reader, writer) = tokio::io::split(client_io);
        let client = ClientRemoteSession::connect(
            reader,
            writer,
            Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let executor = Arc::new(RemotePullExecutor::new(
            root.clone(),
            client.request_handle(),
            client.sender(),
            Scheduler::new(Default::default()).unwrap(),
        ));
        let (_, mut destinations) = executor.destination_entries(request).await.unwrap();
        let destination = destinations.try_next().await.unwrap().unwrap();
        destinations.close().await.unwrap();
        let rooted = executor.metadata_authority().await.unwrap().clone();
        let action = RemotePullAction::ApplyMetadata {
            source,
            destination,
            unix_mode: Some(0o600),
            modified: None,
        };
        let resources = crate::remote::pull_lower::action_resources(&action);
        let swap = || {
            std::fs::rename(&root, &retained).unwrap();
            std::fs::rename(&replacement, &root).unwrap();
        };
        let result = if admitted {
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
            let working = executor.clone();
            let worker =
                tokio::spawn(
                    async move { working.execute(WorkItem::new(action, resources)).await },
                );
            let reached = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
            swap();
            let resumed = resume_tx.send(());
            let result = tokio::time::timeout(Duration::from_secs(5), worker).await;
            // Drain admitted work before any assertions, including failed reach.
            assert!(matches!(reached, Ok(Ok(()))));
            resumed.unwrap();
            result.unwrap().unwrap()
        } else {
            swap();
            executor.execute(WorkItem::new(action, resources)).await
        };
        let completion = executor.finish_deferred_source_removals().await;
        drop(executor);
        drop(client);
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "local root relocation cannot be reported as success (admitted={admitted})"
        );
        if admitted {
            assert!(matches!(
                result,
                Err(RemotePullError::Endpoint(
                    crate::error::SyncError::CommittedRootChanged { .. }
                ))
            ));
        }
        assert!(
            completion.is_err(),
            "even an empty completion must verify the pinned local root"
        );
        assert_eq!(std::fs::read(retained.join("renamed")).unwrap(), b"old");
        assert_eq!(
            std::fs::metadata(retained.join("renamed")).unwrap().mode() & 0o777,
            if admitted { 0o600 } else { 0o644 }
        );
        assert_eq!(std::fs::read(root.join("renamed")).unwrap(), b"old");
        assert_eq!(
            std::fs::metadata(root.join("renamed")).unwrap().mode() & 0o777,
            0o644
        );
        assert_eq!(std::fs::read(&source_path).unwrap(), b"new");
        let after = std::fs::metadata(&source_path).unwrap();
        assert_eq!(
            (after.ino(), after.ctime(), after.ctime_nsec(), after.mode()),
            (
                source_before.ino(),
                source_before.ctime(),
                source_before.ctime_nsec(),
                source_before.mode()
            )
        );
    }
}
