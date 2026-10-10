use super::*;
use crate::remote::operand::RemoteOperand;
use crate::remote::router::RouterConfig;
use crate::rooted_fs::operand::SourceShape;
use futures::TryStreamExt;

#[tokio::test]
async fn acquisition_is_one_shared_acknowledged_epoch_for_all_handles() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("missing/parent");
        let selected = RelativePath::new("renamed").unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(crate::remote::serve::serve_transport(
            server_reader,
            server_writer,
            RouterConfig::default(),
        ));
        let mut client = ClientRemoteSession::connect_operand(
            reader,
            writer,
            Operation::Push,
            &root.join(selected.as_path()),
            &RemoteOperand::Destination {
                source: SourceShape::Leaf {
                    name: RelativePath::new("original").unwrap(),
                },
            },
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let first = client.request_handle();
        let second = first.clone();
        let independently_obtained = client.request_handle();
        assert!(first.binding().pending);
        let (a, b) = tokio::join!(
            first.acquire_root_for_create(&selected),
            second.acquire_root_for_create(&selected),
        );
        a.unwrap();
        b.unwrap();
        let newly_obtained = client.request_handle();
        let actual = crate::rooted_fs::RootedFs::open(root.clone())
            .await
            .unwrap();
        let namespace = actual.namespace_semantics().await.unwrap();
        assert_eq!(client.namespace_semantics(), namespace);
        for handle in [&first, &second, &independently_obtained, &newly_obtained] {
            assert!(!handle.binding().pending);
            assert_eq!(handle.namespace_semantics(), namespace);
            handle.acquire_root_for_create(&selected).await.unwrap();
        }
        newly_obtained
            .replace_symlink(&selected, Path::new("opaque-target"), None, None)
            .await
            .unwrap();
        let entries: Vec<_> = independently_obtained
            .scan_entry(Default::default(), selected)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            std::fs::read_link(root.join("renamed")).unwrap(),
            Path::new("opaque-target")
        );
        client.finish().await.unwrap();
        server.await.unwrap().unwrap();
    })
    .await
    .expect("shared acquisition did not drain");
}

#[tokio::test]
async fn interrupted_acquisition_fails_the_session_instead_of_retrying_an_unknown_epoch() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("missing/parent");
        let selected = RelativePath::new("renamed").unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let (received, request_received) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut session = crate::remote::runtime::ServerRemoteSession::accept(
                server_reader,
                server_writer,
                RouterConfig::default(),
            )
            .await
            .unwrap();
            let Some(crate::remote::runtime::IncomingRequest::AcquireRoot(incoming)) =
                session.next_request().await.unwrap()
            else {
                panic!("missing acquisition request")
            };
            received.send(()).unwrap();
            resumed.await.unwrap();
            // Cancellation can leave completion uncertain. This deliberately
            // holds the request before native admission; shutdown must close
            // that admission too, not allow a second acquisition attempt.
            let result = session.acquire_root(incoming).await;
            let _ = session.shutdown().await;
            result
        });
        let mut client = ClientRemoteSession::connect_operand(
            reader,
            writer,
            Operation::Push,
            &root.join(selected.as_path()),
            &RemoteOperand::Destination {
                source: SourceShape::Leaf {
                    name: RelativePath::new("original").unwrap(),
                },
            },
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let handle = client.request_handle();
        let attempt_handle = handle.clone();
        let attempt_name = selected.clone();
        let attempt =
            tokio::spawn(
                async move { attempt_handle.acquire_root_for_create(&attempt_name).await },
            );
        request_received.await.unwrap();
        attempt.abort();
        assert!(attempt.await.unwrap_err().is_cancelled());
        assert!(handle.acquire_root_for_create(&selected).await.is_err());
        assert!(handle.binding().pending);
        let _ = client.shutdown().await;
        resume.send(()).unwrap();
        assert!(server.await.unwrap().is_err());
        assert!(!root.parent().unwrap().exists());
    })
    .await
    .expect("interrupted acquisition did not drain");
}
