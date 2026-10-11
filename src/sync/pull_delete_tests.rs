//! Pull deletion exercises the real remote policy handler and common controller.
use super::*;
use sy::engine::delete_plan::DeleteLimit;
use sy::protocol::Operation;
use sy::remote::runtime::ClientRemoteSession;
use tempfile::TempDir;

async fn session(
    root: &Path,
) -> (
    ClientRemoteSession,
    tokio::task::JoinHandle<sy::remote::serve::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (read, write) = tokio::io::split(server);
    let server = tokio::spawn(sy::remote::serve::serve_transport(
        read,
        write,
        Default::default(),
    ));
    let (read, write) = tokio::io::split(client);
    let client =
        ClientRemoteSession::connect(read, write, Operation::Pull, root, Default::default())
            .await
            .unwrap();
    (client, server)
}

fn config(limit: DeleteLimit) -> SyncConfig {
    let mut config = SyncConfig::test_default();
    config.quiet = true;
    config.max_concurrent = 2;
    config.delete = crate::sync::DeleteMode::Enabled {
        limit,
        force: false,
    };
    config
}

#[tokio::test]
async fn pull_delete_protects_remote_rules_vcs_and_ancestors() {
    let source = TempDir::new().unwrap();
    let destination = TempDir::new().unwrap();
    std::fs::create_dir(source.path().join(".git")).unwrap();
    std::fs::write(
        source.path().join(".ignore"),
        b"parent/kept.private\nignored/\n",
    )
    .unwrap();
    std::fs::write(source.path().join(".gitignore"), b"*.git-only\n").unwrap();
    std::fs::write(source.path().join("new"), b"source bytes").unwrap();
    std::fs::create_dir_all(source.path().join("parent")).unwrap();
    std::fs::create_dir_all(source.path().join("ignored")).unwrap();
    std::fs::write(
        source.path().join("parent/kept.private"),
        b"do not transfer",
    )
    .unwrap();
    std::fs::write(source.path().join("ignored/kept"), b"do not transfer").unwrap();
    std::fs::write(source.path().join("source.git-only"), b"do not create").unwrap();
    for directory in ["parent", "ignored", ".git/objects"] {
        std::fs::create_dir_all(destination.path().join(directory)).unwrap();
    }
    for name in [
        "parent/kept.private",
        "ignored/kept",
        ".git/objects/kept",
        "kept.git-only",
        "parent/gone",
        "stale",
    ] {
        std::fs::write(destination.path().join(name), name.as_bytes()).unwrap();
    }
    let (client, server) = session(source.path()).await;
    // Exactly two eligible candidates. Protected entries and their ancestors
    // must not inflate the limit or be authorized by the reverse delete replay.
    let result = execute_with_handle(
        source.path(),
        destination.path(),
        client.request_handle(),
        client.sender(),
        &config(DeleteLimit::Count(2)),
        ScanOptions {
            respect_gitignore: true,
            include_git_dir: false,
            ..Default::default()
        },
    )
    .await;
    drop(client);
    let served = server.await.unwrap();
    let stats =
        result.unwrap_or_else(|error| panic!("pull failed: {error:#?}; server: {served:?}"));
    served.unwrap();
    assert_eq!(stats.files_deleted, 2);
    assert_eq!(
        std::fs::read(destination.path().join("new")).unwrap(),
        b"source bytes"
    );
    for name in [
        "parent/kept.private",
        "ignored/kept",
        ".git/objects/kept",
        "kept.git-only",
    ] {
        assert_eq!(
            std::fs::read(destination.path().join(name)).unwrap(),
            name.as_bytes()
        );
    }
    for name in ["parent/gone", "stale", "source.git-only"] {
        assert!(!destination.path().join(name).exists());
    }
}

#[tokio::test]
async fn pull_delete_threshold_fails_before_copy_or_delete() {
    let source = TempDir::new().unwrap();
    let destination = TempDir::new().unwrap();
    std::fs::write(source.path().join("new"), b"source").unwrap();
    std::fs::write(destination.path().join("stale"), b"retain").unwrap();
    let (client, server) = session(source.path()).await;
    let result = execute_with_handle(
        source.path(),
        destination.path(),
        client.request_handle(),
        client.sender(),
        &config(DeleteLimit::Count(0)),
        ScanOptions::default(),
    )
    .await;
    drop(client);
    server.await.unwrap().unwrap();
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(destination.path().join("stale")).unwrap(),
        b"retain"
    );
    assert!(!destination.path().join("new").exists());
}

#[tokio::test]
async fn pull_delete_missing_policy_reply_prevents_every_effect() {
    use sy::remote::runtime::{IncomingRequest, ServerRemoteSession};
    let source = TempDir::new().unwrap();
    let destination = TempDir::new().unwrap();
    std::fs::write(source.path().join("new"), b"source").unwrap();
    std::fs::write(destination.path().join("stale"), b"retain").unwrap();
    let (client, peer) = tokio::io::duplex(64 * 1024);
    let (read, write) = tokio::io::split(peer);
    let server = tokio::spawn(async move {
        let mut session = ServerRemoteSession::accept(read, write, Default::default())
            .await
            .unwrap();
        let scan = session.scan_handler();
        // The scan is complete, but that cannot stand in for deletion scope.
        let IncomingRequest::Scan(incoming) = session.next_request().await.unwrap().unwrap() else {
            panic!("expected scan");
        };
        scan.serve(incoming).await.unwrap();
        let IncomingRequest::SourceIgnore(incoming) =
            session.next_request().await.unwrap().unwrap()
        else {
            panic!("expected original source policy query before mutation");
        };
        drop(incoming);
        drop(session);
    });
    let (read, write) = tokio::io::split(client);
    let client = ClientRemoteSession::connect(
        read,
        write,
        Operation::Pull,
        source.path(),
        Default::default(),
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        execute_with_handle(
            source.path(),
            destination.path(),
            client.request_handle(),
            client.sender(),
            &config(DeleteLimit::Unlimited),
            ScanOptions::default(),
        ),
    )
    .await
    .unwrap();
    drop(client);
    server.await.unwrap();
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(destination.path().join("stale")).unwrap(),
        b"retain"
    );
    assert!(!destination.path().join("new").exists());
}

#[tokio::test]
async fn pull_delete_policy_read_failure_prevents_unrelated_copy_and_delete() {
    let source = TempDir::new().unwrap();
    let destination = TempDir::new().unwrap();
    std::fs::create_dir_all(source.path().join("sub/.git/info")).unwrap();
    std::fs::write(
        source.path().join("sub/.git/info/exclude"),
        vec![b'x'; 256 * 1024 + 1],
    )
    .unwrap();
    std::fs::write(source.path().join("new"), b"source").unwrap();
    std::fs::create_dir(destination.path().join("sub")).unwrap();
    for name in ["sub/stale", "stale"] {
        std::fs::write(destination.path().join(name), b"retain").unwrap();
    }
    let (client, server) = session(source.path()).await;
    let result = execute_with_handle(
        source.path(),
        destination.path(),
        client.request_handle(),
        client.sender(),
        &config(DeleteLimit::Unlimited),
        ScanOptions {
            respect_gitignore: true,
            include_git_dir: false,
            ..Default::default()
        },
    )
    .await;
    drop(client);
    assert!(server.await.unwrap().is_err());
    assert!(result.is_err());
    assert!(!destination.path().join("new").exists());
    for name in ["sub/stale", "stale"] {
        assert_eq!(
            std::fs::read(destination.path().join(name)).unwrap(),
            b"retain"
        );
    }
}
