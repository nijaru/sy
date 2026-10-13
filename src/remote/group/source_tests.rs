use super::*;
use crate::protocol::{Platform, RelativeWirePath, WireFileBasis, WireFileBegin};
use crate::remote::group::declarations::add_to_seal;
use crate::remote::router::{FrameRouter, RouterConfig, RouterRole, StreamInbox};
use std::path::Path;

fn path(name: &str) -> RelativeWirePath {
    crate::remote::path::encode_relative_path(Path::new(name)).unwrap()
}
async fn send(sender: &RouterSender, inbox: &StreamInbox, kind: FrameKind, payload: Bytes) {
    sender
        .send(Frame::new(kind, FrameFlags::empty(), inbox.stream_id(), payload).unwrap())
        .await
        .unwrap();
}
async fn reply(inbox: &mut StreamInbox) -> WireGroupResult {
    let routed = inbox.recv().await.unwrap().unwrap();
    assert_eq!(routed.frame().kind(), FrameKind::GroupResult);
    WireGroupResult::decode(routed.frame().payload()).unwrap()
}

#[tokio::test]
async fn lease_keeps_source_authority_through_each_member_and_exact_completion() {
    for outcome in [
        "complete",
        "changed-parent",
        "original-parent-alias",
        "premature-completion",
    ] {
        let change_second_parent = outcome == "changed-parent";
        let replace_original_parent = outcome == "original-parent-alias";
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("owner")).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("owner/first"), b"original").unwrap();
        std::fs::hard_link(
            root.path().join("owner/first"),
            root.path().join("dir/second"),
        )
        .unwrap();
        if replace_original_parent {
            std::fs::create_dir(root.path().join("alternate")).unwrap();
            std::fs::hard_link(
                root.path().join("owner/first"),
                root.path().join("alternate/first"),
            )
            .unwrap();
        }
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        crate::rooted_fs::test_source::scan(&rooted).await;
        let rooted_for_worker = rooted.clone();
        let identity = tokio::task::spawn_blocking(move || {
            rooted_for_worker
                .path_identity_blocking(
                    &decode_relative_path(path("owner/first"), Platform::current().os).unwrap(),
                )
                .unwrap()
                .unwrap()
                .1
        })
        .await
        .unwrap();
        let members = [
            WireGroupMember {
                ordinal: 0,
                path: path("owner/first"),
                observation: Some((WireEntryKind::File, *identity.as_bytes())),
            },
            WireGroupMember {
                ordinal: 1,
                path: path("dir/second"),
                observation: Some((WireEntryKind::File, *identity.as_bytes())),
            },
        ];
        let mut digest = blake3::Hasher::new();
        for member in &members {
            add_to_seal(&mut digest, member).unwrap();
        }
        let seal = WireGroupSeal {
            count: 2,
            digest: *digest.finalize().as_bytes(),
        };
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let client =
            FrameRouter::start(cr, cw, RouterRole::Client, RouterConfig::default()).unwrap();
        // The transport outlives one handler, as in the real session. A queued
        // terminal acknowledgement must survive until the client receives it.
        let mut peer =
            FrameRouter::start(sr, sw, RouterRole::Server, RouterConfig::default()).unwrap();
        let sender = client.sender();
        let mut inbox = sender.open_stream().unwrap();
        let open = WireGroupOpen {
            role: WireGroupRole::Source,
            source_bytes: false,
            retain_metadata: false,
            xattrs: false,
            acls: false,
            source_flags: false,
            verify_bytes: false,
            compression: WireFetchCompression::None,
            file: WireFileBegin::delta(
                path("owner/first"),
                8,
                WireFileBasis::new(8, *identity.as_bytes()),
            ),
        };
        send(&sender, &inbox, FrameKind::GroupOpen, open.encode()).await;
        let incoming = peer.incoming().recv().await.unwrap().unwrap();
        let peer_sender = peer.sender();
        let server = tokio::spawn(async move {
            serve(
                rooted,
                incoming,
                &peer_sender,
                Platform::current().os,
                ResolvedOperand::Tree,
            )
            .await
        });
        for member in &members {
            send(&sender, &inbox, FrameKind::GroupMember, member.encode()).await;
        }
        send(&sender, &inbox, FrameKind::GroupMembersEnd, seal.encode()).await;
        assert_eq!(reply(&mut inbox).await, WireGroupResult::Declarations(seal));
        send(
            &sender,
            &inbox,
            FrameKind::GroupStep,
            WireGroupStep::Prepare.encode(),
        )
        .await;
        assert_eq!(
            reply(&mut inbox).await,
            WireGroupResult::SourceReady { flags: None }
        );
        assert!(!server.is_finished());
        send(
            &sender,
            &inbox,
            FrameKind::GroupStep,
            WireGroupStep::Validate(0).encode(),
        )
        .await;
        assert_eq!(reply(&mut inbox).await, WireGroupResult::Validated(0));
        if outcome == "premature-completion" {
            send(
                &sender,
                &inbox,
                FrameKind::GroupStep,
                WireGroupStep::Finish(seal).encode(),
            )
            .await;
            assert!(matches!(server.await.unwrap(), Err(GroupError::Phase(_))));
            assert_eq!(
                std::fs::read(root.path().join("owner/first")).unwrap(),
                b"original"
            );
            continue;
        }
        if change_second_parent {
            std::fs::rename(root.path().join("dir"), root.path().join("held-dir")).unwrap();
            std::fs::create_dir(root.path().join("dir")).unwrap();
            std::fs::write(root.path().join("dir/second"), b"original").unwrap();
        }
        if replace_original_parent {
            std::fs::rename(root.path().join("owner"), root.path().join("held-owner")).unwrap();
            std::fs::rename(root.path().join("alternate"), root.path().join("owner")).unwrap();
            assert_eq!(
                crate::endpoint::local_identity::metadata_identity(
                    &std::fs::metadata(root.path().join("owner/first")).unwrap(),
                    crate::engine::domain::EntryKind::File,
                ),
                Some(identity),
                "directory renames leave the scanned file observation unchanged"
            );
        }
        send(
            &sender,
            &inbox,
            FrameKind::GroupStep,
            WireGroupStep::Validate(1).encode(),
        )
        .await;
        if change_second_parent || replace_original_parent {
            assert!(matches!(server.await.unwrap(), Err(GroupError::Source(_))));
            let retained = if replace_original_parent {
                "held-owner/first"
            } else {
                "held-dir/second"
            };
            assert_eq!(
                std::fs::read(root.path().join(retained)).unwrap(),
                b"original"
            );
        } else {
            assert_eq!(reply(&mut inbox).await, WireGroupResult::Validated(1));
            assert!(!server.is_finished());
            send(
                &sender,
                &inbox,
                FrameKind::GroupStep,
                WireGroupStep::Finish(seal).encode(),
            )
            .await;
            assert_eq!(
                reply(&mut inbox).await,
                WireGroupResult::SourceComplete(seal)
            );
            server.await.unwrap().unwrap();
        }
        assert_eq!(
            std::fs::read(root.path().join("owner/first")).unwrap(),
            b"original"
        );
    }
}
