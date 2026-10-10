use super::*;
use crate::endpoint::local_identity::metadata_identity;
use crate::engine::domain::SyncScope;
use crate::engine::reconcile::{EngineError, OrderedReconciler};
use crate::protocol::Operation;
use crate::remote::router::RouterConfig;
use crate::remote::runtime::{ClientRemoteSession, IncomingRequest, ServerRemoteSession};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn selected_scan_codec_preserves_non_utf8_unix_names_without_native_creation() {
    let name = PathBuf::from(OsString::from_vec(b"name\xff".to_vec()));
    let target = PathBuf::from(OsString::from_vec(b"../target\xfe".to_vec()));
    let wire = scan_request_to_wire(
        ScanRequest::default(),
        WireScanScope::Entry(encode_native_path(&name).unwrap()),
    )
    .unwrap();
    let frame = Frame::new(
        FrameKind::ScanRequest,
        FrameFlags::empty(),
        StreamId::new(1),
        wire.encode(),
    )
    .unwrap();
    let (_, selection) = decode_scan_request(&frame, Platform::current().os).unwrap();
    assert_eq!(selection.unwrap().as_path(), name);
    let entry = Entry::symlink(
        RelativePath::new(name).unwrap(),
        target,
        Timestamp::UNIX_EPOCH,
    );
    let decoded = WireEntry::decode(&entry_to_wire(&entry).unwrap().encode().unwrap()).unwrap();
    assert_eq!(
        wire_to_entry(decoded, Platform::current().os).unwrap(),
        entry
    );
}

async fn sessions(root: &Path, operation: Operation) -> (ClientRemoteSession, ServerRemoteSession) {
    let (client_io, server_io) = tokio::io::duplex(1024);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    // One frame per direction exposes retained ScanRequest/Entry/EntryEnd
    // permits and ACK-ordering mistakes rather than hiding them in roomy queues.
    let config = RouterConfig {
        max_inbound_frames: 1,
        max_outbound_frames: 1,
        ..Default::default()
    };
    let (client, server) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(
            ClientRemoteSession::connect(client_reader, client_writer, operation, root, config),
            ServerRemoteSession::accept(server_reader, server_writer, config),
        )
    })
    .await
    .unwrap();
    (client.unwrap(), server.unwrap())
}

async fn shutdown(mut client: ClientRemoteSession, mut server: ServerRemoteSession) {
    let (client, server) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(client.shutdown(), server.shutdown())
    })
    .await
    .unwrap();
    client.unwrap();
    server.unwrap();
}

async fn selected(
    client: &ClientRemoteSession,
    server: &mut ServerRemoteSession,
    path: &Path,
    request: ScanRequest,
) -> Vec<Entry> {
    tokio::time::timeout(DEADLINE, async {
        let mut entries = client
            .request_handle()
            .scan_entry(request, RelativePath::new(path).unwrap())
            .await
            .unwrap();
        let Some(IncomingRequest::Scan(incoming)) = server.next_request().await.unwrap() else {
            panic!("expected routed scan");
        };
        let handler = server.scan_handler();
        let receive = async {
            let mut observed = Vec::new();
            while let Some(entry) = entries.next().await {
                observed.push(entry.map_err(RemoteScanError::LocalScan)?);
            }
            entries.close().await.map_err(RemoteScanError::LocalScan)?;
            Ok::<_, RemoteScanError>(observed)
        };
        let (observed, ()) = tokio::try_join!(receive, handler.serve(incoming)).unwrap();
        observed
    })
    .await
    .unwrap()
}

