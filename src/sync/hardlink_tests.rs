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
            |_| false,
        )
        .await
        .unwrap();
        plan.validate_hardlink_bytes(|commitment| {
            let endpoint = source.endpoint();
            async move {
                super::policy::observed_hash(&endpoint, commitment.entry(), false)
                    .await
                    .map_err(|error| {
                        crate::engine::controller::ControllerError::backend(
                            "source commitment",
                            error,
                        )
                    })
            }
        })
        .await
        .unwrap();
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
use crate::error::{Result, SyncError};
use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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

pub(super) async fn assert_byte_commitments<F, Fut>(mut execute: F)
where
    F: FnMut(PathBuf, PathBuf, SyncConfig) -> Fut,
    Fut: Future<Output = Result<SyncStats>>,
{
    // Metadata/Unchanged, both mixed-intent orders, preview, explicit source
    // updates, and a selection leaving only one member of an excluded group.
    for (first_payload, second_payload, permissions, preview, checksum, exclude_second) in [
        (&b"other1"[..], &b"other2"[..], true, false, false, false),
        (&b"old"[..], &b"other2"[..], false, false, false, false),
        (&b"other1"[..], &b"old"[..], false, false, false, false),
        (&b"other1"[..], &b"other2"[..], false, true, false, false),
        (&b"other1"[..], &b"other2"[..], false, false, true, false),
        (&b"other1"[..], &b"other2"[..], false, false, false, true),
        (&b"target"[..], &b"target"[..], false, false, false, false),
    ] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("aaa-new"), b"unrelated").unwrap();
        std::fs::write(source.path().join("first"), b"source").unwrap();
        std::fs::hard_link(source.path().join("first"), source.path().join("second")).unwrap();
        std::fs::write(destination.path().join("first"), first_payload).unwrap();
        std::fs::write(destination.path().join("second"), second_payload).unwrap();
        for root in [source.path(), destination.path()] {
            for name in ["first", "second"] {
                let path = root.join(name);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
                filetime::set_file_mtime(
                    &path,
                    filetime::FileTime::from_unix_time(1_600_000_000, 123),
                )
                .unwrap();
            }
        }
        if permissions {
            std::fs::set_permissions(
                destination.path().join("first"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let before = std::fs::metadata(destination.path().join("first")).unwrap();
        let mut config = SyncConfig::test_default();
        config.preserve.hardlinks = true;
        config.preserve.permissions = permissions;
        config.dry_run = preview;
        config.comparison.checksum = checksum;
        if exclude_second {
            config.filter_engine.add_exclude("second").unwrap();
        }
        let result = execute(source.path().into(), destination.path().into(), config).await;
        let conflict = !checksum && !exclude_second && first_payload != second_payload;
        if conflict {
            let error: SyncError = result.unwrap_err();
            assert!(
                error.to_string().contains("incompatible byte commitments"),
                "{error:?}"
            );
            assert!(
                !destination.path().join("aaa-new").exists(),
                "preflight allowed an earlier unrelated write"
            );
            assert_eq!(
                std::fs::read(destination.path().join("first")).unwrap(),
                first_payload
            );
            assert_eq!(
                std::fs::read(destination.path().join("second")).unwrap(),
                second_payload
            );
            let after = std::fs::metadata(destination.path().join("first")).unwrap();
            assert_eq!(
                (
                    before.ino(),
                    before.mode(),
                    before.mtime(),
                    before.mtime_nsec(),
                    before.ctime(),
                    before.ctime_nsec(),
                    before.nlink()
                ),
                (
                    after.ino(),
                    after.mode(),
                    after.mtime(),
                    after.mtime_nsec(),
                    after.ctime(),
                    after.ctime_nsec(),
                    after.nlink()
                )
            );
        } else {
            result.unwrap();
            let expected_first: &[u8] = if checksum { b"source" } else { first_payload };
            let expected_second: &[u8] = if checksum { b"source" } else { second_payload };
            assert_eq!(
                std::fs::read(destination.path().join("first")).unwrap(),
                expected_first
            );
            assert_eq!(
                std::fs::read(destination.path().join("second")).unwrap(),
                expected_second
            );
            if checksum {
                assert_eq!(
                    std::fs::metadata(destination.path().join("first"))
                        .unwrap()
                        .ino(),
                    std::fs::metadata(destination.path().join("second"))
                        .unwrap()
                        .ino()
                );
            }
        }
        assert_eq!(
            std::fs::read(source.path().join("first")).unwrap(),
            b"source"
        );
        assert_eq!(
            std::fs::read(source.path().join("second")).unwrap(),
            b"source"
        );
    }
}
