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

async fn observed(root: &Path) -> Entry {
    local_entry_stream(root.to_path_buf(), ScanRequest::default())
        .try_next()
        .await
        .unwrap()
        .unwrap()
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
    let source_rooted = RootedFs::open(source.path().to_path_buf()).await.unwrap();
    let dest_rooted = RootedFs::open(dest.path().to_path_buf()).await.unwrap();
    let plan = preflight_sync_scoped_with_content(
        local_entry_stream(source.path().to_path_buf(), ScanRequest::default()),
        local_entry_stream(dest.path().to_path_buf(), ScanRequest::default()),
        ComparisonPolicy {
            mode: ComparisonMode::Checksum,
            ..ComparisonPolicy::default()
        },
        None,
        |_| true,
        |_| true,
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
            source.path().to_path_buf(),
            dest.path().to_path_buf(),
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
        let source_entry = observed(source.path()).await;
        let dest_entry = if same_entry {
            source_entry.clone()
        } else {
            observed(dest.path()).await
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
            session.hash_handler().serve(incoming).await
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
            source.path().to_path_buf(),
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
async fn root_aliases_are_refused_but_distinct_hardlink_names_are_allowed() {
    use sy::engine::controller::SyncPlanExecutor;
    use sy::engine::planner::ExecutionPolicy;

    let source = tempfile::TempDir::new().unwrap();
    let destination = tempfile::TempDir::new().unwrap();
    let alias = destination.path().join("alias");
    std::fs::write(source.path().join("file"), b"only copy").unwrap();
    std::os::unix::fs::symlink(source.path(), &alias).unwrap();
    let entry = observed(source.path()).await;
    let executor = LocalSyncExecutor::new(source.path().to_path_buf(), alias, scheduler())
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

    std::fs::hard_link(source.path().join("file"), destination.path().join("file")).unwrap();
    let source_entry = observed(source.path()).await;
    let mut dest_scan =
        local_entry_stream(destination.path().to_path_buf(), ScanRequest::default());
    let destination_entry = loop {
        let entry = dest_scan.try_next().await.unwrap().unwrap();
        if entry.path.as_path() == Path::new("file") {
            break entry;
        }
    };
    let executor = LocalSyncExecutor::new(
        source.path().to_path_buf(),
        destination.path().to_path_buf(),
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
    assert!(!source.path().join("file").exists());
    assert_eq!(
        std::fs::read(destination.path().join("file")).unwrap(),
        b"only copy"
    );
}
