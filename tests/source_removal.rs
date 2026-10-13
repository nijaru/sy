#![cfg(unix)]

use futures::TryStreamExt;
use std::num::NonZeroUsize;
use std::path::Path;
use sy::endpoint::local_entry_scan::local_entry_stream;
use sy::engine::controller::{preflight_sync_scoped_with_content, SyncController};
use sy::engine::domain::Entry;
use sy::engine::planner::{ComparisonMode, ComparisonPolicy};
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::remote::local_executor::LocalSyncExecutor;
use sy::rooted_fs::RootedFs;

async fn observed(stream: sy::engine::reconcile::EntryStream) -> Entry {
    stream.try_collect::<Vec<_>>().await.unwrap().remove(0)
}

fn scheduler() -> Scheduler {
    Scheduler::new(ResourceBudget::default()).unwrap()
}

#[tokio::test]
async fn replacing_destination_after_checksum_preflight_retains_source() {
    let source = tempfile::TempDir::new().unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    std::fs::write(source.path().join("file"), b"original").unwrap();
    std::fs::write(dest.path().join("file"), b"original").unwrap();
    let authority = sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let source_rooted = authority.rooted();
    let dest_rooted = RootedFs::open(dest.path().to_path_buf()).await.unwrap();
    let (dest_rooted, namespace) = dest_rooted.with_namespace_qualification().await.unwrap();
    let plan = preflight_sync_scoped_with_content(
        sy::engine::reconcile::OrderedReconciler::new(
            authority.entries(ScanRequest::default()),
            local_entry_stream(dest.path().to_path_buf(), ScanRequest::default()),
        ),
        ComparisonPolicy {
            mode: ComparisonMode::Checksum,
            ..ComparisonPolicy::default()
        },
        None,
        sy::engine::namespace::NamespacePreflight::new(&namespace),
        |_| true,
        |_| futures::future::ready(Ok(true)),
        move |source, dest| {
            let source_rooted = source_rooted.clone();
            let dest_rooted = dest_rooted.clone();
            async move {
                let source_hash = sy::remote::hash::hash_rooted_file(
                    source_rooted,
                    source.path,
                    source.size,
                    source.identity.unwrap(),
                )
                .await?;
                let dest_hash = sy::remote::hash::hash_rooted_file(
                    dest_rooted,
                    dest.path,
                    dest.size,
                    dest.identity.unwrap(),
                )
                .await?;
                Ok(source_hash == dest_hash)
            }
        },
    )
    .await
    .unwrap();
    std::fs::rename(dest.path().join("file"), dest.path().join("old")).unwrap();
    std::fs::write(dest.path().join("file"), b"raced").unwrap();
    let controller = SyncController::new(
        LocalSyncExecutor::new(
            authority,
            sy::endpoint::local::LocalEndpoint::new(dest.path().to_path_buf()),
            scheduler(),
        )
        .with_remove_source_files(true),
        NonZeroUsize::new(2).unwrap(),
    );
    assert!(controller.execute(plan).await.is_err());
    assert_eq!(
        std::fs::read(source.path().join("file")).unwrap(),
        b"original"
    );
    assert_eq!(std::fs::read(dest.path().join("file")).unwrap(), b"raced");
}

