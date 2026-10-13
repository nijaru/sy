use super::*;
use crate::engine::controller::{preflight_sync, SyncController, SyncPlan};
use crate::engine::planner::ComparisonPolicy;
use crate::engine::scan::{EntryMetadataRequest, ScanRequest};
use crate::engine::scheduler::ResourceBudget;
use crate::remote::runtime::ClientRemoteSession;
use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

async fn session(
    root: &Path,
) -> (
    ClientRemoteSession,
    tokio::task::JoinHandle<crate::remote::serve::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(server);
    let server = tokio::spawn(crate::remote::serve::serve_transport(
        sr,
        sw,
        Default::default(),
    ));
    let (cr, cw) = tokio::io::split(client);
    let client = ClientRemoteSession::connect(
        cr,
        cw,
        crate::protocol::Operation::Push,
        root,
        Default::default(),
    )
    .await
    .unwrap();
    (client, server)
}

async fn plan(
    root: &crate::endpoint::source_root::SourceRoot,
    remote: &ClientRemoteHandle,
    permissions: bool,
) -> SyncPlan {
    let request = ScanRequest {
        metadata: EntryMetadataRequest {
            unix_mode: true,
            hardlink_group: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut plan = preflight_sync(
        root.entries(request),
        remote.scan(request).await.unwrap(),
        ComparisonPolicy {
            preserve_permissions: permissions,
            namespace_semantics: remote.namespace_semantics(),
            ..Default::default()
        },
        None,
        |_| true,
    )
    .await
    .unwrap();
    plan.seal_hardlink_membership().await.unwrap();
    plan
}

fn executor(
    root: crate::endpoint::source_root::SourceRoot,
    remote: ClientRemoteHandle,
    remove: bool,
    xattrs: bool,
) -> RemotePushExecutor {
    RemotePushExecutor::new(
        root,
        remote,
        Scheduler::new(ResourceBudget::default()).unwrap(),
        BasisIndexLimits::default(),
    )
    .with_hardlinks(true)
    .with_remove_source_files(remove)
    .with_xattrs(xattrs)
}

async fn close(
    client: ClientRemoteSession,
    server: tokio::task::JoinHandle<crate::remote::serve::Result<()>>,
) {
    drop(client);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    if let Err(error) = result {
        assert!(
            matches!(&error, crate::remote::serve::ServeError::Session(RemoteSessionError::Router(error)) if matches!(error.as_ref(), crate::remote::router::RouterError::TransportEof)),
            "{error:?}"
        );
    }
}

fn source_group(root: &Path) {
    std::fs::write(root.join("a"), b"SOURCE").unwrap();
    std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
}
fn quick_old(source: &Path, destination: &Path) {
    std::fs::write(destination, b"OLDOLD").unwrap();
    std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o600)).unwrap();
    let time = filetime::FileTime::from_last_modification_time(&std::fs::metadata(source).unwrap());
    filetime::set_file_mtime(destination, time).unwrap();
}

fn observation(path: &Path) -> (u64, u64, u32, i64, i64, u64, u64, i64, i64) {
    let stat = std::fs::metadata(path).unwrap();
    (
        stat.dev(),
        stat.ino(),
        stat.mode(),
        stat.ctime(),
        stat.ctime_nsec(),
        stat.nlink(),
        stat.len(),
        stat.mtime(),
        stat.mtime_nsec(),
    )
}

