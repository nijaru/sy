#![cfg(unix)]

use futures::TryStreamExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use sy::endpoint::local_entry_scan::local_entry_stream;
use sy::engine::controller::SyncPlanExecutor;
use sy::engine::domain::{ContentComparison, Entry, SyncOp};
use sy::engine::planner::ExecutionPolicy;
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::engine::work::WorkResult;
use sy::protocol::Operation;
use sy::remote::push::RemotePushExecutor;
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteSession;
use sy::transfer::delta::BasisIndexLimits;

async fn scan_file(entries: sy::engine::reconcile::EntryStream) -> Entry {
    entries.try_collect::<Vec<_>>().await.unwrap().remove(0)
}

/// Quick checks do not imply byte equality. Test real lowering and the real
/// server, including a fresh receipt after metadata changes invalidate ctime.
#[tokio::test]
async fn quick_unchanged_push_preserves_destination_bytes_and_removes_source_only_with_fresh_parity(
) {
    for (equal_bytes, remove_source) in [(false, false), (false, true), (true, true)] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let source_path = source.path().join("file");
        let destination_path = destination.path().join("file");
        std::fs::write(&source_path, b"source").unwrap();
        let payload: &[u8] = if equal_bytes { b"source" } else { b"target" };
        std::fs::write(&destination_path, payload).unwrap();
        for path in [&source_path, &destination_path] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).unwrap();
            filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_600_000_000, 123))
                .unwrap();
        }
        xattr::set(&source_path, "user.sy-observed-push", b"preserved").unwrap();
        xattr::set(&destination_path, "user.sy-stale", b"remove").unwrap();
        let authority = sy::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
            .await
            .unwrap();
        let mut request = ScanRequest::default();
        request.metadata.unix_mode = true;
        let source_entry = scan_file(authority.entries(request)).await;
        let destination_entry = scan_file(local_entry_stream(
            destination.path().to_path_buf(),
            request,
        ))
        .await;
        assert_eq!(source_entry.size, destination_entry.size);
        assert_eq!(source_entry.modified, destination_entry.modified);
        let before = std::fs::metadata(&destination_path).unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(sy::remote::serve::serve_transport(
            server_reader,
            server_writer,
            RouterConfig::default(),
        ));
        let client = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let scheduler = Scheduler::new(ResourceBudget::default()).unwrap();
        let executor = RemotePushExecutor::new(
            authority,
            client.request_handle(),
            scheduler,
            BasisIndexLimits::default(),
        )
        .with_xattrs(true)
        .with_remove_source_files(remove_source);
        let work = executor
            .lower(
                SyncOp::Unchanged {
                    source: source_entry,
                    destination: destination_entry,
                    comparison: ContentComparison::Unverified,
                },
                ExecutionPolicy {
                    preserve_permissions: true,
                    preserve_times: true,
                },
            )
            .unwrap()
            .unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), executor.execute(work))
                .await
                .unwrap();
        assert_eq!(std::fs::read(&destination_path).unwrap(), payload);
        if remove_source && !equal_bytes {
            assert!(matches!(
                result,
                Err(sy::remote::push::RemotePushError::Existing(
                    sy::endpoint::existing::ExistingDestinationError::ContentMismatch(_)
                ))
            ));
        } else {
            assert!(matches!(result, Ok(WorkResult::Metadata)), "{result:?}");
        }
        let after = std::fs::metadata(&destination_path).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(
            (before.mode(), before.mtime(), before.mtime_nsec()),
            (after.mode(), after.mtime(), after.mtime_nsec())
        );
        assert_eq!(
            xattr::get(&destination_path, "user.sy-observed-push").unwrap(),
            Some(b"preserved".to_vec())
        );
        assert!(xattr::get(&destination_path, "user.sy-stale")
            .unwrap()
            .is_none());
        assert_eq!(source_path.exists(), !(equal_bytes && remove_source));
        if source_path.exists() {
            assert_eq!(std::fs::read(&source_path).unwrap(), b"source");
        }
        server.abort();
        let _ = server.await;
    }
}
