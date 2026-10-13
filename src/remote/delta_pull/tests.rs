use super::*;
use crate::engine::domain::{EntryKind, Timestamp};
use crate::protocol::{
    read_frame, write_frame, StreamId, WireDeltaCopy, WireFileEnd, WireXattrResult,
};
use crate::remote::router::{FrameRouter, RouterRole};
use crate::rooted_fs::RootedFs;
use bytes::Bytes;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

fn entry(root: &Path, name: &str) -> Entry {
    let metadata = std::fs::metadata(root.join(name)).unwrap();
    let mut entry = Entry::file(
        RelativePath::new(name).unwrap(),
        metadata.len(),
        Timestamp::UNIX_EPOCH,
    );
    entry.identity = crate::endpoint::local_identity::metadata_identity(&metadata, EntryKind::File);
    entry.unix_mode = Some(0o640);
    entry
}

fn policy(capabilities: &crate::endpoint::Capabilities) -> FetchPolicy<'_> {
    FetchPolicy {
        compression: None,
        rate_limiter: None,
        preservation: Default::default(),
        capabilities,
    }
}

async fn upload(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> StreamId {
    let opener = read_frame(reader).await.unwrap();
    assert_eq!(opener.kind(), FrameKind::DeltaFetchRequest);
    let request = WireDeltaFetchRequest::decode(opener.payload()).unwrap();
    write_frame(
        writer,
        &Frame::new(
            FrameKind::DeltaFetchReady,
            FrameFlags::empty(),
            opener.stream_id(),
            Bytes::new(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let mut count = 0;
    loop {
        let frame = read_frame(reader).await.unwrap();
        assert!(frame.flags().is_empty());
        match frame.kind() {
            FrameKind::Signature => {
                assert_eq!(
                    WireSignature::decode(frame.payload()).unwrap().index(),
                    count
                );
                count += 1;
            }
            FrameKind::SignatureEnd => {
                let end = WireSignatureEnd::decode(frame.payload()).unwrap();
                assert_eq!(end.block_count(), count);
                assert_eq!(end.file_size(), request.basis_size);
                break;
            }
            kind => panic!("unexpected upload {kind:?}"),
        }
    }
    opener.stream_id()
}

#[tokio::test]
async fn malformed_operations_abort_private_stage_before_publication() {
    // Each case protects a distinct publication boundary, not parser text.
    for case in [
        "range",
        "overflow",
        "oversized",
        "short",
        "digest",
        "compressed",
        "late metadata",
        "unrequested",
        "flags",
        "truncated",
    ] {
        let root = tempfile::tempdir().unwrap();
        let original = vec![b'o'; 4096];
        std::fs::write(root.path().join("file"), &original).unwrap();
        let old = entry(root.path(), "file");
        let source = old.clone();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (mut sr, mut sw) = tokio::io::split(server_io);
        let client = FrameRouter::start(cr, cw, RouterRole::Client, Default::default()).unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let id = upload(&mut sr, &mut sw).await;
            let copy = WireDeltaCopy::new(0, 4096).unwrap();
            let mut frames = Vec::new();
            let mut copy_payload = copy.encode();
            match case {
                "range" => copy_payload = WireDeltaCopy::new(4096, 1).unwrap().encode(),
                "overflow" => {
                    let mut payload = u64::MAX.to_be_bytes().to_vec();
                    payload.extend_from_slice(&1_u32.to_be_bytes());
                    copy_payload = payload.into();
                }
                "oversized" => {
                    let mut payload = 0_u64.to_be_bytes().to_vec();
                    payload.extend_from_slice(
                        &(crate::protocol::MAX_DELTA_COPY_SIZE + 1).to_be_bytes(),
                    );
                    copy_payload = payload.into();
                }
                "compressed" => frames.push(
                    Frame::new(
                        FrameKind::Data,
                        FrameFlags::COMPRESSED,
                        id,
                        Bytes::from_static(b"not zstd"),
                    )
                    .unwrap(),
                ),
                "truncated" => {
                    drop(sw);
                    return;
                }
                _ => {}
            }
            if case != "short" {
                frames.push(
                    Frame::new(
                        FrameKind::DeltaCopy,
                        if case == "flags" {
                            FrameFlags::COMPRESSED
                        } else {
                            FrameFlags::empty()
                        },
                        id,
                        copy_payload,
                    )
                    .unwrap(),
                );
            }
            if matches!(case, "late metadata" | "unrequested") {
                let metadata = Frame::new(
                    FrameKind::FileXattrs,
                    FrameFlags::empty(),
                    id,
                    WireXattrResult::new(Vec::new()).unwrap().encode().unwrap(),
                )
                .unwrap();
                if case == "unrequested" {
                    frames.insert(0, metadata);
                } else {
                    frames.push(metadata);
                }
            }
            frames.push(
                Frame::new(
                    FrameKind::FileEnd,
                    FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                    id,
                    WireFileEnd::new(
                        4096,
                        if case == "digest" {
                            [1; 32]
                        } else {
                            *blake3::hash(&vec![b'o'; 4096]).as_bytes()
                        },
                    )
                    .encode(),
                )
                .unwrap(),
            );
            for frame in frames {
                if write_frame(&mut sw, &frame).await.is_err() {
                    break;
                }
            }
            let _ = released.await;
        });
        let caps = crate::endpoint::Capabilities::local();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_delta(
                &client.sender(),
                &source,
                crate::protocol::Platform::current().os,
                PullTarget {
                    endpoint: LocalEndpoint::new(root.path().to_path_buf())
                        .with_rooted_authority(rooted),
                    path: old.path.clone(),
                    old: old.clone(),
                    unix_mode: 0o640,
                    modified: None,
                    final_flags: None,
                },
                policy(&caps),
            ),
        )
        .await
        .unwrap();
        let _ = release.send(());
        server.await.unwrap();
        assert!(result.is_err(), "{case} must not publish");
        assert_eq!(
            std::fs::read(root.path().join("file")).unwrap(),
            original,
            "{case}"
        );
        assert_eq!(entry(root.path(), "file").identity, old.identity, "{case}");
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "private cleanup: {case}"
        );
    }
}

