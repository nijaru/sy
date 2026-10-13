use super::*;
use crate::protocol::{read_frame, write_frame, StreamId};
use crate::remote::fetch::serve_incoming_file_fetch;
use crate::remote::router::{FrameRouter, RouterRole};
use crate::rooted_fs::RootedFs;
use bytes::Bytes;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn request(root: &std::path::Path, name: &str) -> WireDeltaFetchRequest {
    let metadata = std::fs::metadata(root.join(name)).unwrap();
    let identity = crate::endpoint::local_identity::metadata_identity(
        &metadata,
        crate::engine::domain::EntryKind::File,
    )
    .unwrap();
    WireDeltaFetchRequest {
        source: WireFileFetchRequest::new(
            crate::remote::path::encode_relative_path(std::path::Path::new(name)).unwrap(),
            metadata.len(),
            *identity.as_bytes(),
            WireFetchCompression::None,
        ),
        block_size: SignatureBlockSize::new(4096).unwrap(),
        basis_size: 4096,
        basis_identity: [9; 32],
    }
}

fn signature(id: StreamId) -> Frame {
    Frame::new(
        FrameKind::Signature,
        FrameFlags::empty(),
        id,
        WireSignature::new(0, 4096, 0, [0; 16]).unwrap().encode(),
    )
    .unwrap()
}

fn footer(id: StreamId) -> Frame {
    Frame::new(
        FrameKind::SignatureEnd,
        FrameFlags::empty(),
        id,
        WireSignatureEnd::new(4096, 1, [9; 32]).unwrap().encode(),
    )
    .unwrap()
}

async fn server(
    root: &std::path::Path,
    io: tokio::io::DuplexStream,
) -> tokio::task::JoinHandle<Result<crate::engine::work::TransferSummary>> {
    let mut rooted = RootedFs::open(root.to_path_buf()).await.unwrap();
    crate::rooted_fs::test_source::scan(&rooted).await;
    let (sr, sw) = tokio::io::split(io);
    let router = FrameRouter::start(sr, sw, RouterRole::Server, Default::default()).unwrap();
    rooted.bind_session_mutations(router.sender().publication_admission(), true);
    tokio::spawn(async move {
        let mut router = router;
        let incoming = router.incoming().recv().await.unwrap().unwrap();
        serve_incoming_file_fetch(
            rooted,
            incoming,
            &router.sender(),
            crate::protocol::Platform::current().os,
        )
        .await
    })
}

