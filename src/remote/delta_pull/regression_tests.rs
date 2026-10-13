use super::*;
use crate::engine::domain::{EntryKind, Timestamp};
use crate::protocol::{read_frame, write_frame, Operation, Platform, WireFileEnd};
use crate::remote::router::{FrameRouter, RouterRole};
use crate::rooted_fs::{PublicationPause, PublicationPausePoint, RootedFs};
use bytes::Bytes;
use futures::TryStreamExt;
use std::time::Duration;

fn frame(kind: FrameKind, id: crate::protocol::StreamId, payload: Bytes) -> Frame {
    Frame::new(kind, FrameFlags::empty(), id, payload).unwrap()
}

fn old_entry(root: &std::path::Path) -> Entry {
    let metadata = std::fs::metadata(root.join("file")).unwrap();
    let mut old = Entry::file(
        RelativePath::new("file").unwrap(),
        metadata.len(),
        Timestamp::UNIX_EPOCH,
    );
    old.identity = crate::endpoint::local_identity::metadata_identity(&metadata, EntryKind::File);
    old
}

#[tokio::test]
async fn queued_delta_requests_do_not_starve_admitted_uploads_or_completion_acks() {
    // Four 24 MiB requests compete for 64 MiB. Each signature upload is four
    // times the global 128-frame inbound window. Only admitted streams may
    // upload; waiting openers cannot block the ACK that admits the next pair.
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), vec![b's'; 4096]).unwrap();
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (sr, sw) = tokio::io::split(server_io);
    let server = tokio::spawn(crate::remote::serve::serve_transport(
        sr,
        sw,
        Default::default(),
    ));
    let (cr, cw) = tokio::io::split(client_io);
    let client = crate::remote::runtime::ClientRemoteSession::connect(
        cr,
        cw,
        Operation::Pull,
        &root,
        Default::default(),
    )
    .await
    .unwrap();
    let entries: Vec<_> = client
        .scan(crate::engine::scan::ScanRequest {
            metadata: crate::engine::scan::EntryMetadataRequest {
                identity: true,
                ..Default::default()
            },
            respect_gitignore: false,
            ..Default::default()
        })
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let source = entries.iter().find(|entry| entry.is_file()).unwrap();
    let request = WireDeltaFetchRequest {
        source: WireFileFetchRequest::new(
            crate::remote::path::encode_relative_path(source.path.as_path()).unwrap(),
            source.size,
            *source.identity.unwrap().as_bytes(),
            WireFetchCompression::None,
        ),
        block_size: SignatureBlockSize::new(4096).unwrap(),
        basis_size: 512 * 4096,
        basis_identity: [9; 32],
    };
    let mut streams = Vec::new();
    for _ in 0..4 {
        let inbox = client.sender().open_stream().unwrap();
        client
            .sender()
            .send(frame(
                FrameKind::DeltaFetchRequest,
                inbox.stream_id(),
                request.encode(),
            ))
            .await
            .unwrap();
        streams.push(inbox);
    }
    // Remote roots retain held-FD authority even after losing their spelling.
    // Subsequent SOURCE capture must not adopt a replacement at that spelling.
    std::fs::rename(&root, parent.path().join("held")).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), b"foreign").unwrap();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(4);
    let (release, gate) = tokio::sync::watch::channel(false);
    let mut workers = tokio::task::JoinSet::new();
    for mut inbox in streams {
        let sender = client.sender();
        let ready_tx = ready_tx.clone();
        let mut gate = gate.clone();
        workers.spawn(async move {
            let id = inbox.stream_id();
            let ready = inbox.recv().await.unwrap().unwrap();
            assert_eq!(ready.frame().kind(), FrameKind::DeltaFetchReady);
            assert!(ready.frame().flags().is_empty() && ready.frame().payload().is_empty());
            drop(ready);
            ready_tx.send(id).await.unwrap();
            gate.wait_for(|released| *released).await.unwrap();
            for index in 0..512 {
                sender
                    .send(frame(
                        FrameKind::Signature,
                        id,
                        WireSignature::new(index, 4096, 0, [0; 16])
                            .unwrap()
                            .encode(),
                    ))
                    .await
                    .unwrap();
            }
            sender
                .send(frame(
                    FrameKind::SignatureEnd,
                    id,
                    WireSignatureEnd::new(512 * 4096, 512, [9; 32])
                        .unwrap()
                        .encode(),
                ))
                .await
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let received = inbox.recv().await.unwrap().unwrap();
                match received.frame().kind() {
                    FrameKind::Data => bytes.extend_from_slice(received.frame().payload()),
                    FrameKind::FileEnd => {
                        let end = WireFileEnd::decode(received.frame().payload()).unwrap();
                        assert_eq!(end.digest(), *blake3::hash(&bytes).as_bytes());
                        assert_eq!(bytes, vec![b's'; 4096]);
                        drop(received);
                        crate::remote::fetch::acknowledge_fetch(&sender, id)
                            .await
                            .unwrap();
                        break;
                    }
                    kind => panic!("wrong phase: {kind:?}"),
                }
            }
        });
    }
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), ready_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let premature = tokio::time::timeout(Duration::from_millis(100), ready_rx.recv()).await;
    release.send(true).unwrap();
    assert!(
        premature.is_err(),
        "readiness must follow byte admission, not merely receipt of an opener"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = workers.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("queued streams must not deadlock admitted uploads/ACKs");
    drop(client);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(root.join("file")).unwrap(), b"foreign");
    assert_eq!(
        std::fs::read(parent.path().join("held/file")).unwrap(),
        vec![b's'; 4096]
    );
}

