use super::*;
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::scan::{EntryMetadataRequest, ScanRequest};
use crate::engine::scheduler::{ResourceBudget, Scheduler};
use crate::protocol::Operation;
use crate::remote::local_executor::LocalSyncExecutor;
use crate::remote::runtime::ClientRemoteSession;
use crate::rooted_fs::{PublicationPause, PublicationPausePoint};
use std::time::Duration;

// Hold B at the worker-future boundary, before scheduler/native admission.
// Either cutoff releases it, so a run-only cutoff deterministically exposes the
// bug: B can still issue a real remote backup and transfer during owned drain.
struct HoldSecondPushWorker {
    inner: crate::remote::push::RemotePushExecutor,
    queued: tokio::sync::Notify,
    cutoff: tokio::sync::Notify,
    completed: std::sync::atomic::AtomicUsize,
    drained: AtomicBool,
}

impl SyncPlanExecutor for HoldSecondPushWorker {
    type Action = crate::remote::push::RemotePushAction;
    type Error = crate::remote::push::RemotePushError;

    fn close_mutation_admission(&self) {
        self.inner.close_mutation_admission();
        self.cutoff.notify_one();
    }
    fn cancel_mutations(&self) {
        self.inner.cancel_mutations();
        self.cutoff.notify_one();
    }
    fn lower(
        &self,
        op: SyncOp,
        policy: ExecutionPolicy,
    ) -> std::result::Result<Option<WorkItem<Self::Action>>, Self::Error> {
        self.inner.lower(op, policy)
    }
    fn is_directory_action(&self, action: &Self::Action) -> bool {
        self.inner.is_directory_action(action)
    }
    async fn execute(
        &self,
        item: WorkItem<Self::Action>,
    ) -> std::result::Result<WorkResult, Self::Error> {
        let crate::remote::push::RemotePushAction::TransferFile { source, .. } = item.action()
        else {
            panic!("expected file worker");
        };
        if source.path.as_path() == std::path::Path::new("a") {
            self.queued.notified().await;
        } else {
            assert_eq!(source.path.as_path(), std::path::Path::new("b"));
            self.queued.notify_one();
            self.cutoff.notified().await;
        }
        let result = self.inner.execute(item).await;
        self.completed.fetch_add(1, Ordering::Release);
        result
    }
    async fn execute_delete(&self, action: DeleteAction) -> std::result::Result<(), Self::Error> {
        self.inner.execute_delete(action).await
    }
    async fn execute_finalize(
        &self,
        metadata: FinalizeMetadata,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.execute_finalize(metadata).await
    }
    async fn finish_deferred_source_removals(&self) -> std::result::Result<(), Self::Error> {
        self.inner.finish_deferred_source_removals().await
    }
    async fn close_source_removals(&self) -> std::result::Result<u64, Self::Error> {
        assert_eq!(self.completed.load(Ordering::Acquire), 2);
        let removed = self.inner.close_source_removals().await?;
        assert_eq!(removed, 0);
        self.drained.store(true, Ordering::Release);
        Ok(removed)
    }
    async fn remove_unchanged_source(
        &self,
        source: &Entry,
        destination: &Entry,
        policy: ExecutionPolicy,
    ) -> std::result::Result<(), Self::Error> {
        self.inner
            .remove_unchanged_source(source, destination, policy)
            .await
    }
}