#[tokio::test]
async fn signature_upload_rejects_invalid_membership_truncation_and_phase() {
    for case in [
        "missing",
        "duplicate",
        "index",
        "short",
        "truncated",
        "flags",
        "wrong footer",
        "mutation",
        "readiness",
        "ack",
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"SOURCE").unwrap();
        let request = request(root.path(), "file");
        let before = request.source.identity();
        let (mut client, io) = tokio::io::duplex(4096);
        let server = server(root.path(), io).await;
        let id = StreamId::new(1);
        write_frame(
            &mut client,
            &Frame::new(
                FrameKind::DeltaFetchRequest,
                FrameFlags::empty(),
                id,
                request.encode(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ready = read_frame(&mut client).await.unwrap();
        assert_eq!(ready.kind(), FrameKind::DeltaFetchReady);
        assert!(ready.flags().is_empty() && ready.payload().is_empty());
        let invalid = match case {
            "missing" => footer(id),
            "index" => Frame::new(
                FrameKind::Signature,
                FrameFlags::empty(),
                id,
                WireSignature::new(1, 4096, 0, [0; 16]).unwrap().encode(),
            )
            .unwrap(),
            "short" => Frame::new(
                FrameKind::Signature,
                FrameFlags::empty(),
                id,
                WireSignature::new(0, 1, 0, [0; 16]).unwrap().encode(),
            )
            .unwrap(),
            "truncated" => Frame::new(
                FrameKind::Signature,
                FrameFlags::empty(),
                id,
                Bytes::from_static(&[0; 31]),
            )
            .unwrap(),
            "flags" => Frame::new(
                FrameKind::Signature,
                FrameFlags::COMPRESSED,
                id,
                signature(id).payload().clone(),
            )
            .unwrap(),
            "mutation" | "readiness" | "ack" => Frame::new(
                match case {
                    "mutation" => FrameKind::Mutation,
                    "readiness" => FrameKind::DeltaFetchReady,
                    _ => FrameKind::Ack,
                },
                FrameFlags::empty(),
                id,
                Bytes::new(),
            )
            .unwrap(),
            "wrong footer" => {
                write_frame(&mut client, &signature(id)).await.unwrap();
                Frame::new(
                    FrameKind::SignatureEnd,
                    FrameFlags::empty(),
                    id,
                    WireSignatureEnd::new(4096, 1, [8; 32]).unwrap().encode(),
                )
                .unwrap()
            }
            "duplicate" => {
                write_frame(&mut client, &signature(id)).await.unwrap();
                signature(id)
            }
            _ => unreachable!(),
        };
        write_frame(&mut client, &invalid).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err(), "{case}");
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"SOURCE");
        assert_eq!(request_source_identity(root.path()), before);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn runtime_refuses_delta_without_negotiated_capability_or_source_role() {
    use crate::protocol::{CapabilitySet, ClientHello, Operation, Platform, SessionOpen};
    for (operation, missing) in [
        (Operation::Pull, CapabilitySet::DELTA_PULL),
        (Operation::Pull, CapabilitySet::BLAKE3),
        (Operation::Push, CapabilitySet::empty()),
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        let request = request(root.path(), "file");
        let (mut client, io) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(io);
        let server = tokio::spawn(async move {
            let mut session =
                crate::remote::runtime::ServerRemoteSession::accept(sr, sw, Default::default())
                    .await
                    .unwrap();
            session.next_request().await.err().unwrap()
        });
        let mut capabilities = CapabilitySet::all();
        capabilities.remove(missing);
        let hello = ClientHello::new(
            crate::protocol::SUPPORTED_VERSIONS,
            capabilities,
            Platform::current(),
            "test",
        )
        .unwrap();
        write_frame(
            &mut client,
            &Frame::control(FrameKind::ClientHello, hello.encode().unwrap()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            read_frame(&mut client).await.unwrap().kind(),
            FrameKind::ServerHello
        );
        let open = SessionOpen::new(
            operation,
            crate::remote::encode_target_root(root.path(), Platform::current().os).unwrap(),
        );
        write_frame(
            &mut client,
            &Frame::control(FrameKind::SessionOpen, open.encode().unwrap()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            read_frame(&mut client).await.unwrap().kind(),
            FrameKind::SessionReady
        );
        write_frame(
            &mut client,
            &Frame::new(
                FrameKind::DeltaFetchRequest,
                FrameFlags::empty(),
                StreamId::new(1),
                request.encode(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        match operation {
            Operation::Pull => assert!(matches!(
                error,
                crate::remote::runtime::RemoteSessionError::Transfer(
                    RemoteTransferError::PreservationUnavailable {
                        feature: "delta PULL"
                    }
                )
            )),
            Operation::Push => assert!(matches!(
                error,
                crate::remote::runtime::RemoteSessionError::OperationMismatch {
                    kind: FrameKind::DeltaFetchRequest,
                    ..
                }
            )),
            _ => unreachable!(),
        }
        assert_eq!(
            std::fs::read(root.path().join("file")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

fn request_source_identity(root: &std::path::Path) -> [u8; 32] {
    request(root, "file").source.identity()
}

#[tokio::test]
async fn source_races_during_delta_generation_never_authorize_file_end() {
    for parent in [false, true] {
        let root = tempfile::tempdir().unwrap();
        for name in ["dir", "alternate"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let path = root.path().join("dir/file");
        std::fs::write(
            &path,
            vec![b'x'; (PRODUCER_QUEUE_DEPTH + 8) * crate::protocol::MAX_TRANSFER_DATA_SIZE],
        )
        .unwrap();
        std::fs::hard_link(&path, root.path().join("alternate/file")).unwrap();
        let request = request(root.path(), "dir/file");
        let expected = request.source.identity();
        let (mut client, io) = tokio::io::duplex(4096);
        let mut server = server(root.path(), io).await;
        let id = StreamId::new(1);
        for frame in [
            Frame::new(
                FrameKind::DeltaFetchRequest,
                FrameFlags::empty(),
                id,
                request.encode(),
            )
            .unwrap(),
            signature(id),
            footer(id),
        ] {
            let opener = frame.kind() == FrameKind::DeltaFetchRequest;
            write_frame(&mut client, &frame).await.unwrap();
            if opener {
                assert_eq!(
                    read_frame(&mut client).await.unwrap().kind(),
                    FrameKind::DeltaFetchReady
                );
            }
        }
        assert_eq!(
            read_frame(&mut client).await.unwrap().kind(),
            FrameKind::Data
        );
        if parent {
            std::fs::rename(root.path().join("dir"), root.path().join("held")).unwrap();
            std::fs::rename(root.path().join("alternate"), root.path().join("dir")).unwrap();
            assert_eq!(
                self::request(root.path(), "dir/file").source.identity(),
                expected,
                "named inode and genuine FD still match"
            );
        } else {
            std::fs::write(&path, b"changed").unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut server => break result.unwrap(),
                    frame = read_frame(&mut client) => match frame {
                        Ok(frame) => assert_eq!(frame.kind(), FrameKind::Data, "SOURCE race must not authorize publication"),
                        Err(_) => break server.await.unwrap(),
                    }
                }
            }
        }).await.unwrap();
        assert!(result.is_err());
    }
}

#[tokio::test]
async fn late_signature_and_early_ack_are_refused_before_footer() {
    for early_kind in [
        FrameKind::Signature,
        FrameKind::Ack,
        FrameKind::Mutation,
        FrameKind::Cancel,
        FrameKind::DeltaFetchReady,
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("file"),
            vec![b'x'; (PRODUCER_QUEUE_DEPTH + 8) * crate::protocol::MAX_TRANSFER_DATA_SIZE],
        )
        .unwrap();
        let request = request(root.path(), "file");
        let before = request.source.identity();
        let (mut client, io) = tokio::io::duplex(4096);
        let mut server = server(root.path(), io).await;
        let id = StreamId::new(1);
        for frame in [
            Frame::new(
                FrameKind::DeltaFetchRequest,
                FrameFlags::empty(),
                id,
                request.encode(),
            )
            .unwrap(),
            signature(id),
            footer(id),
        ] {
            let opener = frame.kind() == FrameKind::DeltaFetchRequest;
            write_frame(&mut client, &frame).await.unwrap();
            if opener {
                assert_eq!(
                    read_frame(&mut client).await.unwrap().kind(),
                    FrameKind::DeltaFetchReady
                );
            }
        }
        assert_eq!(
            read_frame(&mut client).await.unwrap().kind(),
            FrameKind::Data
        );
        write_frame(
            &mut client,
            &Frame::new(early_kind, FrameFlags::empty(), id, Bytes::new()).unwrap(),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut server => break result.unwrap(),
                    frame = read_frame(&mut client) => match frame {
                        Ok(frame) => assert_ne!(frame.kind(), FrameKind::FileEnd, "late upload must not authorize commit"),
                        Err(_) => break server.await.unwrap(),
                    }
                }
            }
        }).await.unwrap();
        assert!(
            matches!(result, Err(RemoteTransferError::PrematureFetchFrame(actual)) if actual == early_kind),
            "{result:?}"
        );
        assert_eq!(request_source_identity(root.path()), before);
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "upload grants no mutation"
        );
    }
}

#[tokio::test]
async fn source_unreadability_is_mandatory_and_never_repaired() {
    // SAFETY: geteuid has no arguments or memory preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file");
    std::fs::write(&path, b"SOURCE").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
    let request = request(root.path(), "file");
    let before = request.source.identity();
    let (mut client, io) = tokio::io::duplex(4096);
    let server = server(root.path(), io).await;
    write_frame(
        &mut client,
        &Frame::new(
            FrameKind::DeltaFetchRequest,
            FrameFlags::empty(),
            StreamId::new(1),
            request.encode(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, Err(RemoteTransferError::Io(ref error)) if matches!(error.get_ref().and_then(|cause| cause.downcast_ref::<crate::remote::source_file::SourceFileError>()), Some(crate::remote::source_file::SourceFileError::Rooted(crate::rooted_fs::RootedFsError::Io(error))) if error.kind() == std::io::ErrorKind::PermissionDenied)),
        "{result:?}"
    );
    assert_eq!(request_source_identity(root.path()), before);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"SOURCE");
}

#[tokio::test]
async fn original_scan_binding_is_required_before_delta_reads_and_old_survives() {
    use crate::engine::scan::{EntryMetadataRequest, ScanRequest};
    use futures::TryStreamExt;
    for parent in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        for dir in ["dir", "alternate"] {
            std::fs::create_dir(source.path().join(dir)).unwrap();
        }
        std::fs::write(source.path().join("dir/file"), vec![b's'; 8192]).unwrap();
        std::fs::hard_link(
            source.path().join("dir/file"),
            source.path().join("alternate/file"),
        )
        .unwrap();
        std::fs::write(destination.path().join("file"), vec![b'o'; 4096]).unwrap();
        let mut rooted = RootedFs::open(source.path().to_path_buf()).await.unwrap();
        let target = RootedFs::open(destination.path().to_path_buf())
            .await
            .unwrap();
        let scan = ScanRequest {
            metadata: EntryMetadataRequest {
                identity: true,
                unix_mode: true,
                ..Default::default()
            },
            respect_gitignore: false,
            ..Default::default()
        };
        // Only the serving SOURCE's real scanner establishes original records.
        let source_entry = rooted
            .source_entry_stream(scan)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.path.as_path() == std::path::Path::new("dir/file"))
            .unwrap();
        let old = target
            .entry_stream(scan)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .pop()
            .unwrap();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let client = FrameRouter::start(cr, cw, RouterRole::Client, Default::default()).unwrap();
        let mut router =
            FrameRouter::start(sr, sw, RouterRole::Server, Default::default()).unwrap();
        rooted.bind_session_mutations(router.sender().publication_admission(), true);
        let server = tokio::spawn(async move {
            let incoming = router.incoming().recv().await.unwrap().unwrap();
            serve_incoming_file_fetch(
                rooted,
                incoming,
                &router.sender(),
                crate::protocol::Platform::current().os,
            )
            .await
        });
        if parent {
            std::fs::rename(source.path().join("dir"), source.path().join("held")).unwrap();
            std::fs::rename(source.path().join("alternate"), source.path().join("dir")).unwrap();
            assert_eq!(
                request(source.path(), "dir/file").source.identity(),
                *source_entry.identity.unwrap().as_bytes()
            );
        } else {
            std::fs::write(source.path().join("dir/file"), vec![b'r'; 8192]).unwrap();
        }
        let caps = crate::endpoint::Capabilities::local();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_delta(
                &client.sender(),
                &source_entry,
                crate::protocol::Platform::current().os,
                PullTarget {
                    endpoint: LocalEndpoint::new(destination.path().to_path_buf())
                        .with_rooted_authority(target),
                    path: old.path.clone(),
                    old,
                    unix_mode: 0o640,
                    modified: None,
                    final_flags: None,
                },
                FetchPolicy {
                    compression: None,
                    rate_limiter: None,
                    preservation: Default::default(),
                    capabilities: &caps,
                },
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err(), "SOURCE race cannot publish: {parent}");
        let error = server.await.unwrap().unwrap_err();
        assert!(
            matches!(error, RemoteTransferError::Io(ref cause)
            if matches!(cause.get_ref().and_then(|cause| cause.downcast_ref::<crate::remote::source_file::SourceFileError>()),
                Some(crate::remote::source_file::SourceFileError::Changed { .. }))),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read(destination.path().join("file")).unwrap(),
            vec![b'o'; 4096]
        );
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
    }
}
