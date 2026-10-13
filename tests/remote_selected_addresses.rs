//! Selected plans execute over the real v3 router with independent physical
//! addresses. Original observations come from the actual SOURCE producers;
//! CLI operand resolution is deliberately outside this contract.
#![cfg(unix)]

use futures::TryStreamExt;
use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;
use sy::endpoint::local_entry_scan::selected_leaf_stream;
use sy::engine::controller::{preflight_sync_scoped_with_content, SyncController, SyncPlan};
use sy::engine::delete_plan::{DeleteLimit, DeletePolicy};
use sy::engine::domain::{RelativePath, SyncOp, SyncScope};
use sy::engine::planner::{ComparisonMode, ComparisonPolicy};
use sy::engine::reconcile::OrderedReconciler;
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::Scheduler;
use sy::protocol::Operation;
use sy::remote::pull::RemotePullExecutor;
use sy::remote::push::{RemoteBackupPlan, RemotePushExecutor};
use sy::remote::runtime::ClientRemoteSession;

#[derive(Clone, Copy, Debug)]
enum Case {
    Create,
    Update,
    Delta,
    Replace,
    Metadata,
    Unchanged,
    Noop,
    Symlink,
    RemoveCreated,
    RemoveExistingEqual,
    RemoveExistingUnequal,
}

impl Case {
    fn removes(self) -> bool {
        matches!(
            self,
            Self::RemoveCreated | Self::RemoveExistingEqual | Self::RemoveExistingUnequal
        )
    }
    fn preserves_xattrs(self) -> bool {
        self.retains() && !matches!(self, Self::Noop)
    }
    fn retains(self) -> bool {
        matches!(
            self,
            Self::Metadata
                | Self::Unchanged
                | Self::Noop
                | Self::RemoveExistingEqual
                | Self::RemoveExistingUnequal
        )
    }
}

fn relative(name: &str) -> RelativePath {
    RelativePath::new(name).unwrap()
}

fn seed(path: &Path, bytes: &[u8], mode: u32) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_600_000_000, 123)).unwrap();
}