fn metadata_request() -> ScanRequest {
    ScanRequest {
        metadata: EntryMetadataRequest {
            unix_mode: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn selected_scan_never_walks_siblings_and_preserves_opaque_names_and_targets() {
    let root = tempfile::tempdir().unwrap();
    // APFS rejects non-UTF-8 names at creation. Linux exercises actual raw
    // filenames; a separate scan codec test covers their bytes on all Unix.
    let file_name = PathBuf::from(OsString::from_vec(if cfg!(target_os = "linux") {
        b"selected\xff".to_vec()
    } else {
        "selected-é".as_bytes().to_vec()
    }));
    let link_name = PathBuf::from(OsString::from_vec(if cfg!(target_os = "linux") {
        b"link\xfe".to_vec()
    } else {
        "link-é".as_bytes().to_vec()
    }));
    let target = PathBuf::from(OsString::from_vec(b"../opaque\xfd/target".to_vec()));
    std::fs::write(root.path().join(&file_name), b"only this metadata").unwrap();
    symlink(&target, root.path().join(&link_name)).unwrap();
    std::fs::create_dir(root.path().join("directory")).unwrap();
    std::fs::write(root.path().join("directory/child"), b"must not enumerate").unwrap();
    // A whole-tree scan plus filtering cannot succeed: this socket is an
    // unsupported native entry, and an unreadable directory also blocks descent.
    let _socket =
        std::os::unix::net::UnixListener::bind(root.path().join("a-unsupported")).unwrap();
    std::fs::create_dir(root.path().join("b-unreadable")).unwrap();
    std::fs::set_permissions(
        root.path().join("b-unreadable"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    let huge = std::fs::File::create(root.path().join("huge")).unwrap();
    huge.set_len(64 * 1024 * 1024 * 1024).unwrap();
    for index in 0..512 {
        std::fs::write(root.path().join(format!("sibling{index:04}")), b"ignored").unwrap();
    }

    let (client, mut server) = sessions(root.path(), Operation::Pull).await;
    let file = selected(&client, &mut server, &file_name, metadata_request()).await;
    assert_eq!(file.len(), 1);
    let metadata = std::fs::metadata(root.path().join(&file_name)).unwrap();
    assert_eq!(file[0].path.as_path(), file_name);
    assert_eq!(file[0].kind, EntryKind::File);
    assert_eq!(file[0].size, metadata.len());
    assert_eq!(file[0].unix_mode, Some(metadata.mode() & 0o7777));
    assert_eq!(
        file[0].identity,
        metadata_identity(&metadata, EntryKind::File)
    );
    assert_eq!(
        file[0].modified,
        Timestamp::new(metadata.mtime(), metadata.mtime_nsec() as u32).unwrap()
    );
    assert!(file[0].symlink_target.is_none());
    assert!(file[0].hardlink_group.is_none());

    let link = selected(&client, &mut server, &link_name, metadata_request()).await;
    assert_eq!(link.len(), 1);
    assert_eq!(link[0].path.as_path(), link_name);
    assert_eq!(link[0].kind, EntryKind::Symlink);
    assert_eq!(link[0].symlink_target.as_ref(), Some(&target));
    let link_metadata = std::fs::symlink_metadata(root.path().join(&link_name)).unwrap();
    assert_eq!(
        link[0].identity,
        metadata_identity(&link_metadata, EntryKind::Symlink)
    );

    let lean = ScanRequest {
        // Depth applies to tree descent, not whether the selected name exists.
        max_depth: Some(0),
        metadata: EntryMetadataRequest {
            unix_mode: false,
            symlink_target: false,
            identity: false,
            hardlink_group: false,
        },
        ..Default::default()
    };
    let link = selected(&client, &mut server, &link_name, lean).await;
    assert_eq!(link.len(), 1);
    assert_eq!(link[0].kind, EntryKind::Symlink);
    assert!(link[0].identity.is_none());
    assert!(link[0].unix_mode.is_none());
    assert!(link[0].symlink_target.is_none());

    let directory = selected(
        &client,
        &mut server,
        Path::new("directory"),
        metadata_request(),
    )
    .await;
    assert_eq!(directory.len(), 1);
    assert_eq!(directory[0].kind, EntryKind::Directory);
    assert_eq!(directory[0].path.as_path(), Path::new("directory"));
    assert!(selected(
        &client,
        &mut server,
        Path::new("missing"),
        metadata_request()
    )
    .await
    .is_empty());

    // The transport may observe a directory for a destination operand. Only
    // source SelectedLeaf reconciliation forbids treating it as a leaf source.
    let path = RelativePath::new("directory").unwrap();
    let mut reconciler = OrderedReconciler::with_scope(
        EntryStream::new(futures::stream::iter(directory.into_iter().map(Ok))),
        EntryStream::new(futures::stream::empty()),
        SyncScope::SelectedLeaf {
            source: path.clone(),
            destination: path,
        },
    );
    assert!(matches!(
        reconciler.next().await,
        Err(EngineError::Invariant(
            "directory entry requires a tree scope"
        ))
    ));
    reconciler.close().await.unwrap();
    shutdown(client, server).await;
    // Restore cleanup permission; the scan itself must never do this.
    std::fs::set_permissions(
        root.path().join("b-unreadable"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
}

#[tokio::test]
async fn selected_scan_observes_original_read_only_root_after_path_replacement() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("leaf"), b"original inode bytes").unwrap();
    let old_metadata = std::fs::metadata(root.join("leaf")).unwrap();
    let (client, mut server) = sessions(&root, Operation::Pull).await;
    std::fs::rename(&root, parent.path().join("original")).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("leaf"), b"new").unwrap();
    let entries = selected(&client, &mut server, Path::new("leaf"), metadata_request()).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].size, old_metadata.len());
    assert_eq!(
        entries[0].identity,
        metadata_identity(&old_metadata, EntryKind::File)
    );
    assert_ne!(
        entries[0].identity,
        metadata_identity(
            &std::fs::metadata(root.join("leaf")).unwrap(),
            EntryKind::File
        )
    );
    assert_eq!(
        std::fs::read(parent.path().join("original/leaf")).unwrap(),
        b"original inode bytes"
    );
    assert_eq!(std::fs::read(root.join("leaf")).unwrap(), b"new");
    shutdown(client, server).await;
}

#[tokio::test]
async fn selected_scan_mode000_is_metadata_only_for_unprivileged_uid() {
    // SAFETY: geteuid has no pointer arguments or side effects.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "metadata-only proof requires an unprivileged UID"
    );
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("leaf");
    std::fs::write(&path, b"no payload open allowed").unwrap();
    let held = std::fs::File::open(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
    assert_eq!(
        std::fs::File::open(&path).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let (client, mut server) = sessions(root.path(), Operation::Pull).await;
    let entries = selected(&client, &mut server, Path::new("leaf"), metadata_request()).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].size, held.metadata().unwrap().len());
    assert_eq!(entries[0].unix_mode, Some(0));
    assert_eq!(
        entries[0].identity,
        metadata_identity(&held.metadata().unwrap(), EntryKind::File)
    );
    shutdown(client, server).await;
}

async fn raw_request(
    client: &ClientRemoteSession,
    server: &mut ServerRemoteSession,
    payload: bytes::Bytes,
) -> Result<()> {
    let sender = client.sender();
    let inbox = sender.open_stream().unwrap();
    sender
        .send(
            Frame::new(
                FrameKind::ScanRequest,
                FrameFlags::empty(),
                inbox.stream_id(),
                payload,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let Some(IncomingRequest::Scan(incoming)) = server.next_request().await.unwrap() else {
        panic!("expected scan request");
    };
    let result = server.scan_handler().serve(incoming).await;
    drop(inbox);
    result
}

#[tokio::test]
async fn selected_scan_rejects_native_paths_before_absent_or_present_producers() {
    for present in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("preview");
        if present {
            std::fs::create_dir(&root).unwrap();
        }
        let (client, mut server) = sessions(&root, Operation::PreviewPush).await;
        for name in [
            b"".as_slice(),
            b"..",
            b".",
            b"/absolute",
            b"a/b",
            b"a/",
            b"a/.",
            b"a\0b",
            b"../escape",
        ] {
            let wire = scan_request_to_wire(
                ScanRequest::default(),
                WireScanScope::Entry(WirePath::new(name.to_vec()).unwrap()),
            )
            .unwrap();
            let result =
                tokio::time::timeout(DEADLINE, raw_request(&client, &mut server, wire.encode()))
                    .await
                    .unwrap();
            assert!(
                matches!(
                    result,
                    Err(RemoteScanError::InvalidPathComponent | RemoteScanError::PathContainsNul)
                ),
                "path {name:?}: {result:?}"
            );
        }
        // Unknown selection, oversized claimed length, truncated Entry payload,
        // and unknown scan flags must not be successful empty observations.
        for payload in [
            vec![0, 0, 0, 0, 0, 0, 2],
            vec![0, 0, 0, 0, 0, 0, 1, 255, 255, 255, 255],
            vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2, b'a'],
            vec![128, 0, 0, 0, 0, 0, 0],
        ] {
            assert!(matches!(
                tokio::time::timeout(DEADLINE, raw_request(&client, &mut server, payload.into()))
                    .await
                    .unwrap(),
                Err(RemoteScanError::Protocol(_))
            ));
        }
        assert!(selected(
            &client,
            &mut server,
            Path::new("missing"),
            ScanRequest::default()
        )
        .await
        .is_empty());
        assert_eq!(root.exists(), present);
        shutdown(client, server).await;
    }
}

#[tokio::test]
async fn selected_scan_retains_typed_policy_and_native_failures() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("leaf"), b"leaf").unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
    let (client, mut server) = sessions(root.path(), Operation::Pull).await;
    for (name, request) in [
        (
            "leaf",
            ScanRequest {
                follow_symlinks: true,
                ..Default::default()
            },
        ),
        (
            "leaf",
            ScanRequest {
                respect_gitignore: true,
                ..Default::default()
            },
        ),
        ("socket", ScanRequest::default()),
    ] {
        let mut entries = client
            .request_handle()
            .scan_entry(request, RelativePath::new(name).unwrap())
            .await
            .unwrap();
        let Some(IncomingRequest::Scan(incoming)) = server.next_request().await.unwrap() else {
            panic!("expected scan");
        };
        let result = tokio::time::timeout(DEADLINE, server.scan_handler().serve(incoming))
            .await
            .unwrap();
        if request.follow_symlinks {
            assert!(matches!(
                result,
                Err(RemoteScanError::RemoteFollowUnsupported)
            ));
        } else {
            assert!(
                matches!(result, Err(RemoteScanError::LocalScan(_))),
                "expected typed native refusal, got {result:?}"
            );
        }
        entries.close().await.unwrap();
    }
    shutdown(client, server).await;
}

