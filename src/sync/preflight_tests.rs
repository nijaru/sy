//! Real shared direction entrypoints with a deliberately less capable peer.
//! Only negotiated capability advertisement is changed; all data-plane and
//! filesystem handlers remain real. No SSH process or network is involved.
use super::{scanner::ScanOptions, SyncConfig};
use crate::protocol::{
    read_frame, write_frame, CapabilitySet, Frame, FrameKind, Operation, SessionReady,
};
use crate::remote::runtime::ClientRemoteSession;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use tokio::io::AsyncWriteExt;

fn observation(path: &Path) -> (u64, u32, i64, i64, i64, i64) {
    let metadata = std::fs::metadata(path).unwrap();
    (
        metadata.ino(),
        metadata.mode(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

async fn limited_peer(
    operation: Operation,
    root: &Path,
    unavailable: CapabilitySet,
) -> (
    ClientRemoteSession,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let (client_io, relay_client) = tokio::io::duplex(64 * 1024);
    let (relay_server, server_io) = tokio::io::duplex(64 * 1024);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    let server = tokio::spawn(async move {
        crate::remote::serve::serve_transport(server_reader, server_writer, Default::default())
            .await
            .unwrap();
    });
    let relay = tokio::spawn(async move {
        let (mut client_reader, mut client_writer) = tokio::io::split(relay_client);
        let (mut server_reader, mut server_writer) = tokio::io::split(relay_server);
        let requests = async {
            tokio::io::copy(&mut client_reader, &mut server_writer)
                .await
                .unwrap();
            server_writer.shutdown().await.unwrap();
        };
        let responses = async {
            let hello = read_frame(&mut server_reader).await.unwrap();
            assert_eq!(hello.kind(), FrameKind::ServerHello);
            write_frame(&mut client_writer, &hello).await.unwrap();
            let frame = read_frame(&mut server_reader).await.unwrap();
            assert_eq!(frame.kind(), FrameKind::SessionReady);
            let mut ready = SessionReady::decode(frame.payload()).unwrap();
            ready.capabilities.remove(unavailable);
            let frame = Frame::control(FrameKind::SessionReady, ready.encode().unwrap()).unwrap();
            write_frame(&mut client_writer, &frame).await.unwrap();
            tokio::io::copy(&mut server_reader, &mut client_writer)
                .await
                .unwrap();
            client_writer.shutdown().await.unwrap();
        };
        tokio::join!(requests, responses);
    });
    let (reader, writer) = tokio::io::split(client_io);
    let client = ClientRemoteSession::connect(reader, writer, operation, root, Default::default())
        .await
        .unwrap();
    (client, server, relay)
}

#[tokio::test]
async fn remote_preservation_admission_precedes_directory_root_and_backup_effects() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for pull in [false, true] {
            for pending in [false, true] {
                for (unavailable, feature) in [
                    (CapabilitySet::ACL, "ACLs"),
                    (CapabilitySet::XATTR, "xattrs"),
                    (CapabilitySet::HARDLINK, "hardlinks"),
                    (CapabilitySet::BSD_FLAGS, "BSD flags"),
                ] {
                    let fixture = tempfile::tempdir().unwrap();
                    let source = fixture.path().join("source");
                    let missing = fixture.path().join("missing");
                    let destination = if pending {
                        missing.join("parents/destination")
                    } else {
                        fixture.path().join("destination")
                    };
                    std::fs::create_dir(&source).unwrap();
                    // Directory preparation precedes leaf work in the actual
                    // controller; the update also exercises PUSH backup risk.
                    std::fs::create_dir(source.join("a-directory")).unwrap();
                    std::fs::write(source.join("z-file"), b"replacement content").unwrap();
                    if !pending {
                        std::fs::create_dir(&destination).unwrap();
                        std::fs::write(destination.join("z-file"), b"OLD").unwrap();
                    }
                    let before = (!pending).then(|| {
                        (
                            observation(&destination),
                            observation(&destination.join("z-file")),
                        )
                    });
                    let mut config = SyncConfig::test_default();
                    config.backup = Some("simple".into());
                    // ACL refusal includes an independently supported request:
                    // support for xattrs must not waive unsupported ACLs.
                    config.preserve.xattrs =
                        unavailable == CapabilitySet::XATTR || unavailable == CapabilitySet::ACL;
                    config.preserve.acls = unavailable == CapabilitySet::ACL;
                    config.preserve.hardlinks = unavailable == CapabilitySet::HARDLINK;
                    config.preserve.flags = unavailable == CapabilitySet::BSD_FLAGS;
                    let (mut client, server, relay) = limited_peer(
                        if pull {
                            Operation::Pull
                        } else {
                            Operation::Push
                        },
                        if pull { &source } else { &destination },
                        unavailable,
                    )
                    .await;
                    let result = if pull {
                        super::pull::execute_with_handle(
                            &source,
                            &destination,
                            client.request_handle(),
                            client.sender(),
                            &config,
                            ScanOptions::default(),
                        )
                        .await
                    } else {
                        super::push::execute_with_handle(
                            &source,
                            &destination,
                            client.request_handle(),
                            &config,
                            ScanOptions::default(),
                        )
                        .await
                    };
                    // Refusal occurs with no producer or mutation stream left
                    // running; ordinary session finish must still drain.
                    client.finish().await.unwrap();
                    server.await.unwrap();
                    relay.await.unwrap();
                    let error = result.unwrap_err();
                    assert!(
                        error.to_string().contains(feature),
                        "pull={pull} pending={pending}: {error}"
                    );
                    if pending {
                        assert!(
                            !missing.exists(),
                            "{feature}: acquired missing destination parents"
                        );
                    } else {
                        assert_eq!(
                            Some((
                                observation(&destination),
                                observation(&destination.join("z-file")),
                            )),
                            before,
                            "{feature}: destination metadata changed"
                        );
                        assert_eq!(std::fs::read(destination.join("z-file")).unwrap(), b"OLD");
                        assert_eq!(
                            std::fs::read_dir(&destination).unwrap().count(),
                            1,
                            "{feature}: directory or backup published before refusal"
                        );
                    }
                    assert_eq!(
                        std::fs::read(source.join("z-file")).unwrap(),
                        b"replacement content"
                    );
                }
            }
        }
    })
    .await
    .expect("preservation admission did not drain");
}
