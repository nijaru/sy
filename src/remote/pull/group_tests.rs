//! Whole-group contracts through the shared controller and real v3 transport.
use super::*;
use crate::engine::controller::{preflight_sync, SyncController, SyncPlan};
use crate::engine::planner::ComparisonPolicy;
use crate::engine::scan::{EntryMetadataRequest, ScanRequest};
use crate::engine::scheduler::ResourceBudget;
use crate::protocol::Operation;
use crate::remote::runtime::ClientRemoteSession;
use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

async fn setup(
    source: &Path,
    destination: &Path,
    policy: ComparisonPolicy,
    xattrs: bool,
) -> (
    RemotePullExecutor,
    SyncPlan,
    ClientRemoteSession,
    tokio::task::JoinHandle<crate::remote::serve::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let server = tokio::spawn(crate::remote::serve::serve_transport(
        reader,
        writer,
        Default::default(),
    ));
    let (reader, writer) = tokio::io::split(client);
    let client =
        ClientRemoteSession::connect(reader, writer, Operation::Pull, source, Default::default())
            .await
            .unwrap();
    // Only one live file is available: group execution must not nest permits
    // or leave ordinary workers running ahead of its prepared directories.
    let executor = RemotePullExecutor::new(
        destination.to_path_buf(),
        client.request_handle(),
        client.sender(),
        Scheduler::new(ResourceBudget {
            active_files: 1,
            ..Default::default()
        })
        .unwrap(),
    )
    .with_hardlinks(true)
    .with_xattrs(xattrs);
    let request = ScanRequest {
        respect_gitignore: false,
        include_git_dir: true,
        metadata: EntryMetadataRequest {
            identity: true,
            unix_mode: true,
            hardlink_group: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let (namespace_semantics, destination) = executor.destination_entries(request).await.unwrap();
    let source = client.request_handle().scan(request).await.unwrap();
    let mut plan = preflight_sync(
        source,
        destination,
        ComparisonPolicy {
            namespace_semantics,
            ..policy
        },
        None,
        |_| false,
    )
    .await
    .unwrap();
    plan.seal_hardlink_membership().await.unwrap();
    (executor, plan, client, server)
}

async fn close(
    client: ClientRemoteSession,
    server: tokio::task::JoinHandle<crate::remote::serve::Result<()>>,
) {
    drop(client);
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    if let Err(error) = result {
        assert!(
            matches!(&error, crate::remote::serve::ServeError::Session(
            RemoteSessionError::Router(error)) if matches!(error.as_ref(), crate::remote::router::RouterError::TransportEof)),
            "{error:?}"
        );
    }
}

fn stamp(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_600_000_000, 123)).unwrap();
}

fn observation(path: &Path) -> (u64, u32, i64, i64, u64) {
    let stat = std::fs::metadata(path).unwrap();
    (
        stat.ino(),
        stat.mode(),
        stat.ctime(),
        stat.ctime_nsec(),
        stat.nlink(),
    )
}