#[tokio::test]
async fn selected_scan_ack_allows_server_first_owned_finish() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("leaf"), b"leaf").unwrap();
    let (mut client, mut server) = sessions(root.path(), Operation::Pull).await;
    tokio::time::timeout(DEADLINE, async {
        let mut entries = client
            .request_handle()
            .scan_entry(ScanRequest::default(), RelativePath::new("leaf").unwrap())
            .await
            .unwrap();
        let Some(IncomingRequest::Scan(incoming)) = server.next_request().await.unwrap() else {
            panic!("expected scan");
        };
        let handler = server.scan_handler();
        let serving = tokio::spawn(async move {
            handler.serve(incoming).await.unwrap();
            // The agent finishes first as soon as the acknowledged stream ends.
            server.finish().await.unwrap();
        });
        let entry = entries.next().await.unwrap().unwrap();
        assert_eq!(entry.path.as_path(), Path::new("leaf"));
        assert!(!serving.is_finished());
        assert!(entries.next().await.is_none());
        entries.close().await.unwrap();
        client.finish().await.unwrap();
        serving.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn selected_scan_disconnect_drains_handler_and_both_router_owners() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("leaf"), b"leaf").unwrap();
    let (mut client, mut server) = sessions(root.path(), Operation::Pull).await;
    tokio::time::timeout(DEADLINE, async {
        let mut entries = client
            .request_handle()
            .scan_entry(ScanRequest::default(), RelativePath::new("leaf").unwrap())
            .await
            .unwrap();
        let Some(IncomingRequest::Scan(incoming)) = server.next_request().await.unwrap() else {
            panic!("expected scan");
        };
        let handler = server.scan_handler();
        let serving = tokio::spawn(async move { handler.serve(incoming).await });
        assert!(entries.next().await.unwrap().is_ok());
        // Disconnect before acknowledging EntryEnd. The server must join its
        // admitted metadata worker and fail rather than report scan success.
        entries.close().await.unwrap();
        client.shutdown().await.unwrap();
        assert!(serving.await.unwrap().is_err());
        server.shutdown().await.unwrap();
    })
    .await
    .unwrap();
}
