use super::*;
use crate::protocol::{Operation, WireSourceIgnoreRequest};
use crate::remote::runtime::ClientRemoteSession;

fn path(name: &str) -> RelativePath {
    RelativePath::new(name).unwrap()
}

async fn connect(
    root: &std::path::Path,
    operation: Operation,
) -> (
    ClientRemoteSession,
    tokio::task::JoinHandle<crate::remote::serve::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (read, write) = tokio::io::split(server);
    let server = tokio::spawn(crate::remote::serve::serve_transport(
        read,
        write,
        Default::default(),
    ));
    let (read, write) = tokio::io::split(client);
    let operand = match operation {
        Operation::Pull => crate::remote::operand::RemoteOperand::Source { contents: false },
        _ => crate::remote::operand::RemoteOperand::Destination {
            source: crate::rooted_fs::operand::SourceShape::Leaf {
                name: path("selected"),
            },
        },
    };
    let client = ClientRemoteSession::connect_operand(
        read,
        write,
        operation,
        root,
        &operand,
        Default::default(),
    )
    .await
    .unwrap();
    (client, server)
}

#[tokio::test]
async fn source_policy_reads_original_root_after_relocation_and_replacement() {
    let fixture = tempfile::tempdir().unwrap();
    let original = fixture.path().join("root");
    let moved = fixture.path().join("moved");
    std::fs::create_dir(&original).unwrap();
    std::fs::write(original.join(".ignore"), b"kept\n").unwrap();
    let (client, server) = connect(&original, Operation::Pull).await;
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    std::fs::write(original.join(".ignore"), b"other\n").unwrap();
    let handle = client.request_handle();
    assert!(handle
        .source_ignored(&path("kept"), false, false)
        .await
        .unwrap());
    assert!(!handle
        .source_ignored(&path("other"), false, false)
        .await
        .unwrap());
    drop(handle);
    drop(client);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn policy_queries_are_pull_only_and_scoped_to_the_selected_source_name() {
    let fixture = tempfile::tempdir().unwrap();
    let selected = fixture.path().join("selected");
    std::fs::write(&selected, b"source").unwrap();
    for operation in [Operation::Pull, Operation::Push] {
        let (client, server) = connect(&selected, operation).await;
        let mut inbox = client.sender().open_stream().unwrap();
        client
            .sender()
            .send(
                Frame::new(
                    FrameKind::SourceIgnoreRequest,
                    FrameFlags::FINAL,
                    inbox.stream_id(),
                    WireSourceIgnoreRequest {
                        path: encode_relative_path(path("sibling").as_path()).unwrap(),
                        is_directory: false,
                        respect_gitignore: false,
                    }
                    .encode()
                    .unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(inbox.recv().await, Err(_) | Ok(None)));
        drop(inbox);
        drop(client);
        assert!(server.await.unwrap().is_err());
    }
}

#[tokio::test]
async fn trusted_configuration_leaf_symlinks_retain_source_policy() {
    let source = tempfile::tempdir().unwrap();
    let policy = tempfile::tempdir().unwrap();
    std::fs::write(policy.path().join("rules"), b"kept\n").unwrap();
    std::os::unix::fs::symlink(policy.path().join("rules"), source.path().join(".ignore")).unwrap();
    let (client, server) = connect(source.path(), Operation::Pull).await;
    assert!(client
        .request_handle()
        .source_ignored(&path("kept"), false, false)
        .await
        .unwrap());
    drop(client);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn query_ancestry_cannot_follow_a_source_symlink_outside_the_root() {
    let source = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join(".ignore"), b"secret\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), source.path().join("escape")).unwrap();
    let (client, server) = connect(source.path(), Operation::Pull).await;
    assert!(client
        .request_handle()
        .source_ignored(&path("escape/secret"), false, false)
        .await
        .is_err());
    drop(client);
    assert!(server.await.unwrap().is_err());
    assert_eq!(
        std::fs::read(outside.path().join(".ignore")).unwrap(),
        b"secret\n"
    );
}

#[tokio::test]
async fn source_policy_handler_rejects_wrong_request_flags() {
    let fixture = tempfile::tempdir().unwrap();
    let (client, server) = connect(fixture.path(), Operation::Pull).await;
    let mut inbox = client.sender().open_stream().unwrap();
    client
        .sender()
        .send(
            Frame::new(
                FrameKind::SourceIgnoreRequest,
                FrameFlags::FINAL | FrameFlags::COMPRESSED,
                inbox.stream_id(),
                WireSourceIgnoreRequest {
                    path: encode_relative_path(path("entry").as_path()).unwrap(),
                    is_directory: false,
                    respect_gitignore: false,
                }
                .encode()
                .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(inbox.recv().await, Err(_) | Ok(None)));
    drop(inbox);
    drop(client);
    assert!(server.await.unwrap().is_err());
}