#[tokio::test]
async fn routed_push_source_failure_cuts_off_waiting_worker_before_owned_drain() {
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("a"), b"source a").unwrap();
    std::fs::write(source_dir.path().join("b"), b"source b").unwrap();
    std::fs::write(destination_dir.path().join("b"), b"old").unwrap();
    let source = SourceRoot::open(source_dir.path().to_path_buf())
        .await
        .unwrap();
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
        Operation::Push,
        destination_dir.path(),
        Default::default(),
    )
    .await
    .unwrap();
    let remote = client.request_handle();
    let request = ScanRequest {
        metadata: EntryMetadataRequest {
            unix_mode: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let plan = preflight_sync(
        source.entries(request),
        remote.scan(request).await.unwrap(),
        ComparisonPolicy::default(),
        None,
        crate::engine::namespace::NamespacePreflight::new(&TEST_NAMESPACE),
        |_| true,
    )
    .await
    .unwrap();
    // A has no OLD destination or backup RPC. Its changed size fails the real
    // original SOURCE validation before FileBegin; B's SOURCE remains valid.
    std::fs::write(source_dir.path().join("a"), b"changed source a").unwrap();
    let inner = crate::remote::push::RemotePushExecutor::new(
        source,
        remote,
        Scheduler::new(ResourceBudget::default()).unwrap(),
        Default::default(),
    )
    .with_remove_source_files(true)
    .with_backup(Some(crate::remote::push::RemoteBackupPlan {
        suffix: "~".into(),
        dir: None,
    }));
    let controller = SyncController::new(
        HoldSecondPushWorker {
            inner,
            queued: Default::default(),
            cutoff: Default::default(),
            completed: Default::default(),
            drained: Default::default(),
        },
        NonZeroUsize::new(2).unwrap(),
    );
    let error = tokio::time::timeout(Duration::from_secs(5), controller.execute(plan))
        .await
        .unwrap()
        .unwrap_err();
    assert!(controller.executor.drained.load(Ordering::Acquire));
    assert!(!controller.cancelled.load(Ordering::Acquire));
    assert_eq!(
        std::fs::read(destination_dir.path().join("b")).unwrap(),
        b"old"
    );
    assert!(!destination_dir.path().join("b~").exists());
    assert!(!destination_dir.path().join("a").exists());
    assert_eq!(
        std::fs::read_dir(destination_dir.path()).unwrap().count(),
        1
    );
    assert_eq!(
        std::fs::read(source_dir.path().join("a")).unwrap(),
        b"changed source a"
    );
    assert_eq!(
        std::fs::read(source_dir.path().join("b")).unwrap(),
        b"source b"
    );
    let ControllerError::OwnedDrain { operation, .. } = error else {
        panic!("lost waiting worker's drain failure: {error:?}");
    };
    let ControllerError::Backend { source, .. } = *operation else {
        panic!("lost original SOURCE failure: {operation:?}");
    };
    assert!(
        matches!(
            source.downcast_ref::<crate::remote::push::RemotePushError>(),
            Some(crate::remote::push::RemotePushError::Remote(
                crate::remote::runtime::RemoteSessionError::Transfer(
                    crate::remote::transfer::RemoteTransferError::RootedFs(
                        crate::rooted_fs::RootedFsError::SourceBindingChanged(path)
                    )
                )
            )) if path == std::path::Path::new("a")
        ),
        "lost typed SOURCE change: {source:?}"
    );
    assert!(controller.run.lock().await.worker.is_none());
    drop(controller);
    drop(client);
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    // No request was admitted after scanning: the server drains orderly EOF.
    result.unwrap();
}

// Directory preparation fails outside main_result's worker scope. Failure must
// terminate the session there too, whereas successful runs leave it reusable.
#[tokio::test]
async fn routed_push_inline_failure_closes_session_but_success_keeps_it_reusable() {
    for fail in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let destination_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(source_dir.path().join("parent")).unwrap();
        let source = SourceRoot::open(source_dir.path().to_path_buf())
            .await
            .unwrap();
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
            Operation::Push,
            destination_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let remote = client.request_handle();
        let request = ScanRequest {
            metadata: EntryMetadataRequest {
                unix_mode: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let namespace = remote.begin_namespace_qualification().await.unwrap();
        let plan = preflight_sync(
            source.entries(request),
            remote.scan(request).await.unwrap(),
            ComparisonPolicy::default(),
            None,
            crate::engine::namespace::NamespacePreflight::new(namespace.as_ref()),
            |_| true,
        )
        .await
        .unwrap();
        drop(namespace);
        if fail {
            std::fs::remove_dir(source_dir.path().join("parent")).unwrap();
            std::fs::write(source_dir.path().join("parent"), b"changed kind").unwrap();
        }
        let controller = SyncController::new(
            crate::remote::push::RemotePushExecutor::new(
                source,
                remote.clone(),
                Scheduler::new(ResourceBudget::default()).unwrap(),
                Default::default(),
            )
            .with_remove_source_files(true),
            NonZeroUsize::new(2).unwrap(),
        );
        let result = tokio::time::timeout(Duration::from_secs(5), controller.execute(plan))
            .await
            .unwrap();
        assert!(!controller.cancelled.load(Ordering::Acquire));
        if fail {
            assert!(
                matches!(
                    result,
                    Err(ControllerError::Backend {
                        phase: "directory preparation",
                        ..
                    })
                ),
                "{result:?}"
            );
            assert!(matches!(
                client.sender().check_active(),
                Err(error) if matches!(error.as_ref(), crate::remote::router::RouterError::ShuttingDown)
            ));
            assert!(!destination_dir.path().join("parent").exists());
            assert_eq!(
                std::fs::read(source_dir.path().join("parent")).unwrap(),
                b"changed kind"
            );
        } else {
            assert_eq!(result.unwrap().removed_sources, 0);
            // A real second routed request, not just an active-state check.
            use futures::StreamExt;
            let mut entries = remote.scan(request).await.unwrap();
            let entry = entries.next().await.unwrap().unwrap();
            assert_eq!(entry.path.as_path(), std::path::Path::new("parent"));
            assert!(entry.is_directory());
            assert!(entries.next().await.is_none());
            entries.close().await.unwrap();
            assert!(source_dir.path().join("parent").is_dir());
        }
        assert!(controller.run.lock().await.worker.is_none());
        drop(controller);
        drop(remote);
        drop(client);
        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        result.unwrap();
    }
}

// Exercises deferred SOURCE admission via real LOCAL and routed PUSH. The
// transfer and required destination finalization complete before the pause;
// this cancellation must never be described as guaranteed SOURCE retention.
#[tokio::test]
async fn local_and_push_source_cutoff_joins_and_reports_confirmed_unlinks() {
    for push in [false, true] {
        for (grouped, skip, admitted) in [
            (false, 0, false),
            (false, 0, true),
            (true, 0, false),
            (true, 0, true),
            (true, 1, true),
        ] {
            let source_dir = tempfile::tempdir().unwrap();
            let destination_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("file"), b"source").unwrap();
            if grouped {
                std::fs::hard_link(
                    source_dir.path().join("file"),
                    source_dir.path().join("other"),
                )
                .unwrap();
            }
            let source = SourceRoot::open(source_dir.path().to_path_buf())
                .await
                .unwrap();
            let request = ScanRequest {
                metadata: EntryMetadataRequest {
                    unix_mode: true,
                    hardlink_group: grouped,
                    ..Default::default()
                },
                ..Default::default()
            };
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            source.rooted().pause_mutation_at(
                skip,
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
            let scheduler = Scheduler::new(ResourceBudget::default()).unwrap();
            let result = if push {
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
                    Operation::Push,
                    destination_dir.path(),
                    Default::default(),
                )
                .await
                .unwrap();
                let remote = client.request_handle();
                let namespace = remote.begin_namespace_qualification().await.unwrap();
                let destination = remote.scan(request).await.unwrap();
                let mut plan = preflight_sync(
                    source.entries(request),
                    destination,
                    ComparisonPolicy::default(),
                    None,
                    crate::engine::namespace::NamespacePreflight::new(namespace.as_ref()),
                    |_| true,
                )
                .await
                .unwrap();
                drop(namespace);
                if grouped {
                    plan.seal_hardlink_membership().await.unwrap();
                }
                let executor = crate::remote::push::RemotePushExecutor::new(
                    source.clone(),
                    remote,
                    scheduler,
                    Default::default(),
                )
                .with_remove_source_files(true)
                .with_hardlinks(grouped);
                let controller = SyncController::new(executor, NonZeroUsize::new(1).unwrap());
                let result = cancel_removal(&controller, plan, reached_rx, resume_tx).await;
                drop(client);
                let result_server = tokio::time::timeout(Duration::from_secs(5), server)
                    .await
                    .unwrap()
                    .unwrap();
                if let Err(error) = result_server {
                    assert!(
                        matches!(&error, crate::remote::serve::ServeError::Session(crate::remote::runtime::RemoteSessionError::Router(error)) if matches!(error.as_ref(), crate::remote::router::RouterError::TransportEof)),
                        "{error:?}"
                    );
                }
                result
            } else {
                let destination = LocalEndpoint::new(destination_dir.path().to_path_buf());
                let mut plan = preflight_sync(
                    source.entries(request),
                    destination
                        .rooted_fs(false)
                        .await
                        .unwrap()
                        .entry_stream(request),
                    ComparisonPolicy::default(),
                    None,
                    crate::engine::namespace::NamespacePreflight::new(&TEST_NAMESPACE),
                    |_| true,
                )
                .await
                .unwrap();
                if grouped {
                    plan.seal_hardlink_membership().await.unwrap();
                }
                let executor = LocalSyncExecutor::new(source.clone(), destination, scheduler)
                    .with_remove_source_files(true)
                    .with_hardlinks(grouped);
                let controller = SyncController::new(executor, NonZeroUsize::new(1).unwrap());
                cancel_removal(&controller, plan, reached_rx, resume_tx).await
            };
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains(&format!(
                    "{} confirmed unlinks",
                    skip as u64 + u64::from(admitted)
                )),
                "lost confirmed removal outcome: {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("current native removal uncertain: false"),
                "joined cutoff should not invent an uncertain unlink: {error}"
            );
            assert_eq!(source_dir.path().join("file").exists(), !admitted);
            if grouped {
                assert_eq!(source_dir.path().join("other").exists(), skip == 0);
            }
            assert_eq!(
                std::fs::read(destination_dir.path().join("file")).unwrap(),
                b"source"
            );
            source.validate().await.unwrap();
        }
    }
}