#[tokio::test]
async fn requested_xattr_parity_is_required_before_removing_unchanged_source() {
    let source = tempfile::TempDir::new().unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    std::fs::write(source.path().join("file"), b"same").unwrap();
    std::fs::write(dest.path().join("file"), b"same").unwrap();
    xattr::set(source.path().join("file"), "user.sy-preservation", b"keep").unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(format!("{}/", source.path().display()))
        .arg(dest.path())
        .args(["--remove-source-files", "--checksum", "-X"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!source.path().join("file").exists());
    assert_eq!(std::fs::read(dest.path().join("file")).unwrap(), b"same");
    assert_eq!(
        xattr::get(dest.path().join("file"), "user.sy-preservation").unwrap(),
        Some(b"keep".to_vec())
    );
}

#[tokio::test]
async fn checksum_mode_does_not_forge_a_content_receipt_for_unchanged_links() {
    let source = tempfile::TempDir::new().unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    std::os::unix::fs::symlink("target", source.path().join("link")).unwrap();
    std::os::unix::fs::symlink("target", dest.path().join("link")).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(format!("{}/", source.path().display()))
        .arg(dest.path())
        .args(["--remove-source-files", "--checksum", "--links=preserve"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_link(source.path().join("link")).unwrap(),
        Path::new("target")
    );
    assert_eq!(
        std::fs::read_link(dest.path().join("link")).unwrap(),
        Path::new("target")
    );
}

#[tokio::test]
async fn remote_parity_rechecks_identity_and_selected_metadata() {
    use sy::engine::controller::SyncPlanExecutor;
    use sy::engine::planner::ExecutionPolicy;
    use sy::protocol::Operation;
    use sy::remote::push::RemotePushExecutor;
    use sy::remote::router::RouterConfig;
    use sy::remote::runtime::{ClientRemoteSession, IncomingRequest, ServerRemoteSession};
    use sy::transfer::delta::BasisIndexLimits;

    for (replace, same_entry) in [(true, false), (false, false), (false, true)] {
        let source = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"same").unwrap();
        std::fs::write(dest.path().join("file"), b"same").unwrap();
        if !replace && !same_entry {
            xattr::set(source.path().join("file"), "user.sy-preservation", b"keep").unwrap();
        }
        let authority = sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
            .await
            .unwrap();
        let source_entry = observed(authority.entries(ScanRequest::default())).await;
        let dest_entry = if same_entry {
            source_entry.clone()
        } else {
            observed(local_entry_stream(
                dest.path().to_path_buf(),
                ScanRequest::default(),
            ))
            .await
        };
        if replace {
            std::fs::rename(dest.path().join("file"), dest.path().join("old")).unwrap();
            std::fs::write(dest.path().join("file"), b"race").unwrap();
        }
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let root = if same_entry {
            source.path()
        } else {
            dest.path()
        }
        .to_path_buf();
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let IncomingRequest::Hash(incoming) = session.next_request().await.unwrap().unwrap()
            else {
                panic!("expected destination verification");
            };
            session.hash_handler().unwrap().serve(incoming).await
        });
        let client = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            &root,
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let executor = RemotePushExecutor::new(
            authority,
            client.request_handle(),
            scheduler(),
            BasisIndexLimits::default(),
        )
        .with_remove_source_files(true)
        .with_xattrs(!replace && !same_entry);
        let result = SyncPlanExecutor::remove_unchanged_source(
            &executor,
            &source_entry,
            &dest_entry,
            ExecutionPolicy::default(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(source.path().join("file")).unwrap(), b"same");
        assert_eq!(server.await.unwrap().is_err(), replace);
        assert_eq!(
            std::fs::read(dest.path().join("file")).unwrap(),
            if replace { b"race" } else { b"same" }
        );
    }
}

#[tokio::test]
async fn self_sync_cannot_remove_the_only_copy() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::write(root.path().join("file"), b"only copy").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(format!("{}/", root.path().display()))
        .arg(root.path())
        .args(["--checksum", "--remove-source-files"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(root.path().join("file")).unwrap(),
        b"only copy"
    );
}