#[tokio::test]
async fn unreadable_source_group_is_observation_only_unless_data_is_required() {
    for requirement in ["basic", "xattrs", "removal", "bytes", "setter"] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        for (root, bytes) in [(source.path(), b"SOURCE"), (destination.path(), b"OLDOLD")] {
            std::fs::write(root.join("a"), bytes).unwrap();
            std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
            filetime::set_file_mtime(
                root.join("a"),
                filetime::FileTime::from_unix_time(1_600_000_000, 123),
            )
            .unwrap();
            std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o000))
                .unwrap();
        }
        if requirement == "setter" {
            std::fs::set_permissions(
                destination.path().join("a"),
                std::fs::Permissions::from_mode(0o200),
            )
            .unwrap();
        }
        if requirement == "bytes" {
            // Topology repair must consume SOURCE even for quick-equal files.
            std::fs::remove_file(destination.path().join("b")).unwrap();
        }
        assert_eq!(
            std::fs::File::open(source.path().join("a"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let paths = [
            source.path().join("a"),
            source.path().join("b"),
            destination.path().join("a"),
        ];
        let before = paths.each_ref().map(|path| observation(path));
        let root = crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
            .await
            .unwrap();
        let (client, server) = session(destination.path()).await;
        let selected = plan(&root, &client.request_handle(), true).await;
        let result = SyncController::new(
            executor(
                root,
                client.request_handle(),
                requirement == "removal",
                requirement == "xattrs",
            ),
            NonZeroUsize::new(1).unwrap(),
        )
        .execute(selected)
        .await;
        if requirement == "basic" {
            close(client, server).await;
        } else {
            assert!(result.is_err(), "{requirement} must retain mandatory reads");
            drop(client);
            let served = tokio::time::timeout(std::time::Duration::from_secs(10), server)
                .await
                .unwrap()
                .unwrap();
            // A local SOURCE-open failure may precede every remote request.
            // The peer then has no failed handler and can close successfully.
            if requirement == "setter" {
                assert!(
                    matches!(&served, Err(crate::remote::serve::ServeError::Request(message)) if message.contains("Permission denied")),
                    "{served:?}"
                );
            }
            if let Err(error) = served {
                assert!(
                    matches!(
                        error,
                        crate::remote::serve::ServeError::Request(_)
                            | crate::remote::serve::ServeError::Session(_)
                    ),
                    "{error:?}"
                );
            }
        }
        if requirement == "basic" {
            let summary = result.unwrap();
            assert_eq!(summary.main_operations, 0);
            assert_eq!(summary.files_skipped, 2);
            assert_eq!(summary.literal_bytes, 0);
            assert_eq!(observation(&destination.path().join("b")), before[2]);
        } else {
            let error = result.unwrap_err();
            if requirement != "setter" {
                assert!(
                    error.to_string().contains("Permission denied"),
                    "{requirement}: {error:?}"
                );
            }
        }
        assert_eq!(paths.each_ref().map(|path| observation(path)), before);
    }
}

#[tokio::test]
async fn selected_first_not_scheduled_member_owns_mode_and_topology_repair_bytes() {
    for (missing, preserve, expected_mode) in
        [("a", false, 0o640), ("b", false, 0o600), ("b", true, 0o640)]
    {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        source_group(source.path());
        let existing = if missing == "a" { "b" } else { "a" };
        quick_old(
            &source.path().join(existing),
            &destination.path().join(existing),
        );
        let root = crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
            .await
            .unwrap();
        let (client, server) = session(destination.path()).await;
        let selected = plan(&root, &client.request_handle(), preserve).await;
        let result = SyncController::new(
            executor(root, client.request_handle(), false, false),
            NonZeroUsize::new(2).unwrap(),
        )
        .execute(selected)
        .await;
        close(client, server).await;
        let summary = result.unwrap();
        assert_eq!(summary.literal_bytes, 6);
        assert_eq!(summary.files_transferred, 1);
        assert_eq!(summary.delta_files, 0);
        assert_eq!(summary.reused_bytes, 0);
        let a = std::fs::metadata(destination.path().join("a")).unwrap();
        let b = std::fs::metadata(destination.path().join("b")).unwrap();
        assert_eq!(a.ino(), b.ino());
        assert_eq!(a.mode() & 0o777, expected_mode);
        assert_eq!(
            std::fs::read(destination.path().join("a")).unwrap(),
            b"SOURCE"
        );
        assert_eq!(std::fs::read(source.path().join("a")).unwrap(), b"SOURCE");
    }
}

#[tokio::test]
async fn grouped_delta_uses_observed_old_basis_and_reports_only_physical_population() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let old = vec![b'x'; 128 * 1024];
    let mut wanted = old.clone();
    wanted.extend_from_slice(b"changed tail");
    std::fs::write(source.path().join("a"), &wanted).unwrap();
    std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
    std::fs::write(destination.path().join("a"), &old).unwrap();
    std::fs::hard_link(destination.path().join("a"), destination.path().join("b")).unwrap();
    let root = crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let (client, server) = session(destination.path()).await;
    let selected = plan(&root, &client.request_handle(), false).await;
    let result = SyncController::new(
        executor(root, client.request_handle(), true, false).with_delta_min_size(1),
        NonZeroUsize::new(2).unwrap(),
    )
    .execute(selected)
    .await;
    close(client, server).await;
    let summary = result.unwrap();
    assert_eq!(summary.files_transferred, 1);
    assert_eq!(summary.delta_files, 1);
    assert_eq!(
        summary.literal_bytes + summary.reused_bytes,
        wanted.len() as u64
    );
    assert!(summary.reused_bytes > 0);
    for name in ["a", "b"] {
        assert_eq!(
            std::fs::read(destination.path().join(name)).unwrap(),
            wanted
        );
        assert!(!source.path().join(name).exists());
    }
}