// Change a later destination while an earlier SOURCE unlink is admitted.
// The group and standalone lanes used to have separate counters and only the
// standalone lane repeated destination validation after its complete gate.
#[tokio::test]
async fn local_and_push_late_destination_races_preserve_candidates_and_run_counts() {
    for push in [false, true] {
        for (skip, changed_name) in [(0, "other"), (1, "z-standalone")] {
            let source_dir = tempfile::tempdir().unwrap();
            let destination_dir = tempfile::tempdir().unwrap();
            std::fs::write(source_dir.path().join("file"), b"source").unwrap();
            std::fs::hard_link(
                source_dir.path().join("file"),
                source_dir.path().join("other"),
            )
            .unwrap();
            std::fs::write(source_dir.path().join("z-standalone"), b"standalone").unwrap();
            let source = SourceRoot::open(source_dir.path().to_path_buf())
                .await
                .unwrap();
            let request = ScanRequest {
                metadata: EntryMetadataRequest {
                    unix_mode: true,
                    hardlink_group: true,
                    ..Default::default()
                },
                ..Default::default()
            };
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            source.rooted().pause_mutation_at(
                skip,
                PublicationPause {
                    point: PublicationPausePoint::AfterAdmission,
                    reached: reached_tx,
                    resume: resume_rx,
                },
            );
            let scheduler = Scheduler::new(ResourceBudget::default()).unwrap();
            let error = if push {
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
                    Operation::Push,
                    destination_dir.path(),
                    Default::default(),
                )
                .await
                .unwrap();
                let remote = client.request_handle();
                let namespace = remote.begin_namespace_qualification().await.unwrap();
                let mut plan = preflight_sync(
                    source.entries(request),
                    remote.scan(request).await.unwrap(),
                    ComparisonPolicy::default(),
                    None,
                    crate::engine::namespace::NamespacePreflight::new(namespace.as_ref()),
                    |_| true,
                )
                .await
                .unwrap();
                drop(namespace);
                plan.seal_hardlink_membership().await.unwrap();
                let executor = crate::remote::push::RemotePushExecutor::new(
                    source.clone(),
                    remote,
                    scheduler,
                    Default::default(),
                )
                .with_remove_source_files(true)
                .with_hardlinks(true);
                let controller = SyncController::new(executor, NonZeroUsize::new(1).unwrap());
                let error = race_later_destination(
                    &controller,
                    plan,
                    reached_rx,
                    resume_tx,
                    destination_dir.path(),
                    changed_name,
                )
                .await;
                drop(client);
                let server_result = tokio::time::timeout(Duration::from_secs(5), server)
                    .await
                    .unwrap()
                    .unwrap();
                // This deliberately failed DEST verification is a terminal
                // handler rejection, not a waived SOURCE termination error.
                let expected =
                    crate::rooted_fs::RootedFsError::DestinationChanged(changed_name.into())
                        .to_string();
                assert!(
                    matches!(server_result,
                    Err(crate::remote::serve::ServeError::Request(ref message)) if message == &expected),
                    "{server_result:?}"
                );
                error
            } else {
                let destination = LocalEndpoint::new(destination_dir.path().to_path_buf());
                let mut plan = preflight_sync(
                    source.entries(request),
                    destination
                        .rooted_fs(false)
                        .await
                        .unwrap()
                        .entry_stream(request),
                    ComparisonPolicy::default(),
                    None,
                    crate::engine::namespace::NamespacePreflight::new(&TEST_NAMESPACE),
                    |_| true,
                )
                .await
                .unwrap();
                plan.seal_hardlink_membership().await.unwrap();
                let executor = LocalSyncExecutor::new(source.clone(), destination, scheduler)
                    .with_remove_source_files(true)
                    .with_hardlinks(true);
                let controller = SyncController::new(executor, NonZeroUsize::new(1).unwrap());
                race_later_destination(
                    &controller,
                    plan,
                    reached_rx,
                    resume_tx,
                    destination_dir.path(),
                    changed_name,
                )
                .await
            };
            let ControllerError::Backend {
                source: backend, ..
            } = &error
            else {
                panic!("expected removal failure: {error:?}");
            };
            let io = if push {
                let Some(crate::remote::push::RemotePushError::SourceRemoval(_, io)) =
                    backend.downcast_ref::<crate::remote::push::RemotePushError>()
                else {
                    panic!("lost PUSH removal report: {error:?}");
                };
                io
            } else {
                let Some(crate::remote::local_executor::LocalSyncError::Source(_, io)) =
                    backend.downcast_ref::<crate::remote::local_executor::LocalSyncError>()
                else {
                    panic!("lost LOCAL removal report: {error:?}");
                };
                io
            };
            let report = io
                .get_ref()
                .unwrap()
                .downcast_ref::<crate::engine::source_removals::SourceRemovalError>()
                .unwrap();
            assert_eq!(report.removed, skip as u64 + 1, "{error}");
            assert!(!report.removal_uncertain, "{error}");
            assert!(!source_dir.path().join("file").exists());
            assert_eq!(source_dir.path().join("other").exists(), skip == 0);
            assert!(source_dir.path().join("z-standalone").exists());
            assert!(source_dir.path().join(changed_name).exists());
            assert_eq!(
                std::fs::read(destination_dir.path().join(changed_name)).unwrap(),
                b"foreign"
            );
        }
    }
}