#[tokio::test]
async fn standalone_existing_aliases_continue_after_own_source_unlink() {
    use std::os::unix::fs::MetadataExt;
    let source = tempfile::TempDir::new().unwrap();
    let destination = tempfile::TempDir::new().unwrap();
    std::fs::write(source.path().join("a"), b"only copy").unwrap();
    std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
    for name in ["a", "b"] {
        std::fs::hard_link(source.path().join(name), destination.path().join(name)).unwrap();
    }
    let original = std::fs::metadata(source.path().join("a")).unwrap();
    assert_eq!(original.nlink(), 4);
    let authority = sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let source_rooted = authority.rooted();
    let destination_rooted = RootedFs::open(destination.path().to_path_buf())
        .await
        .unwrap();
    let (destination_rooted, namespace) = destination_rooted
        .with_namespace_qualification()
        .await
        .unwrap();
    let plan = preflight_sync_scoped_with_content(
        sy::engine::reconcile::OrderedReconciler::new(
            authority.entries(ScanRequest::default()),
            local_entry_stream(destination.path().to_path_buf(), ScanRequest::default()),
        ),
        ComparisonPolicy {
            mode: ComparisonMode::Checksum,
            ..ComparisonPolicy::default()
        },
        None,
        sy::engine::namespace::NamespacePreflight::new(&namespace),
        |_| true,
        |_| futures::future::ready(Ok(true)),
        move |source, destination| {
            let source_rooted = source_rooted.clone();
            let destination_rooted = destination_rooted.clone();
            async move {
                let source_hash = sy::remote::hash::hash_rooted_file(
                    source_rooted,
                    source.path,
                    source.size,
                    source.identity.unwrap(),
                )
                .await?;
                let destination_hash = sy::remote::hash::hash_rooted_file(
                    destination_rooted,
                    destination.path,
                    destination.size,
                    destination.identity.unwrap(),
                )
                .await?;
                Ok(source_hash == destination_hash)
            }
        },
    )
    .await
    .unwrap();
    let controller = SyncController::new(
        LocalSyncExecutor::new(
            authority,
            sy::endpoint::local::LocalEndpoint::new(destination.path().to_path_buf()),
            scheduler(),
        )
        .with_remove_source_files(true),
        NonZeroUsize::new(2).unwrap(),
    );
    controller.execute(plan).await.unwrap();
    for name in ["a", "b"] {
        assert!(!source.path().join(name).exists());
        assert_eq!(
            std::fs::read(destination.path().join(name)).unwrap(),
            b"only copy"
        );
        let surviving = std::fs::metadata(destination.path().join(name)).unwrap();
        assert_eq!(
            (surviving.dev(), surviving.ino()),
            (original.dev(), original.ino())
        );
        assert_eq!(surviving.nlink(), 2);
    }
}

#[tokio::test]
async fn root_aliases_are_refused_but_distinct_hardlink_names_are_allowed() {
    use sy::engine::controller::SyncPlanExecutor;
    use sy::engine::planner::ExecutionPolicy;

    let source = tempfile::TempDir::new().unwrap();
    let destination = tempfile::TempDir::new().unwrap();
    let alias = destination.path().join("alias");
    std::fs::write(source.path().join("file"), b"only copy").unwrap();
    std::os::unix::fs::symlink(source.path(), &alias).unwrap();
    std::fs::hard_link(source.path().join("file"), destination.path().join("file")).unwrap();
    let authority = sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
        .await
        .unwrap();
    let entry = observed(authority.entries(ScanRequest::default())).await;
    let executor = LocalSyncExecutor::new(
        authority.clone(),
        sy::endpoint::local::LocalEndpoint::new(alias),
        scheduler(),
    )
    .with_remove_source_files(true);
    assert!(SyncPlanExecutor::remove_unchanged_source(
        &executor,
        &entry,
        &entry,
        ExecutionPolicy::default()
    )
    .await
    .is_err());
    assert!(source.path().join("file").exists());

    let source_entry = observed(authority.entries(ScanRequest::default())).await;
    let mut dest_scan =
        local_entry_stream(destination.path().to_path_buf(), ScanRequest::default());
    let destination_entry = loop {
        let entry = dest_scan.try_next().await.unwrap().unwrap();
        if entry.path.as_path() == Path::new("file") {
            break entry;
        }
    };
    let executor = LocalSyncExecutor::new(
        authority,
        sy::endpoint::local::LocalEndpoint::new(destination.path().to_path_buf()),
        scheduler(),
    )
    .with_remove_source_files(true);
    SyncPlanExecutor::remove_unchanged_source(
        &executor,
        &source_entry,
        &destination_entry,
        ExecutionPolicy::default(),
    )
    .await
    .unwrap();
    assert!(source.path().join("file").exists());
    SyncPlanExecutor::finish_deferred_source_removals(&executor)
        .await
        .unwrap();
    assert!(!source.path().join("file").exists());
    assert_eq!(
        std::fs::read(destination.path().join("file")).unwrap(),
        b"only copy"
    );
}
