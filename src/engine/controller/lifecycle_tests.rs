use super::*;
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::scheduler::{ResourceBudget, Scheduler};
use crate::remote::local_executor::LocalSyncExecutor;
use crate::rooted_fs::{PublicationPause, PublicationPausePoint};
use std::path::Path;
use std::time::Duration;

async fn local_plan(source: &SourceRoot, destination: &LocalEndpoint) -> SyncPlan {
    let request = crate::engine::scan::ScanRequest {
        metadata: crate::engine::scan::EntryMetadataRequest {
            unix_mode: true,
            ..Default::default()
        },
        ..Default::default()
    };
    preflight_sync(
        source.entries(request),
        destination
            .rooted_fs(false)
            .await
            .unwrap()
            .entry_stream(request),
        ComparisonPolicy {
            preserve_times: true,
            ..Default::default()
        },
        None,
        crate::engine::namespace::NamespacePreflight::new(&TEST_NAMESPACE),
        |_| true,
    )
    .await
    .unwrap()
}

fn executor(source: SourceRoot, destination: LocalEndpoint) -> LocalSyncExecutor {
    LocalSyncExecutor::new(
        source,
        destination,
        Scheduler::new(ResourceBudget::default()).unwrap(),
    )
    .with_remove_source_files(true)
}

// Protects both execution lanes: directory work is inline in the async phase,
// whereas file publication lives in the bounded worker set. Dropping either
// waiter must cut off late native admission without dropping its join owner.
#[tokio::test]
async fn cancelled_local_phase_retains_and_joins_native_work() {
    for directory in [false, true] {
        for admitted in [false, true] {
            let source_dir = tempfile::tempdir().unwrap();
            let destination_dir = tempfile::tempdir().unwrap();
            if directory {
                std::fs::create_dir(source_dir.path().join("entry")).unwrap();
                std::fs::write(source_dir.path().join("entry/child"), b"source").unwrap();
            } else {
                std::fs::write(source_dir.path().join("entry"), b"new source").unwrap();
                std::fs::write(destination_dir.path().join("entry"), b"old").unwrap();
            }
            let source = SourceRoot::open(source_dir.path().to_path_buf())
                .await
                .unwrap();
            let destination = LocalEndpoint::new(destination_dir.path().to_path_buf());
            let rooted = destination.rooted_fs(false).await.unwrap();
            let plan = local_plan(&source, &destination).await;
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            rooted.pause_mutation_at(
                0,
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
            let controller = SyncController::new(
                executor(source.clone(), destination),
                NonZeroUsize::new(2).unwrap(),
            );
            {
                let execution = controller.execute(plan);
                tokio::pin!(execution);
                tokio::select! {
                    result = &mut execution => panic!("execution ended before native barrier: {result:?}"),
                    reached = reached_rx => reached.unwrap(),
                }
                // Drop, not abort, the waiter. The controller retains the main
                // phase, workers, staging cleanup and eventual operation error.
            }
            let mut close = Box::pin(controller.close());
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut close)
                    .await
                    .is_err(),
                "close returned before owned native work joined"
            );
            // Even a cancelled close waiter must leave the original join owned.
            drop(close);
            resume_tx.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), controller.close())
                .await
                .unwrap();
            assert!(
                result.is_err(),
                "cancellation must not authorize removal/finalization"
            );
            let visible = destination_dir.path().join("entry");
            if directory {
                assert_eq!(visible.exists(), admitted);
                assert!(!visible.join("child").exists());
            } else {
                assert_eq!(
                    std::fs::read(&visible).unwrap(),
                    if admitted {
                        &b"new source"[..]
                    } else {
                        &b"old"[..]
                    }
                );
            }
            assert!(source_dir.path().join("entry").exists());
            assert_eq!(
                std::fs::read_dir(destination_dir.path()).unwrap().count(),
                usize::from(!directory || admitted),
                "owned staging must be cleaned before close completes"
            );
            // A SOURCE reader remains usable after the destination cutoff.
            source.validate().await.unwrap();
            if !directory {
                use crate::endpoint::Endpoint;
                assert_eq!(
                    source
                        .endpoint()
                        .metadata(Path::new("entry"))
                        .await
                        .unwrap()
                        .size,
                    10
                );
            }
        }
    }
}