#[tokio::test]
async fn retained_unequal_old_bytes_isolate_metadata_from_outside_alias_and_keep_sources() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    source_group(source.path());
    quick_old(&source.path().join("a"), &destination.path().join("a"));
    std::fs::hard_link(destination.path().join("a"), destination.path().join("b")).unwrap();
    // Outside the scanned root, but sharing the retained inode.
    let outside = tempfile::tempdir().unwrap();
    std::fs::hard_link(destination.path().join("a"), outside.path().join("alias")).unwrap();
    xattr::set(source.path().join("a"), "user.sy-push-group", b"wanted").unwrap();
    let root = crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let before = std::fs::metadata(source.path().join("a")).unwrap();
    let (client, server) = session(destination.path()).await;
    let selected = plan(&root, &client.request_handle(), false).await;
    let result = SyncController::new(
        executor(root, client.request_handle(), true, true),
        NonZeroUsize::new(2).unwrap(),
    )
    .execute(selected)
    .await;
    close(client, server).await;
    assert_eq!(result.unwrap().literal_bytes, 0);
    let a = std::fs::metadata(destination.path().join("a")).unwrap();
    assert_eq!(
        a.ino(),
        std::fs::metadata(destination.path().join("b"))
            .unwrap()
            .ino()
    );
    assert_ne!(
        a.ino(),
        std::fs::metadata(outside.path().join("alias"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        std::fs::read(destination.path().join("a")).unwrap(),
        b"OLDOLD"
    );
    assert_eq!(
        xattr::get(destination.path().join("a"), "user.sy-push-group").unwrap(),
        Some(b"wanted".to_vec())
    );
    assert_eq!(
        xattr::get(outside.path().join("alias"), "user.sy-push-group").unwrap(),
        None
    );
    assert!(source.path().join("a").exists() && source.path().join("b").exists());
    let after = std::fs::metadata(source.path().join("a")).unwrap();
    assert_eq!(
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec())
    );
}

#[tokio::test]
async fn fulfilled_coalesced_group_is_effect_free_despite_quick_unequal_source() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    source_group(source.path());
    quick_old(&source.path().join("a"), &destination.path().join("a"));
    std::fs::hard_link(destination.path().join("a"), destination.path().join("b")).unwrap();
    let before = std::fs::metadata(destination.path().join("a")).unwrap();
    let source_before = std::fs::metadata(source.path().join("a")).unwrap();
    let root = crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let (client, server) = session(destination.path()).await;
    let selected = plan(&root, &client.request_handle(), false).await;
    let result = SyncController::new(
        executor(root, client.request_handle(), true, false),
        NonZeroUsize::new(2).unwrap(),
    )
    .execute(selected)
    .await;
    close(client, server).await;
    let summary = result.unwrap();
    assert_eq!(summary.main_operations, 0);
    let after = std::fs::metadata(destination.path().join("a")).unwrap();
    assert_eq!(
        (before.ino(), before.ctime(), before.ctime_nsec()),
        (after.ino(), after.ctime(), after.ctime_nsec())
    );
    let source_after = std::fs::metadata(source.path().join("a")).unwrap();
    assert_eq!(
        (source_before.ctime(), source_before.ctime_nsec()),
        (source_after.ctime(), source_after.ctime_nsec())
    );
    assert!(source.path().join("b").exists());
}