#[tokio::test]
async fn catalogue_order_owns_mode_and_canonical_bytes_across_parents() {
    for (first_exists, preserve_permissions) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("z")).unwrap();
        std::fs::write(source.path().join("a"), b"source").unwrap();
        std::fs::hard_link(source.path().join("a"), source.path().join("z/b")).unwrap();
        stamp(&source.path().join("a"), 0o640);
        std::fs::create_dir(destination.path().join("z")).unwrap();
        std::fs::write(destination.path().join("z/b"), b"target").unwrap();
        stamp(&destination.path().join("z/b"), 0o700);
        if first_exists {
            std::fs::write(destination.path().join("a"), b"target").unwrap();
            stamp(&destination.path().join("a"), 0o600);
            filetime::set_file_mtime(
                destination.path().join("a"),
                filetime::FileTime::from_unix_time(1_700_000_000, 456),
            )
            .unwrap();
        }
        // C/E and M/E topology repair both use canonical bytes. Only the
        // FIRST selected operation supplies unrequested mode/time baselines.
        let (executor, plan, client, server) = setup(
            source.path(),
            destination.path(),
            ComparisonPolicy {
                mode: crate::engine::planner::ComparisonMode::SizeOnly,
                preserve_permissions,
                ..Default::default()
            },
            false,
        )
        .await;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            SyncController::new(executor, NonZeroUsize::new(2).unwrap()).execute(plan),
        )
        .await
        .unwrap();
        close(client, server).await;
        let summary = result.unwrap();
        assert_eq!(summary.literal_bytes, 6);
        assert_eq!(summary.files_created, u64::from(!first_exists));
        assert_eq!(summary.files_updated, if first_exists { 2 } else { 1 });
        // The unchanged directory is still a semantic skip; repaired file
        // members are updates rather than quick-equal skips.
        assert_eq!(summary.files_skipped, 1);
        // Two namespace results, but only one populated creator and no delta
        // reconstruction: aliases must not invent transferred/reused bytes.
        assert_eq!(summary.files_transferred, 1);
        assert_eq!(summary.reused_bytes, 0);
        assert_eq!(summary.delta_files, 0);
        let a = std::fs::metadata(destination.path().join("a")).unwrap();
        let b = std::fs::metadata(destination.path().join("z/b")).unwrap();
        assert_eq!(a.ino(), b.ino());
        assert_eq!(
            a.mode() & 0o7777,
            if first_exists && !preserve_permissions {
                0o600
            } else {
                0o640
            }
        );
        if first_exists {
            assert_eq!(a.mtime(), 1_700_000_000);
            assert_eq!(a.mtime_nsec(), 456);
        }
        assert_eq!(a.nlink(), 2);
        for name in ["a", "z/b"] {
            assert_eq!(
                std::fs::read(destination.path().join(name)).unwrap(),
                b"source"
            );
        }
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 2);
        assert_eq!(
            std::fs::read_dir(destination.path().join("z"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn retained_bytes_isolate_outside_alias_then_matching_group_is_effect_free() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), b"source").unwrap();
    std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
    stamp(&source.path().join("a"), 0o640);
    std::fs::write(destination.path().join("a"), b"target").unwrap();
    std::fs::hard_link(destination.path().join("a"), destination.path().join("b")).unwrap();
    std::fs::hard_link(destination.path().join("a"), outside.path().join("alias")).unwrap();
    stamp(&destination.path().join("a"), 0o600);
    xattr::set(destination.path().join("a"), "user.sy-retained", b"old-md").unwrap();
    let old_inode = std::fs::metadata(outside.path().join("alias"))
        .unwrap()
        .ino();
    for repeat in [false, true] {
        let before = observation(&destination.path().join("a"));
        let (executor, plan, client, server) = setup(
            source.path(),
            destination.path(),
            ComparisonPolicy {
                preserve_permissions: true,
                ..Default::default()
            },
            false,
        )
        .await;
        let result = SyncController::new(executor, NonZeroUsize::new(2).unwrap())
            .execute(plan)
            .await;
        close(client, server).await;
        let summary = result.unwrap();
        assert_eq!(summary.literal_bytes, 0);
        assert_eq!(summary.files_transferred, 0);
        let a = std::fs::metadata(destination.path().join("a")).unwrap();
        assert_eq!(
            a.ino(),
            std::fs::metadata(destination.path().join("b"))
                .unwrap()
                .ino()
        );
        assert_ne!(a.ino(), old_inode);
        assert_eq!(a.mode() & 0o7777, 0o640);
        assert_eq!(a.mtime(), 1_600_000_000);
        assert_eq!(a.mtime_nsec(), 123);
        assert_eq!(a.nlink(), 2);
        assert_eq!(
            xattr::get(destination.path().join("a"), "user.sy-retained").unwrap(),
            Some(b"old-md".to_vec())
        );
        for name in ["a", "b"] {
            assert_eq!(
                std::fs::read(destination.path().join(name)).unwrap(),
                b"target"
            );
        }
        let alias = std::fs::metadata(outside.path().join("alias")).unwrap();
        assert_eq!(alias.ino(), old_inode);
        assert_eq!(alias.mode() & 0o7777, 0o600);
        assert_eq!(
            std::fs::read(outside.path().join("alias")).unwrap(),
            b"target"
        );
        if repeat {
            assert_eq!(
                observation(&destination.path().join("a")),
                before,
                "no-op changed ctime/inode/link count"
            );
            assert_eq!(summary.main_operations, 0);
            assert_eq!(summary.files_skipped, 2);
        }
    }
}

#[tokio::test]
async fn requested_xattrs_override_retained_metadata_without_refetching_bytes() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    for (root, payload) in [(source.path(), b"source"), (destination.path(), b"target")] {
        std::fs::write(root.join("a"), payload).unwrap();
        std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
        stamp(&root.join("a"), 0o640);
    }
    xattr::set(source.path().join("a"), "user.sy-pull", b"source-md").unwrap();
    // Native oversized defaults must not enter the bounded SOURCE metadata
    // path when the captured SOURCE set will replace them completely.
    let old_metadata = if cfg!(target_os = "macos") {
        vec![b'x'; crate::protocol::MAX_XATTR_TOTAL_BYTES + 1]
    } else {
        b"old-md".to_vec()
    };
    xattr::set(destination.path().join("a"), "user.sy-pull", &old_metadata).unwrap();
    for repeat in [false, true] {
        let before = observation(&destination.path().join("a"));
        let (executor, plan, client, server) =
            setup(source.path(), destination.path(), Default::default(), true).await;
        let result = SyncController::new(executor, NonZeroUsize::new(1).unwrap())
            .execute(plan)
            .await;
        close(client, server).await;
        let summary = result.unwrap();
        assert_eq!(summary.literal_bytes, 0);
        assert_eq!(
            std::fs::read(destination.path().join("a")).unwrap(),
            b"target"
        );
        assert_eq!(
            xattr::get(destination.path().join("b"), "user.sy-pull").unwrap(),
            Some(b"source-md".to_vec())
        );
        if repeat {
            assert_eq!(observation(&destination.path().join("a")), before);
        }
    }
}