#[tokio::test]
async fn old_races_after_signing_refuse_even_same_inode_under_foreign_parent() {
    for parent in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::create_dir(root.path().join("alternate")).unwrap();
        std::fs::write(root.path().join("dir/file"), vec![b'o'; 4096]).unwrap();
        std::fs::hard_link(
            root.path().join("dir/file"),
            root.path().join("alternate/file"),
        )
        .unwrap();
        let old = entry(root.path(), "dir/file");
        let source = old.clone();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (mut sr, mut sw) = tokio::io::split(server_io);
        let client = FrameRouter::start(cr, cw, RouterRole::Client, Default::default()).unwrap();
        let path = root.path().to_path_buf();
        let identity = old.identity;
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let id = upload(&mut sr, &mut sw).await;
            let bytes = if parent {
                std::fs::rename(path.join("dir"), path.join("held")).unwrap();
                std::fs::rename(path.join("alternate"), path.join("dir")).unwrap();
                assert_eq!(
                    entry(&path, "dir/file").identity,
                    identity,
                    "named inode checks alone pass"
                );
                vec![b'o'; 4096]
            } else {
                let bytes = vec![b'r'; 4096];
                std::fs::write(path.join("dir/file"), &bytes).unwrap();
                bytes
            };
            for frame in [
                Frame::new(
                    FrameKind::DeltaCopy,
                    FrameFlags::empty(),
                    id,
                    WireDeltaCopy::new(0, 4096).unwrap().encode(),
                )
                .unwrap(),
                Frame::new(
                    FrameKind::FileEnd,
                    FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                    id,
                    WireFileEnd::new(4096, *blake3::hash(&bytes).as_bytes()).encode(),
                )
                .unwrap(),
            ] {
                write_frame(&mut sw, &frame).await.unwrap();
            }
            let _ = released.await;
        });
        let caps = crate::endpoint::Capabilities::local();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_delta(
                &client.sender(),
                &source,
                crate::protocol::Platform::current().os,
                PullTarget {
                    endpoint: LocalEndpoint::new(root.path().to_path_buf())
                        .with_rooted_authority(rooted),
                    path: old.path.clone(),
                    old,
                    unix_mode: 0o640,
                    modified: None,
                    final_flags: None,
                },
                policy(&caps),
            ),
        )
        .await
        .unwrap();
        let _ = release.send(());
        server.await.unwrap();
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(root.path().join("dir/file")).unwrap(),
            vec![if parent { b'o' } else { b'r' }; 4096]
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("dir")).unwrap().count(),
            1
        );
    }
}

