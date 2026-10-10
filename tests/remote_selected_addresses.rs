//! Selected plans execute over the real v3 router with independent physical
//! addresses. Fixture observations are local: selected remote scan RPCs and CLI
//! operand resolution are deliberately not part of this contract.
#![cfg(unix)]

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

async fn selected_plan(source: &Path, destination: &Path, case: Case) -> SyncPlan {
    let mut request = ScanRequest::default();
    request.metadata.unix_mode = true;
    let reconciler = OrderedReconciler::with_scope(
        selected_leaf_stream(source.into(), relative("original"), request, false),
        selected_leaf_stream(destination.into(), relative("renamed"), request, true),
        SyncScope::SelectedLeaf {
            source: relative("original"),
            destination: relative("renamed"),
        },
    );
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
        |_| true,
        |_| true,
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
        let plan = selected_plan(source.path(), destination.path(), case).await;
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
            let authority = sy::endpoint::source_root::SourceRoot::open(source.path().into())
                .await
                .unwrap();
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
            let executor = RemotePullExecutor::new(
                destination.path().into(),
                client.request_handle(),
                client.sender(),
                scheduler,
            )
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

/// A group's representative is a destination address, never its source name.
/// This exercises the per-item lowering boundary without claiming full native
/// hardlink-group projection or selected remote scanning.
#[tokio::test]
async fn renamed_hardlink_members_use_the_published_destination_representative() {
    use futures::TryStreamExt;
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
        let mut members = Vec::new();
        for name in ["a", "b"] {
            let entries: Vec<_> =
                selected_leaf_stream(source.path().into(), relative(name), request, false)
                    .try_collect()
                    .await
                    .unwrap();
            assert_eq!(entries.len(), 1);
            members.extend(entries);
        }
        assert!(members[0].hardlink_group.is_some());
        assert_eq!(members[0].hardlink_group, members[1].hardlink_group);
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
                let authority = sy::endpoint::source_root::SourceRoot::open(source.path().into())
                    .await
                    .unwrap();
                execute_renamed_group(
                    RemotePushExecutor::new(
                        authority,
                        client.request_handle(),
                        scheduler,
                        Default::default(),
                    )
                    .with_hardlinks(true)
                    .with_remove_source_files(true),
                    members,
                )
                .await;
            } else {
                execute_renamed_group(
                    RemotePullExecutor::new(
                        destination.path().into(),
                        client.request_handle(),
                        client.sender(),
                        scheduler,
                    )
                    .with_hardlinks(true),
                    members,
                )
                .await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), execution)
            .await
            .unwrap();
        let first = std::fs::metadata(destination.path().join("renamed-a")).unwrap();
        let second = std::fs::metadata(destination.path().join("renamed-b")).unwrap();
        assert_eq!(first.ino(), second.ino());
        for name in ["a", "b"] {
            assert_eq!(
                std::fs::read(destination.path().join(format!("renamed-{name}"))).unwrap(),
                b"group bytes"
            );
            assert_eq!(
                std::fs::read(destination.path().join(name)).unwrap(),
                b"untouched sibling"
            );
            assert_eq!(source.path().join(name).exists(), !push);
        }
        drop(client);
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn execute_renamed_group<E: sy::engine::controller::SyncPlanExecutor>(
    executor: E,
    members: Vec<sy::engine::domain::Entry>,
) {
    for (index, source) in members.into_iter().enumerate() {
        let destination_path = relative(if index == 0 { "renamed-a" } else { "renamed-b" });
        let work = executor
            .lower(
                SyncOp::Create {
                    source,
                    destination_path,
                },
                Default::default(),
            )
            .unwrap()
            .unwrap();
        let sy::engine::work::WorkResult::Transfer(summary) = executor.execute(work).await.unwrap()
        else {
            panic!("expected hardlink transfer");
        };
        assert_eq!(
            summary.reused_bytes,
            if index == 0 { 0 } else { summary.file_size }
        );
    }
    executor.finish_deferred_source_removals().await.unwrap();
}