// The routed PULL destination uses router admission, but must retain exactly
// the same controller/native lifecycle as LOCAL. This exercises the interrupt
// entrypoint rather than aborting a worker or fabricating a writer receipt.
#[tokio::test]
async fn routed_pull_interrupt_retains_native_publication_until_joined() {
    use crate::engine::scan::{EntryMetadataRequest, ScanRequest};
    use crate::remote::runtime::ClientRemoteSession;
    use crate::rooted_fs::operand::{BoundDestination, DestinationRoot, SourceShape};
    for admitted in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("file"), b"source bytes").unwrap();
        std::fs::write(destination.path().join("file"), b"old").unwrap();
        let binding = BoundDestination::open(
            destination.path().to_path_buf(),
            SourceShape::Directory { basename: None },
        )
        .await
        .unwrap();
        let DestinationRoot::Present(rooted) = &binding.root else {
            panic!("missing destination");
        };
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            0,
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
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(server);
        let server = tokio::spawn(crate::remote::serve::serve_transport(
            reader,
            writer,
            Default::default(),
        ));
        let (reader, writer) = tokio::io::split(client);
        let client = ClientRemoteSession::connect(
            reader,
            writer,
            crate::protocol::Operation::Pull,
            source.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut executor = crate::remote::pull::RemotePullExecutor::new(
            destination.path().to_path_buf(),
            client.request_handle(),
            client.sender(),
            Scheduler::new(ResourceBudget::default()).unwrap(),
        )
        .with_destination_binding(binding);
        let request = ScanRequest {
            metadata: EntryMetadataRequest {
                unix_mode: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let (namespace, dest) = executor.destination_entries(request).await.unwrap();
        let plan = preflight_sync(
            client.request_handle().scan(request).await.unwrap(),
            dest,
            ComparisonPolicy::default(),
            None,
            crate::engine::namespace::NamespacePreflight::new(namespace.as_ref()),
            |_| true,
        )
        .await
        .unwrap();
        let controller = Arc::new(SyncController::new(executor, NonZeroUsize::new(1).unwrap()));
        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        let owner = Arc::clone(&controller);
        let mut interrupted = tokio::spawn(async move {
            owner
                .execute_until(plan, async move {
                    reached_rx.await.unwrap();
                    stopped_tx.send(()).unwrap();
                    Ok(())
                })
                .await
        });
        stopped_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !controller.cancelled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut interrupted)
                .await
                .is_err(),
            "interrupt returned before native join"
        );
        resume_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), interrupted)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(destination.path().join("file")).unwrap(),
            if admitted {
                &b"source bytes"[..]
            } else {
                &b"old"[..]
            }
        );
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(source.path().join("file")).unwrap(),
            b"source bytes"
        );
        controller.close().await.unwrap();
        drop(client);
        let result_server = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        // PULL's FileEnd is not a completion ACK. Cancellation prevents that
        // ACK even when local publication was admitted, so the server must
        // report its admitted fetch as an incomplete request, not orderly EOF.
        assert!(
            matches!(
                result_server,
                Err(crate::remote::serve::ServeError::Request(_))
            ),
            "{result_server:?}"
        );
    }
}

#[tokio::test]
async fn required_directory_finalization_failure_retains_source_after_file_commit() {
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("parent")).unwrap();
    std::fs::write(source_dir.path().join("parent/file"), b"source").unwrap();
    let source = SourceRoot::open(source_dir.path().to_path_buf())
        .await
        .unwrap();
    let destination = LocalEndpoint::new(destination_dir.path().to_path_buf());
    let rooted = destination.rooted_fs(false).await.unwrap();
    let plan = local_plan(&source, &destination).await;
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    // Directory creation and file publication precede the required mtime setter.
    rooted.pause_mutation_at(
        2,
        PublicationPause {
            point: PublicationPausePoint::BeforeAdmission,
            reached: reached_tx,
            resume: resume_rx,
        },
    );
    let controller =
        SyncController::new(executor(source, destination), NonZeroUsize::new(1).unwrap());
    let error = {
        let execution = controller.execute(plan);
        tokio::pin!(execution);
        tokio::select! {
            result = &mut execution => panic!("missing finalization barrier: {result:?}"),
            reached = reached_rx => reached.unwrap(),
        }
        assert_eq!(
            std::fs::read(destination_dir.path().join("parent/file")).unwrap(),
            b"source"
        );
        // Fail the required setter at its real native gate without cancelling
        // the async owner: it must report committed finalization failure.
        controller.executor.close_mutation_admission();
        resume_tx.send(()).unwrap();
        execution.await.unwrap_err()
    };
    assert!(
        matches!(
            error,
            ControllerError::CommittedDirectoryFinalization { .. }
        ),
        "{error}"
    );
    assert_eq!(
        std::fs::read(source_dir.path().join("parent/file")).unwrap(),
        b"source"
    );
}
