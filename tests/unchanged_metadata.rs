#![cfg(unix)]

use futures::TryStreamExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use sy::engine::controller::SyncPlanExecutor;
use sy::engine::domain::{ContentComparison, SyncOp};
use sy::engine::planner::ExecutionPolicy;
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::engine::work::WorkResult;
use sy::remote::local_executor::LocalSyncExecutor;
use sy::remote::pull::RemotePullExecutor;
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteSession;

#[tokio::test]
async fn quick_unchanged_local_and_pull_preservation_keeps_destination_payload_and_inode() {
    for (pull, equal_bytes, remove_source) in [
        (false, false, false),
        (false, false, true),
        (false, true, true),
        (true, false, false),
    ] {
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
        xattr::set(&source_path, "user.sy-kept", b"preserved").unwrap();
        xattr::set(&destination_path, "user.sy-stale", b"remove").unwrap();
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = std::ffi::CString::new(source_path.as_os_str().as_bytes()).unwrap();
            // SAFETY: path is a live NUL-terminated fixture path; UF_NODUMP
            // does not prevent reads, writes, linking, or fixture cleanup.
            assert_eq!(unsafe { libc::chflags(path.as_ptr(), libc::UF_NODUMP) }, 0);
        }
        let mut request = ScanRequest::default();
        request.metadata.unix_mode = true;
        let authority = sy::endpoint::source_root::SourceRoot::open(source.path().into())
            .await
            .unwrap();
        let mut source_entries = authority.entries(request);
        let source_entry = source_entries.try_next().await.unwrap().unwrap();
        source_entries.close().await.unwrap();
        let mut destination_entries =
            sy::endpoint::local_entry_scan::local_entry_stream(destination.path().into(), request);
        let destination_entry = destination_entries.try_next().await.unwrap().unwrap();
        destination_entries.close().await.unwrap();
        let before = std::fs::metadata(&destination_path).unwrap();
        let operation = SyncOp::Unchanged {
            source: source_entry,
            destination: destination_entry,
            comparison: ContentComparison::Unverified,
        };
        let policy = ExecutionPolicy {
            preserve_permissions: true,
            preserve_times: true,
        };
        let scheduler = Scheduler::new(ResourceBudget {
            metadata_ops: 1,
            cpu_tasks: 1,
            ..ResourceBudget::default()
        })
        .unwrap();
        let result = if pull {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (reader, writer) = tokio::io::split(server_io);
            let server = tokio::spawn(sy::remote::serve::serve_transport(
                reader,
                writer,
                RouterConfig::default(),
            ));
            let (reader, writer) = tokio::io::split(client_io);
            let client = ClientRemoteSession::connect(
                reader,
                writer,
                sy::protocol::Operation::Pull,
                source.path(),
                RouterConfig::default(),
            )
            .await
            .unwrap();
            let executor = RemotePullExecutor::new(
                destination.path().into(),
                client.request_handle(),
                client.sender(),
                scheduler,
            )
            .with_xattrs(true)
            .with_bsd_flags(cfg!(target_os = "macos"));
            let work = executor.lower(operation, policy).unwrap().unwrap();
            assert!(matches!(
                work.action(),
                sy::remote::pull::RemotePullAction::ApplyMetadata {
                    unix_mode: Some(0o640),
                    ..
                }
            ));
            assert!(work.resources().buffered_bytes > 0);
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(10), executor.execute(work))
                    .await
                    .unwrap();
            drop(executor);
            drop(client);
            let _closed = server.await.unwrap();
            result.map_err(|error| error.to_string())
        } else {
            let executor = LocalSyncExecutor::new(authority, destination.path().into(), scheduler)
                .with_xattrs(true)
                .with_bsd_flags(cfg!(target_os = "macos"))
                .with_remove_source_files(remove_source);
            let work = executor.lower(operation, policy).unwrap().unwrap();
            assert!(matches!(
                work.action(),
                sy::remote::local_executor::LocalSyncAction::ApplyMetadata {
                    unix_mode: Some(0o640),
                    ..
                }
            ));
            assert!(work.resources().buffered_bytes > 0);
            tokio::time::timeout(std::time::Duration::from_secs(10), executor.execute(work))
                .await
                .unwrap()
                .map_err(|error| error.to_string())
        };
        assert_eq!(
            std::fs::read(&destination_path).unwrap(),
            payload,
            "pull={pull}"
        );
        if remove_source && !equal_bytes {
            assert!(result.is_err());
        } else {
            assert!(matches!(result, Ok(WorkResult::Metadata)), "{result:?}");
        }
        let after = std::fs::metadata(&destination_path).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(
            (before.mode(), before.mtime(), before.mtime_nsec()),
            (after.mode(), after.mtime(), after.mtime_nsec())
        );
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            assert_eq!(after.st_flags() & libc::UF_NODUMP, libc::UF_NODUMP);
        }
        assert_eq!(
            xattr::get(&destination_path, "user.sy-kept").unwrap(),
            Some(b"preserved".to_vec())
        );
        assert_eq!(
            xattr::get(&destination_path, "user.sy-stale").unwrap(),
            None
        );
        assert_eq!(source_path.exists(), !(remove_source && equal_bytes));
        if source_path.exists() {
            assert_eq!(std::fs::read(&source_path).unwrap(), b"source");
        }
    }
}