#[tokio::test]
async fn unsupported_delta_capability_and_mandatory_old_read_are_not_fallbacks() {
    // Root bypasses DAC, so unreadability cannot be asserted in that environment.
    // SAFETY: geteuid takes no arguments and has no memory preconditions.
    let root_user = unsafe { libc::geteuid() } == 0;
    for unreadable in [false, true] {
        if unreadable && root_user {
            continue;
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        if unreadable {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
        }
        let old = entry(root.path(), "file");
        let source = old.clone();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (io, _peer) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(io);
        let client = FrameRouter::start(cr, cw, RouterRole::Client, Default::default()).unwrap();
        let mut caps = crate::endpoint::Capabilities::local();
        caps.delta_pull = unreadable;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_delta(
                &client.sender(),
                &source,
                crate::protocol::Platform::current().os,
                PullTarget {
                    endpoint: LocalEndpoint::new(root.path().to_path_buf())
                        .with_rooted_authority(rooted),
                    path: old.path.clone(),
                    old: old.clone(),
                    unix_mode: 0o640,
                    modified: None,
                    final_flags: None,
                },
                policy(&caps),
            ),
        )
        .await
        .unwrap();
        if unreadable {
            assert!(
                matches!(result, Err(RemoteTransferError::RootedFs(crate::rooted_fs::RootedFsError::Io(ref error))) if error.kind() == std::io::ErrorKind::PermissionDenied),
                "{result:?}"
            );
            assert_eq!(
                entry(root.path(), "file").identity,
                old.identity,
                "no chmod workaround"
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        } else {
            assert!(matches!(
                result,
                Err(RemoteTransferError::PreservationUnavailable {
                    feature: "delta PULL"
                })
            ));
        }
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    }
}

#[tokio::test]
async fn interrupted_upload_and_reconstruction_join_workers_and_abort_staging() {
    for phase in ["upload EOF", "early ACK", "reconstruction EOF"] {
        let root = tempfile::tempdir().unwrap();
        let original = vec![b'o'; 12 * 1024 * 1024];
        std::fs::write(root.path().join("file"), &original).unwrap();
        let old = entry(root.path(), "file");
        let source = old.clone();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (mut sr, mut sw) = tokio::io::split(server_io);
        let client = FrameRouter::start(cr, cw, RouterRole::Client, Default::default()).unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            if phase == "reconstruction EOF" {
                upload(&mut sr, &mut sw).await;
                return;
            }
            let opener = read_frame(&mut sr).await.unwrap();
            assert_eq!(opener.kind(), FrameKind::DeltaFetchRequest);
            write_frame(
                &mut sw,
                &Frame::new(
                    FrameKind::DeltaFetchReady,
                    FrameFlags::empty(),
                    opener.stream_id(),
                    Bytes::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(
                read_frame(&mut sr).await.unwrap().kind(),
                FrameKind::Signature
            );
            // The remaining 3,071 signatures cannot fit in this pipe. Stop
            // consuming here to exercise a backpressured native producer.
            if phase == "early ACK" {
                write_frame(
                    &mut sw,
                    &Frame::new(
                        FrameKind::Ack,
                        FrameFlags::empty(),
                        opener.stream_id(),
                        Bytes::new(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
                let _ = released.await;
            }
        });
        let caps = crate::endpoint::Capabilities::local();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_delta(
                &client.sender(),
                &source,
                crate::protocol::Platform::current().os,
                PullTarget {
                    endpoint: LocalEndpoint::new(root.path().to_path_buf())
                        .with_rooted_authority(rooted),
                    path: old.path.clone(),
                    old: old.clone(),
                    unix_mode: 0o640,
                    modified: None,
                    final_flags: None,
                },
                policy(&caps),
            ),
        )
        .await
        .unwrap();
        let _ = release.send(());
        server.await.unwrap();
        assert!(result.is_err(), "{phase}");
        if phase == "early ACK" {
            assert!(
                matches!(
                    result,
                    Err(RemoteTransferError::PrematureFetchFrame(FrameKind::Ack))
                ),
                "{result:?}"
            );
        }
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), original);
        assert_eq!(entry(root.path(), "file").identity, old.identity);
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "{phase}"
        );
    }
}