// A current-thread worker sends this only after joining the real journal.
// There is no await after the send: the execution task finishes before the
// interrupt waiter wakes. Both select branches are ready, not an admission pause.
struct SignalCompletedDrain<E> {
    inner: E,
    completed: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl<E: SyncPlanExecutor> SyncPlanExecutor for SignalCompletedDrain<E> {
    type Action = E::Action;
    type Error = E::Error;

    fn close_mutation_admission(&self) {
        self.inner.close_mutation_admission();
    }
    fn cancel_mutations(&self) {
        self.inner.cancel_mutations();
    }
    fn lower(
        &self,
        op: SyncOp,
        policy: ExecutionPolicy,
    ) -> std::result::Result<Option<WorkItem<Self::Action>>, Self::Error> {
        self.inner.lower(op, policy)
    }
    fn is_directory_action(&self, action: &Self::Action) -> bool {
        self.inner.is_directory_action(action)
    }
    async fn prepare_destination_for_create(
        &self,
        create: &RelativePath,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.prepare_destination_for_create(create).await
    }
    async fn execute(
        &self,
        item: WorkItem<Self::Action>,
    ) -> std::result::Result<WorkResult, Self::Error> {
        self.inner.execute(item).await
    }
    async fn execute_delete(&self, action: DeleteAction) -> std::result::Result<(), Self::Error> {
        self.inner.execute_delete(action).await
    }
    async fn execute_finalize(
        &self,
        metadata: FinalizeMetadata,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.execute_finalize(metadata).await
    }
    async fn finish_deferred_source_removals(&self) -> std::result::Result<(), Self::Error> {
        self.inner.finish_deferred_source_removals().await
    }
    async fn close_source_removals(&self) -> std::result::Result<u64, Self::Error> {
        let count = self.inner.close_source_removals().await?;
        self.completed
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(())
            .unwrap();
        Ok(count)
    }
    async fn remove_unchanged_source(
        &self,
        source: &Entry,
        destination: &Entry,
        policy: ExecutionPolicy,
    ) -> std::result::Result<(), Self::Error> {
        self.inner
            .remove_unchanged_source(source, destination, policy)
            .await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completed_worker_and_ready_interrupt_preserve_confirmed_removal_count() {
    for interrupt in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let destination_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("file"), b"source").unwrap();
        let source = SourceRoot::open(source_dir.path().to_path_buf())
            .await
            .unwrap();
        let destination = LocalEndpoint::new(destination_dir.path().to_path_buf());
        let request = ScanRequest {
            metadata: EntryMetadataRequest {
                unix_mode: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let plan = preflight_sync(
            source.entries(request),
            destination
                .rooted_fs(false)
                .await
                .unwrap()
                .entry_stream(request),
            ComparisonPolicy::default(),
            None,
            crate::engine::namespace::NamespacePreflight::new(&TEST_NAMESPACE),
            |_| true,
        )
        .await
        .unwrap();
        let (completed, ready) = tokio::sync::oneshot::channel();
        let inner = LocalSyncExecutor::new(
            source,
            destination,
            Scheduler::new(ResourceBudget::default()).unwrap(),
        )
        .with_remove_source_files(true);
        let controller = SyncController::new(
            SignalCompletedDrain {
                inner,
                completed: std::sync::Mutex::new(Some(completed)),
            },
            NonZeroUsize::new(1).unwrap(),
        );
        let result = controller
            .execute_until(plan, async {
                if interrupt {
                    ready.await.unwrap();
                    Ok(())
                } else {
                    // Retain the receiver until success, without waking the stop branch.
                    let _ready = ready;
                    std::future::pending().await
                }
            })
            .await;
        if interrupt {
            assert!(
                matches!(
                    result,
                    Err(ControllerError::Cancelled { removed_sources: 1 })
                ),
                "{result:?}"
            );
        } else {
            assert_eq!(result.unwrap().removed_sources, 1);
        }
        assert!(!source_dir.path().join("file").exists());
        assert_eq!(
            std::fs::read(destination_dir.path().join("file")).unwrap(),
            b"source"
        );
    }
}

async fn race_later_destination<E: SyncPlanExecutor>(
    controller: &SyncController<E>,
    plan: SyncPlan,
    reached: tokio::sync::oneshot::Receiver<()>,
    resume: std::sync::mpsc::Sender<()>,
    destination: &std::path::Path,
    changed_name: &str,
) -> ControllerError {
    let execution = controller.execute(plan);
    tokio::pin!(execution);
    tokio::select! {
        result = &mut execution => panic!("removal never reached native barrier: {result:?}"),
        reached = reached => reached.unwrap(),
    }
    // Rename a distinct inode over the later name, not an in-place edit of the
    // whole destination group. Its original proof must remain authoritative.
    std::fs::write(destination.join("foreign"), b"foreign").unwrap();
    std::fs::rename(destination.join("foreign"), destination.join(changed_name)).unwrap();
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), execution)
        .await
        .unwrap()
        .unwrap_err()
}

async fn cancel_removal<E: SyncPlanExecutor>(
    controller: &SyncController<E>,
    plan: SyncPlan,
    reached: tokio::sync::oneshot::Receiver<()>,
    resume: std::sync::mpsc::Sender<()>,
) -> Result<Option<SyncSummary>> {
    {
        let execution = controller.execute(plan);
        tokio::pin!(execution);
        tokio::select! {
            result = &mut execution => panic!("removal never reached native barrier: {result:?}"),
            reached = reached => reached.unwrap(),
        }
    }
    let mut close = Box::pin(controller.close());
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut close)
        .await
        .is_err());
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), close)
        .await
        .unwrap()
}