#[tokio::test]
async fn readiness_is_exact_and_cannot_be_an_ack_or_a_data_phase_message() {
    for case in ["ack", "flags", "payload", "duplicate"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), vec![b'o'; 4096]).unwrap();
        let old = old_entry(root.path());
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let client = FrameRouter::start(cr, cw, RouterRole::Client, Default::default()).unwrap();
        let (mut sr, mut sw) = tokio::io::split(server_io);
        let peer = tokio::spawn(async move {
            let opener = read_frame(&mut sr).await.unwrap();
            let id = opener.stream_id();
            let ready = Frame::new(
                if case == "ack" {
                    FrameKind::Ack
                } else {
                    FrameKind::DeltaFetchReady
                },
                if case == "flags" {
                    FrameFlags::FINAL
                } else {
                    FrameFlags::empty()
                },
                id,
                if case == "payload" {
                    Bytes::from_static(b"invalid")
                } else {
                    Bytes::new()
                },
            )
            .unwrap();
            write_frame(&mut sw, &ready).await.unwrap();
            if case == "duplicate" {
                loop {
                    if read_frame(&mut sr).await.unwrap().kind() == FrameKind::SignatureEnd {
                        break;
                    }
                }
                write_frame(&mut sw, &ready).await.unwrap();
            }
            let cancelled = read_frame(&mut sr).await.unwrap();
            assert_eq!(
                cancelled.kind(),
                FrameKind::Cancel,
                "no signatures before exact readiness; no ACK on failure"
            );
        });
        let caps = crate::endpoint::Capabilities::local();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_delta(
                &client.sender(),
                &old,
                Platform::current().os,
                PullTarget {
                    endpoint: LocalEndpoint::new(root.path().to_path_buf())
                        .with_rooted_authority(rooted),
                    path: old.path.clone(),
                    old: old.clone(),
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
        assert!(result.is_err(), "{case}");
        peer.await.unwrap();
        assert_eq!(old_entry(root.path()).identity, old.identity);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn local_root_loss_at_publication_is_committed_failure_without_ack() {
    for replace in [false, true] {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("destination");
        let held = parent.path().join("held");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), vec![b'o'; 4096]).unwrap();
        let old = old_entry(&root);
        let rooted = RootedFs::open(root.clone()).await.unwrap();
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterAdmission,
                reached: reached_tx,
                resume: resume_rx,
            },
        );
        let endpoint = LocalEndpoint::new(root.clone()).with_rooted_authority(rooted);
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let client = FrameRouter::start(cr, cw, RouterRole::Client, Default::default()).unwrap();
        let (mut sr, mut sw) = tokio::io::split(server_io);
        let peer = tokio::spawn(async move {
            let opener = read_frame(&mut sr).await.unwrap();
            let id = opener.stream_id();
            write_frame(
                &mut sw,
                &frame(FrameKind::DeltaFetchReady, id, Bytes::new()),
            )
            .await
            .unwrap();
            loop {
                if read_frame(&mut sr).await.unwrap().kind() == FrameKind::SignatureEnd {
                    break;
                }
            }
            let bytes = vec![b'n'; 4096];
            write_frame(&mut sw, &frame(FrameKind::Data, id, bytes.clone().into()))
                .await
                .unwrap();
            write_frame(
                &mut sw,
                &Frame::new(
                    FrameKind::FileEnd,
                    FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                    id,
                    WireFileEnd::new(4096, *blake3::hash(&bytes).as_bytes()).encode(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(
                read_frame(&mut sr).await.unwrap().kind(),
                FrameKind::Cancel,
                "committed failure cannot ACK SOURCE"
            );
        });
        let sender = client.sender();
        let fetch = tokio::spawn(async move {
            let caps = crate::endpoint::Capabilities::local();
            fetch_delta(
                &sender,
                &old,
                Platform::current().os,
                PullTarget {
                    endpoint,
                    path: old.path.clone(),
                    old: old.clone(),
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
            )
            .await
        });
        let reached = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
        if matches!(reached, Ok(Ok(()))) {
            std::fs::rename(&root, &held).unwrap();
            if replace {
                std::fs::create_dir(&root).unwrap();
                std::fs::write(root.join("file"), b"foreign").unwrap();
            }
        }
        let _ = resume_tx.send(());
        assert!(
            matches!(reached, Ok(Ok(()))),
            "must pause actual native admission"
        );
        let result = tokio::time::timeout(Duration::from_secs(5), fetch)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                result,
                Err(RemoteTransferError::Local(
                    crate::error::SyncError::CommittedRootChanged { .. }
                ))
            ),
            "{result:?}"
        );
        peer.await.unwrap();
        assert_eq!(
            std::fs::read(held.join("file")).unwrap(),
            vec![b'n'; 4096],
            "publication really committed"
        );
        assert_eq!(std::fs::read_dir(&held).unwrap().count(), 1);
        if replace {
            assert_eq!(std::fs::read(root.join("file")).unwrap(), b"foreign");
        }
    }
}