#[tokio::test]
async fn source_change_after_validation_is_uncertain_not_group_rollback() {
    for late in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("a"), b"source").unwrap();
        std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
        stamp(&source.path().join("a"), 0o640);
        std::fs::write(destination.path().join("b"), b"old").unwrap();
        let (executor, plan, client, server) =
            setup(source.path(), destination.path(), Default::default(), false).await;
        let rooted = executor.metadata_authority().await.unwrap().clone();
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        // Each member admits private alias creation and then native publication.
        // The last gate changes source after every member validation, so only the
        // still-live SourceFinish check can prevent a false complete result.
        rooted.pause_mutation_at(
            if late { 3 } else { 0 },
            crate::rooted_fs::PublicationPause {
                point: crate::rooted_fs::PublicationPausePoint::AfterAdmission,
                reached: reached_tx,
                resume: resume_rx,
            },
        );
        let worker = tokio::spawn(async move {
            SyncController::new(executor, NonZeroUsize::new(1).unwrap())
                .execute(plan)
                .await
        });
        let reached = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
        let changed = std::fs::write(source.path().join("b"), b"changed");
        let _ = resume_tx.send(());
        let result = tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        drop(client);
        let _served = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(reached, Ok(Ok(()))));
        changed.unwrap();
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("source completion is uncertain"),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read(destination.path().join("a")).unwrap(),
            b"source"
        );
        assert_eq!(
            std::fs::read(destination.path().join("b")).unwrap(),
            if late { &b"source"[..] } else { &b"old"[..] }
        );
        assert_eq!(std::fs::read(source.path().join("a")).unwrap(), b"changed");
    }
}

#[cfg(feature = "acl")]
#[tokio::test]
async fn source_lease_acl_is_applied_to_the_common_staged_inode() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let source_file = source.path().join("a");
    std::fs::write(&source_file, b"source").unwrap();
    // SAFETY: getuid has no pointer arguments or caller preconditions.
    let uid = unsafe { libc::getuid() };
    let mut acl = exacl::getfacl(&source_file, None).unwrap();
    acl.push(exacl::AclEntry::allow_user(
        &uid.to_string(),
        exacl::Perm::READ,
        exacl::Flag::empty(),
    ));
    exacl::setfacl(&[&source_file], &acl, None).unwrap();
    std::fs::hard_link(&source_file, source.path().join("b")).unwrap();
    let expected = exacl::to_string(&exacl::getfacl(&source_file, None).unwrap()).unwrap();
    let (executor, plan, client, server) =
        setup(source.path(), destination.path(), Default::default(), false).await;
    let result = SyncController::new(executor.with_acls(true), NonZeroUsize::new(1).unwrap())
        .execute(plan)
        .await;
    close(client, server).await;
    result.unwrap();
    let first = std::fs::metadata(destination.path().join("a")).unwrap();
    let second = std::fs::metadata(destination.path().join("b")).unwrap();
    assert_eq!(first.ino(), second.ino());
    assert_eq!(first.nlink(), 2);
    let actual =
        exacl::to_string(&exacl::getfacl(destination.path().join("b"), None).unwrap()).unwrap();
    assert_eq!(actual, expected);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn requested_native_flags_finish_on_the_common_inode_after_anchor_retirement() {
    use std::os::macos::fs::MetadataExt;
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), b"source").unwrap();
    stamp(&source.path().join("a"), 0o640);
    let rooted = crate::rooted_fs::RootedFs::open(source.path().to_path_buf())
        .await
        .unwrap();
    tokio::task::spawn_blocking(move || {
        rooted.write_bsd_flags_blocking(
            &RelativePath::new("a").unwrap(),
            EntryKind::File,
            libc::UF_NODUMP,
        )
    })
    .await
    .unwrap()
    .unwrap();
    std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
    let (executor, plan, client, server) =
        setup(source.path(), destination.path(), Default::default(), false).await;
    let result = SyncController::new(executor.with_bsd_flags(true), NonZeroUsize::new(1).unwrap())
        .execute(plan)
        .await;
    close(client, server).await;
    result.unwrap();
    let a = std::fs::metadata(destination.path().join("a")).unwrap();
    let b = std::fs::metadata(destination.path().join("b")).unwrap();
    assert_eq!(a.ino(), b.ino());
    assert_eq!(a.nlink(), 2);
    assert_eq!(a.st_flags(), libc::UF_NODUMP);
    assert_eq!(b.st_flags(), libc::UF_NODUMP);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 2);
}