#[tokio::test]
async fn exact_final_group_proof_authorizes_all_selected_source_unlinks() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    source_group(source.path());
    std::fs::hard_link(source.path().join("a"), outside.path().join("alias")).unwrap();
    let root = crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let (client, server) = session(destination.path()).await;
    let selected = plan(&root, &client.request_handle(), false).await;
    let result = SyncController::new(
        executor(root, client.request_handle(), true, false),
        NonZeroUsize::new(2).unwrap(),
    )
    .execute(selected)
    .await;
    close(client, server).await;
    assert_eq!(result.unwrap().literal_bytes, 6);
    assert!(!source.path().join("a").exists() && !source.path().join("b").exists());
    assert_eq!(
        std::fs::read(outside.path().join("alias")).unwrap(),
        b"SOURCE"
    );
    assert_eq!(
        std::fs::metadata(outside.path().join("alias"))
            .unwrap()
            .nlink(),
        1
    );
    assert_eq!(
        std::fs::metadata(destination.path().join("a"))
            .unwrap()
            .ino(),
        std::fs::metadata(destination.path().join("b"))
            .unwrap()
            .ino()
    );
}

#[tokio::test]
async fn inode_only_noop_ack_cannot_authorize_unlinking_the_destination_binding() {
    let source = tempfile::tempdir().unwrap();
    source_group(source.path());
    let root = crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let (client, server) = session(source.path()).await;
    let selected = plan(&root, &client.request_handle(), false).await;
    let result = SyncController::new(
        executor(root, client.request_handle(), true, false),
        NonZeroUsize::new(2).unwrap(),
    )
    .execute(selected)
    .await;
    close(client, server).await;
    assert_eq!(result.unwrap().main_operations, 0);
    for name in ["a", "b"] {
        assert_eq!(std::fs::read(source.path().join(name)).unwrap(), b"SOURCE");
    }
}

#[tokio::test]
async fn original_local_source_root_and_ancestor_names_cannot_be_adopted_after_scan() {
    for swap_root in [false, true] {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source");
        let destination = tempfile::tempdir().unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(source.join("dir")).unwrap();
        source_group(&source.join("dir"));
        let root = crate::endpoint::source_root::SourceRoot::open(source.clone())
            .await
            .unwrap();
        let (client, server) = session(destination.path()).await;
        let selected = plan(&root, &client.request_handle(), false).await;
        let original = if swap_root {
            source.clone()
        } else {
            source.join("dir")
        };
        let moved = parent.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        if swap_root {
            std::fs::create_dir(original.join("dir")).unwrap();
        }
        let replacement = if swap_root {
            original.join("dir")
        } else {
            original.clone()
        };
        source_group(&replacement);
        let result = SyncController::new(
            executor(root, client.request_handle(), true, false),
            NonZeroUsize::new(2).unwrap(),
        )
        .execute(selected)
        .await;
        close(client, server).await;
        assert!(
            result.is_err(),
            "replacement must not become original authority"
        );
        let old = if swap_root { moved.join("dir") } else { moved };
        assert_eq!(std::fs::read(old.join("a")).unwrap(), b"SOURCE");
        assert_eq!(std::fs::read(replacement.join("b")).unwrap(), b"SOURCE");
        assert!(!destination.path().join("dir/a").exists());
    }
}