async fn selected_plan(
    source: &sy::endpoint::source_root::SourceRoot,
    destination: &Path,
    case: Case,
) -> SyncPlan {
    let mut request = ScanRequest::default();
    request.metadata.unix_mode = true;
    let reconciler = OrderedReconciler::with_scope(
        source.selected_entries(relative("original"), request, false),
        selected_leaf_stream(destination.into(), relative("renamed"), request, true),
        SyncScope::SelectedLeaf {
            source: relative("original"),
            destination: relative("renamed"),
        },
    );
    let rooted = sy::rooted_fs::RootedFs::open(destination.into())
        .await
        .unwrap();
    let (_rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    let mut plan = preflight_sync_scoped_with_content(
        reconciler,
        ComparisonPolicy {
            mode: if case.retains() {
                ComparisonMode::SizeOnly
            } else {
                ComparisonMode::Always
            },
            preserve_permissions: matches!(case, Case::Metadata),
            ..Default::default()
        },
        Some(DeletePolicy {
            limit: DeleteLimit::Unlimited,
            force: false,
        }),
        sy::engine::namespace::NamespacePreflight::new(&namespace),
        |_| true,
        |_| futures::future::ready(Ok(true)),
        |_, _| async { panic!("size-only/always plans do not request content comparison") },
    )
    .await
    .unwrap();
    assert_eq!(plan.operations(), 1);
    assert_eq!(
        plan.delete_candidates(),
        0,
        "a selected source cannot authorize sibling deletion"
    );
    plan.validate_operations(|op| async move {
        assert_eq!(op.path(), &relative("renamed"));
        assert_eq!(op.source().path, relative("original"));
        let correct = match case {
            Case::Create | Case::RemoveCreated | Case::Symlink => {
                matches!(op, SyncOp::Create { .. })
            }
            Case::Update | Case::Delta => matches!(op, SyncOp::Update { .. }),
            Case::Replace => matches!(op, SyncOp::Replace { .. }),
            Case::Metadata => matches!(op, SyncOp::Metadata { .. }),
            _ => matches!(op, SyncOp::Unchanged { .. }),
        };
        assert!(correct, "case {case:?}: {op:?}");
        Ok(())
    })
    .await
    .unwrap();
    plan
}

#[tokio::test]
async fn selected_push_uses_destination_addresses_without_adopting_source_names() {
    tokio::time::timeout(Duration::from_secs(30), selected_address_matrix(true))
        .await
        .unwrap();
}

#[tokio::test]
async fn selected_pull_uses_destination_addresses_without_adopting_source_names() {
    tokio::time::timeout(Duration::from_secs(30), selected_address_matrix(false))
        .await
        .unwrap();
}

async fn selected_address_matrix(push: bool) {
    for case in [
        Case::Create,
        Case::Update,
        Case::Delta,
        Case::Replace,
        Case::Metadata,
        Case::Unchanged,
        Case::Noop,
        Case::Symlink,
        Case::RemoveCreated,
        Case::RemoveExistingEqual,
        Case::RemoveExistingUnequal,
    ] {
        if !push && case.removes() {
            continue;
        } // Pull source removal is not implemented.
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let source_path = source.path().join("original");
        let dest_path = destination.path().join("renamed");
        seed(&source.path().join("renamed"), b"source sibling", 0o600);
        // This is also a tempting but WRONG delta basis for the original source name.
        seed(
            &destination.path().join("original"),
            b"destination sibling",
            0o600,
        );
        let sibling_before = std::fs::metadata(destination.path().join("original")).unwrap();
        let mut bytes = b"source".to_vec();
        let old = match case {
            Case::Delta => {
                // Enough independent blocks for a real rolling-signature candidate.
                bytes = (0..256 * 1024).map(|i| (i / 4096) as u8).collect();
                let old = bytes.clone();
                bytes[16 * 4096..17 * 4096].fill(0xff);
                Some(old)
            }
            Case::Update => Some(b"old payload".to_vec()),
            Case::Metadata | Case::Unchanged | Case::Noop | Case::RemoveExistingUnequal => {
                Some(b"target".to_vec())
            }
            Case::RemoveExistingEqual => Some(bytes.clone()),
            _ => None,
        };
        if matches!(case, Case::Symlink) {
            use std::os::unix::ffi::OsStringExt;
            // Opaque target bytes must not be resolved or rewritten relative to the new name.
            let target = std::ffi::OsString::from_vec(b"../missing/\x80".to_vec());
            std::os::unix::fs::symlink(target, &source_path).unwrap();
        } else {
            seed(&source_path, &bytes, 0o640);
        }
        if matches!(case, Case::Replace) {
            std::os::unix::fs::symlink("original", &dest_path).unwrap();
        }
        if let Some(old) = &old {
            seed(
                &dest_path,
                old,
                if matches!(case, Case::Metadata) {
                    0o600
                } else {
                    0o640
                },
            );
        }
        let before = std::fs::symlink_metadata(&dest_path).ok();
        if case.preserves_xattrs() {
            xattr::set(&source_path, "user.sy-selected", b"preserved").unwrap();
            xattr::set(&dest_path, "user.sy-stale", b"remove").unwrap();
        }
        let authority = sy::endpoint::source_root::SourceRoot::open(source.path().into())
            .await
            .unwrap();
        let plan = selected_plan(&authority, destination.path(), case).await;
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(server_io);
        let server = tokio::spawn(sy::remote::serve::serve_transport(
            reader,
            writer,
            Default::default(),
        ));
        let (reader, writer) = tokio::io::split(client_io);
        let client = ClientRemoteSession::connect(
            reader,
            writer,
            if push {
                Operation::Push
            } else {
                Operation::Pull
            },
            if push {
                destination.path()
            } else {
                source.path()
            },
            Default::default(),
        )
        .await
        .unwrap();
        let scheduler = Scheduler::new(Default::default()).unwrap();
        let summary = if push {
            let namespace = client
                .request_handle()
                .begin_namespace_qualification()
                .await
                .unwrap();
            namespace
                .qualify(sy::engine::namespace::NamespaceParent::Root)
                .await
                .unwrap();
            namespace.seal().await.unwrap();
            let executor = RemotePushExecutor::new(
                authority,
                client.request_handle(),
                scheduler,
                Default::default(),
            )
            .with_delta_min_size(1)
            .with_xattrs(case.preserves_xattrs())
            .with_remove_source_files(case.removes())
            .with_backup(Some(RemoteBackupPlan {
                suffix: "~".into(),
                dir: None,
            }));
            SyncController::new(executor, NonZeroUsize::new(2).unwrap())
                .execute(plan)
                .await
        } else {
            client
                .request_handle()
                .scan_entry(ScanRequest::default(), relative("original"))
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            let mut binding = sy::rooted_fs::operand::BoundDestination::open(
                destination.path().into(),
                sy::rooted_fs::operand::SourceShape::Directory { basename: None },
            )
            .await
            .unwrap();
            let (root, namespace) = binding.root.with_namespace_qualification().await.unwrap();
            use sy::engine::namespace::NamespaceQualification;
            namespace
                .qualify(sy::engine::namespace::NamespaceParent::Root)
                .await
                .unwrap();
            namespace.seal().await.unwrap();
            binding.root = root;
            let executor = RemotePullExecutor::new(
                destination.path().into(),
                client.request_handle(),
                client.sender(),
                scheduler,
            )
            .with_destination_binding(binding)
            .with_xattrs(case.preserves_xattrs())
            .with_backup_enabled(true)
            .with_backup_suffix("~".into());
            SyncController::new(executor, NonZeroUsize::new(2).unwrap())
                .execute(plan)
                .await
        };
        let context = format!("push={push} case={case:?}");
        if matches!(case, Case::RemoveExistingUnequal) {
            assert!(
                summary.is_err(),
                "quick equality must not authorize source removal: {context}"
            );
            assert!(source_path.exists());
        } else {
            let summary = summary.unwrap_or_else(|error| panic!("{context}: {error}"));
            assert_eq!(summary.delete_candidates, 0, "{context}");
            if push && matches!(case, Case::Delta) {
                assert!(
                    summary.reused_bytes > 0,
                    "delta must use the old destination: {context}"
                );
                assert!(summary.literal_bytes < bytes.len() as u64, "{context}");
            }
            assert_eq!(
                std::fs::symlink_metadata(&source_path).is_ok(),
                !case.removes(),
                "{context}"
            );
        }
        if matches!(case, Case::Symlink) {
            assert_eq!(
                std::fs::read_link(&dest_path).unwrap(),
                std::fs::read_link(&source_path).unwrap(),
                "{context}"
            );
        } else {
            let expected = if case.retains() {
                old.as_ref().unwrap()
            } else {
                &bytes
            };
            assert_eq!(&std::fs::read(&dest_path).unwrap(), expected, "{context}");
            if case.retains() {
                let after = std::fs::metadata(&dest_path).unwrap();
                assert_eq!(after.ino(), before.as_ref().unwrap().ino(), "{context}");
                assert_eq!(after.mode() & 0o7777, 0o640, "{context}");
            }
            if case.preserves_xattrs() {
                assert_eq!(
                    xattr::get(&dest_path, "user.sy-selected").unwrap(),
                    Some(b"preserved".to_vec()),
                    "{context}"
                );
                assert_eq!(
                    xattr::get(&dest_path, "user.sy-stale").unwrap(),
                    None,
                    "{context}"
                );
            }
        }
        if matches!(case, Case::Update | Case::Delta) {
            assert_eq!(
                std::fs::read(destination.path().join("renamed~")).unwrap(),
                old.unwrap(),
                "{context}"
            );
        } else {
            assert!(!destination.path().join("renamed~").exists(), "{context}");
        }
        assert!(!destination.path().join("original~").exists(), "{context}");
        assert_eq!(
            std::fs::read(destination.path().join("original")).unwrap(),
            b"destination sibling",
            "{context}"
        );
        assert_eq!(
            std::fs::metadata(destination.path().join("original"))
                .unwrap()
                .ino(),
            sibling_before.ino(),
            "{context}"
        );
        assert_eq!(
            std::fs::read(source.path().join("renamed")).unwrap(),
            b"source sibling",
            "{context}"
        );
        if source_path.exists() && !matches!(case, Case::Symlink) {
            assert_eq!(std::fs::read(&source_path).unwrap(), bytes, "{context}");
        }
        drop(client);
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

/// A selected hardlink member keeps its physical source address while its
/// singleton catalogue publishes at the independently selected destination.
/// Multi-member inode fidelity is covered by whole-group controller tests;
/// arbitrary per-member rename projection is not a supported scan scope.
#[tokio::test]
async fn selected_hardlink_member_uses_physical_source_and_renamed_destination() {
    for push in [true, false] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        seed(&source.path().join("a"), b"group bytes", 0o640);
        std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
        for name in ["a", "b"] {
            seed(&destination.path().join(name), b"untouched sibling", 0o600);
        }
        let mut request = ScanRequest::default();
        request.metadata.unix_mode = true;
        request.metadata.hardlink_group = true;
        let authority = sy::endpoint::source_root::SourceRoot::open(source.path().into())
            .await
            .unwrap();
        let reconciler = OrderedReconciler::with_scope(
            authority.selected_entries(relative("a"), request, false),
            selected_leaf_stream(
                destination.path().into(),
                relative("renamed-a"),
                request,
                true,
            ),
            SyncScope::SelectedLeaf {
                source: relative("a"),
                destination: relative("renamed-a"),
            },
        );
        let rooted = sy::rooted_fs::RootedFs::open(destination.path().into())
            .await
            .unwrap();
        let (_rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        let mut plan = preflight_sync_scoped_with_content(
            reconciler,
            Default::default(),
            None,
            sy::engine::namespace::NamespacePreflight::new(&namespace),
            |_| true,
            |_| futures::future::ready(Ok(false)),
            |_, _| async { panic!("new leaf requires no content comparison") },
        )
        .await
        .unwrap();
        plan.validate_operations(|operation| async move {
            assert!(operation.source().hardlink_group.is_some());
            assert_eq!(operation.source().path, relative("a"));
            assert_eq!(operation.path(), &relative("renamed-a"));
            Ok(())
        })
        .await
        .unwrap();
        plan.seal_hardlink_membership().await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(server_io);
        let server = tokio::spawn(sy::remote::serve::serve_transport(
            reader,
            writer,
            Default::default(),
        ));
        let (reader, writer) = tokio::io::split(client_io);
        let client = ClientRemoteSession::connect(
            reader,
            writer,
            if push {
                Operation::Push
            } else {
                Operation::Pull
            },
            if push {
                destination.path()
            } else {
                source.path()
            },
            Default::default(),
        )
        .await
        .unwrap();
        let scheduler = Scheduler::new(Default::default()).unwrap();
        let execution = async {
            if push {
                let namespace = client
                    .request_handle()
                    .begin_namespace_qualification()
                    .await
                    .unwrap();
                namespace
                    .qualify(sy::engine::namespace::NamespaceParent::Root)
                    .await
                    .unwrap();
                namespace.seal().await.unwrap();
                SyncController::new(
                    RemotePushExecutor::new(
                        authority,
                        client.request_handle(),
                        scheduler,
                        Default::default(),
                    )
                    .with_hardlinks(true)
                    .with_remove_source_files(true),
                    NonZeroUsize::new(2).unwrap(),
                )
                .execute(plan)
                .await
                .unwrap()
            } else {
                client
                    .request_handle()
                    .scan_entry(request, relative("a"))
                    .await
                    .unwrap()
                    .try_collect::<Vec<_>>()
                    .await
                    .unwrap();
                SyncController::new(
                    RemotePullExecutor::new(
                        destination.path().into(),
                        client.request_handle(),
                        client.sender(),
                        scheduler,
                    )
                    .with_hardlinks(true),
                    NonZeroUsize::new(2).unwrap(),
                )
                .execute(plan)
                .await
                .unwrap()
            }
        };
        let summary = tokio::time::timeout(Duration::from_secs(10), execution)
            .await
            .unwrap();
        assert_eq!(summary.files_created, 1);
        assert_eq!(summary.literal_bytes, 11);
        assert_eq!(
            std::fs::metadata(destination.path().join("renamed-a"))
                .unwrap()
                .nlink(),
            1
        );
        assert_eq!(
            std::fs::read(destination.path().join("renamed-a")).unwrap(),
            b"group bytes"
        );
        for name in ["a", "b"] {
            assert_eq!(
                std::fs::read(destination.path().join(name)).unwrap(),
                b"untouched sibling"
            );
        }
        assert_eq!(source.path().join("a").exists(), !push);
        assert_eq!(
            std::fs::read(source.path().join("b")).unwrap(),
            b"group bytes"
        );
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 3);
        drop(client);
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
