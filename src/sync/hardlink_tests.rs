//! Hardlink semantics through the real direction adapters.

/// Preexisting aliases outside the selected source root have the same leaf
/// observations. Only continuity of the original root distinguishes scopes.
pub(super) struct SourceRootSwap {
    parent: tempfile::TempDir,
    pub destination: tempfile::TempDir,
    pub source: crate::endpoint::source_root::SourceRoot,
    pub plan: Option<crate::engine::controller::SyncPlan>,
    observed: crate::engine::domain::EntryIdentity,
}

impl SourceRootSwap {
    pub async fn new() -> Self {
        let parent = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let a = parent.path().join("a-root");
        let b = parent.path().join("b-root");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("first"), b"source bytes").unwrap();
        for path in [a.join("second"), b.join("first"), b.join("second")] {
            std::fs::hard_link(a.join("first"), path).unwrap();
        }
        let source = crate::endpoint::source_root::SourceRoot::open(a)
            .await
            .unwrap();
        let request = crate::engine::scan::ScanRequest {
            respect_gitignore: false,
            include_git_dir: true,
            metadata: crate::engine::scan::EntryMetadataRequest {
                identity: true,
                unix_mode: true,
                hardlink_group: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut plan = crate::engine::controller::preflight_sync(
            source.entries(request),
            crate::endpoint::local_entry_scan::local_entry_stream(
                destination.path().into(),
                request,
            ),
            Default::default(),
            None,
            crate::engine::namespace::NamespacePreflight::new(
                &crate::engine::namespace::FixedNamespaceQualification(
                    crate::engine::namespace::NamespaceSemantics::UNSPECIFIED,
                ),
            ),
            |_| false,
        )
        .await
        .unwrap();
        plan.seal_hardlink_membership().await.unwrap();
        let observed = source
            .rooted()
            .path_identity_blocking(&crate::engine::domain::RelativePath::new("first").unwrap())
            .unwrap()
            .unwrap()
            .1;
        Self {
            parent,
            destination,
            source,
            plan: Some(plan),
            observed,
        }
    }

    pub fn paths(&self) -> [PathBuf; 3] {
        [
            self.parent.path().join("a-root"),
            self.parent.path().join("b-root"),
            self.parent.path().join("held-root"),
        ]
    }

    pub fn swap(&self) {
        swap_source_roots(&self.paths());
    }

    pub fn assert_publications(&self, after_publication: bool) {
        if after_publication {
            for name in ["first", "second"] {
                assert_eq!(
                    std::fs::read(self.destination.path().join(name)).unwrap(),
                    b"source bytes"
                );
            }
            assert_eq!(
                std::fs::metadata(self.destination.path().join("first"))
                    .unwrap()
                    .ino(),
                std::fs::metadata(self.destination.path().join("second"))
                    .unwrap()
                    .ino()
            );
        }
        assert_eq!(
            std::fs::read_dir(self.destination.path()).unwrap().count(),
            if after_publication { 2 } else { 0 }
        );
    }

    pub fn assert_sources_intact(&self) {
        for root in [
            self.parent.path().join("held-root"),
            self.parent.path().join("a-root"),
        ] {
            for name in ["first", "second"] {
                let path = root.join(name);
                assert_eq!(std::fs::read(&path).unwrap(), b"source bytes");
                let observed = crate::endpoint::local_identity::metadata_identity(
                    &std::fs::metadata(path).unwrap(),
                    crate::engine::domain::EntryKind::File,
                )
                .unwrap();
                assert_eq!(
                    observed, self.observed,
                    "source aliases were modified or removed"
                );
            }
        }
    }
}

pub(super) fn is_original_root_change(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<crate::error::SyncError>() {
        return matches!(error, crate::error::SyncError::SourceChanged { .. });
    }
    if let Some(crate::remote::local_executor::LocalSyncError::Endpoint(error)) =
        error.downcast_ref::<crate::remote::local_executor::LocalSyncError>()
    {
        return is_original_root_change(error);
    }
    if let Some(error) = error.downcast_ref::<crate::rooted_fs::RootedFsError>() {
        return matches!(error, crate::rooted_fs::RootedFsError::RootChanged(_));
    }
    if let Some(crate::remote::local_executor::LocalSyncError::Existing(
        crate::endpoint::existing::ExistingDestinationError::Rooted(error),
    )) = error.downcast_ref::<crate::remote::local_executor::LocalSyncError>()
    {
        return is_original_root_change(error);
    }
    if let Some(crate::remote::local_executor::LocalSyncError::Rooted(error)) =
        error.downcast_ref::<crate::remote::local_executor::LocalSyncError>()
    {
        return is_original_root_change(error);
    }
    if let Some(crate::remote::push::RemotePushError::Directory(error)) =
        error.downcast_ref::<crate::remote::push::RemotePushError>()
    {
        return is_original_root_change(error);
    }
    if let Some(
        crate::remote::local_executor::LocalSyncError::Destination(_, error)
        | crate::remote::local_executor::LocalSyncError::Source(_, error),
    ) = error.downcast_ref::<crate::remote::local_executor::LocalSyncError>()
    {
        return is_original_root_change(error);
    }
    if let Some(crate::remote::push::RemotePushError::Remote(error)) =
        error.downcast_ref::<crate::remote::push::RemotePushError>()
    {
        return is_original_root_change(error);
    }
    if let Some(crate::remote::runtime::RemoteSessionError::Transfer(error)) =
        error.downcast_ref::<crate::remote::runtime::RemoteSessionError>()
    {
        return is_original_root_change(error);
    }
    if let Some(crate::remote::transfer::RemoteTransferError::RootedFs(error)) =
        error.downcast_ref::<crate::remote::transfer::RemoteTransferError>()
    {
        return is_original_root_change(error);
    }
    if let Some(error) = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
    {
        return is_original_root_change(error);
    }
    error.source().is_some_and(is_original_root_change)
}

fn swap_source_roots(paths: &[PathBuf; 3]) {
    std::fs::rename(&paths[0], &paths[2]).unwrap();
    std::fs::rename(&paths[1], &paths[0]).unwrap();
}

/// Insert the root replacement at the real controller's final removal seam,
/// after all grouped publication receipts have already been recorded.
pub(super) struct SwapBeforeSourceRemovals<E> {
    pub inner: E,
    pub paths: Option<[PathBuf; 3]>,
}

impl<E: crate::engine::controller::SyncPlanExecutor> crate::engine::controller::SyncPlanExecutor
    for SwapBeforeSourceRemovals<E>
{
    type Action = E::Action;
    type Error = E::Error;

    fn close_mutation_admission(&self) {
        self.inner.close_mutation_admission();
    }
    fn cancel_mutations(&self) {
        self.inner.cancel_mutations();
    }
    async fn prepare_destination_for_create(
        &self,
        create: &crate::engine::domain::RelativePath,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.prepare_destination_for_create(create).await
    }
    async fn close_source_removals(&self) -> std::result::Result<u64, Self::Error> {
        self.inner.close_source_removals().await
    }

    fn lower(
        &self,
        op: crate::engine::domain::SyncOp,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<Option<crate::engine::work::WorkItem<Self::Action>>, Self::Error> {
        self.inner.lower(op, policy)
    }
    fn is_directory_action(&self, action: &Self::Action) -> bool {
        self.inner.is_directory_action(action)
    }
    fn supports_whole_hardlink_groups(&self) -> bool {
        self.inner.supports_whole_hardlink_groups()
    }
    async fn execute_hardlink_group(
        &self,
        group: &mut crate::engine::hardlink_runtime::HardlinkGroupPlan<'_>,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<Option<()>, Self::Error> {
        self.inner.execute_hardlink_group(group, policy).await
    }
    async fn execute(
        &self,
        item: crate::engine::work::WorkItem<Self::Action>,
    ) -> std::result::Result<crate::engine::work::WorkResult, Self::Error> {
        self.inner.execute(item).await
    }
    async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.execute_delete(action).await
    }
    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.execute_finalize(metadata).await
    }
    async fn finish_deferred_source_removals(&self) -> std::result::Result<(), Self::Error> {
        if let Some(paths) = &self.paths {
            swap_source_roots(paths);
        }
        self.inner.finish_deferred_source_removals().await
    }
    async fn remove_unchanged_source(
        &self,
        source: &crate::engine::domain::Entry,
        destination: &crate::engine::domain::Entry,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<(), Self::Error> {
        self.inner
            .remove_unchanged_source(source, destination, policy)
            .await
    }
}

use super::{SyncConfig, SyncStats};
use crate::error::Result;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

pub(super) async fn remote_session(
    operation: sy::protocol::Operation,
    root: &std::path::Path,
) -> (
    sy::remote::runtime::ClientRemoteSession,
    tokio::task::JoinHandle<sy::remote::serve::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let server = tokio::spawn(sy::remote::serve::serve_transport(
        reader,
        writer,
        Default::default(),
    ));
    let (reader, writer) = tokio::io::split(client);
    let client = sy::remote::runtime::ClientRemoteSession::connect(
        reader,
        writer,
        operation,
        root,
        Default::default(),
    )
    .await
    .unwrap();
    (client, server)
}

pub(super) async fn finish_session(
    client: sy::remote::runtime::ClientRemoteSession,
    server: tokio::task::JoinHandle<sy::remote::serve::Result<()>>,
) {
    drop(client);
    let result = server.await.unwrap();
    if let Err(error) = result {
        assert!(
            matches!(&error, sy::remote::serve::ServeError::Session(
            sy::remote::runtime::RemoteSessionError::Router(error)
        ) if matches!(error.as_ref(), sy::remote::router::RouterError::TransportEof)),
            "{error:?}"
        );
    }
}

pub(super) async fn assert_destination_alias_deletion<F, Fut>(mut execute: F)
where
    F: FnMut(PathBuf, PathBuf, SyncConfig) -> Fut,
    Fut: Future<Output = Result<SyncStats>>,
{
    for backup in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(destination.path().join("a"), b"old bytes").unwrap();
        for name in ["b", "c"] {
            std::fs::hard_link(destination.path().join("a"), destination.path().join(name))
                .unwrap();
        }
        let mut config = SyncConfig::test_default();
        config.delete = super::DeleteMode::Enabled {
            limit: crate::engine::delete_plan::DeleteLimit::Count(3),
            force: false,
        };
        if backup {
            config.backup = Some(String::new());
        }
        let stats = execute(source.path().into(), destination.path().into(), config)
            .await
            .unwrap();
        assert_eq!(stats.files_deleted, 3);
        for name in ["a", "b", "c"] {
            assert!(
                !destination.path().join(name).exists(),
                "alias {name} survived deletion"
            );
            if backup {
                assert_eq!(
                    std::fs::read(destination.path().join(format!("{name}~"))).unwrap(),
                    b"old bytes"
                );
            }
        }
        assert_eq!(
            std::fs::read_dir(destination.path()).unwrap().count(),
            if backup { 3 } else { 0 },
            "unexpected entries or private staging leaked"
        );
        assert!(std::fs::read_dir(source.path()).unwrap().next().is_none());
    }
}
