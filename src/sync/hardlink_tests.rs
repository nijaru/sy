//! Hardlink semantics through the real direction adapters.
use super::{SyncConfig, SyncStats};
use crate::error::{Result, SyncError};
use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

pub(super) async fn remote_session(
    operation: sy::protocol::Operation,
    root: &std::path::Path,
) -> (
    sy::remote::runtime::ClientRemoteSession,
    tokio::task::JoinHandle<sy::remote::serve::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let server = tokio::spawn(sy::remote::serve::serve_transport(
        reader,
        writer,
        Default::default(),
    ));
    let (reader, writer) = tokio::io::split(client);
    let client = sy::remote::runtime::ClientRemoteSession::connect(
        reader,
        writer,
        operation,
        root,
        Default::default(),
    )
    .await
    .unwrap();
    (client, server)
}

pub(super) async fn finish_session(
    client: sy::remote::runtime::ClientRemoteSession,
    server: tokio::task::JoinHandle<sy::remote::serve::Result<()>>,
) {
    drop(client);
    let result = server.await.unwrap();
    if let Err(error) = result {
        assert!(
            matches!(&error, sy::remote::serve::ServeError::Session(
            sy::remote::runtime::RemoteSessionError::Router(error)
        ) if matches!(error.as_ref(), sy::remote::router::RouterError::TransportEof)),
            "{error:?}"
        );
    }
}

pub(super) async fn assert_destination_alias_deletion<F, Fut>(mut execute: F)
where
    F: FnMut(PathBuf, PathBuf, SyncConfig) -> Fut,
    Fut: Future<Output = Result<SyncStats>>,
{
    for backup in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(destination.path().join("a"), b"old bytes").unwrap();
        for name in ["b", "c"] {
            std::fs::hard_link(destination.path().join("a"), destination.path().join(name))
                .unwrap();
        }
        let mut config = SyncConfig::test_default();
        config.delete = super::DeleteMode::Enabled {
            limit: crate::engine::delete_plan::DeleteLimit::Count(3),
            force: false,
        };
        if backup {
            config.backup = Some(String::new());
        }
        let stats = execute(source.path().into(), destination.path().into(), config)
            .await
            .unwrap();
        assert_eq!(stats.files_deleted, 3);
        for name in ["a", "b", "c"] {
            assert!(
                !destination.path().join(name).exists(),
                "alias {name} survived deletion"
            );
            if backup {
                assert_eq!(
                    std::fs::read(destination.path().join(format!("{name}~"))).unwrap(),
                    b"old bytes"
                );
            }
        }
        assert_eq!(
            std::fs::read_dir(destination.path()).unwrap().count(),
            if backup { 3 } else { 0 },
            "unexpected entries or private staging leaked"
        );
        assert!(std::fs::read_dir(source.path()).unwrap().next().is_none());
    }
}

pub(super) async fn assert_byte_commitments<F, Fut>(mut execute: F)
where
    F: FnMut(PathBuf, PathBuf, SyncConfig) -> Fut,
    Fut: Future<Output = Result<SyncStats>>,
{
    // Metadata/Unchanged, both mixed-intent orders, preview, explicit source
    // updates, and a selection leaving only one member of an excluded group.
    for (first_payload, second_payload, permissions, preview, checksum, exclude_second) in [
        (&b"other1"[..], &b"other2"[..], true, false, false, false),
        (&b"old"[..], &b"other2"[..], false, false, false, false),
        (&b"other1"[..], &b"old"[..], false, false, false, false),
        (&b"other1"[..], &b"other2"[..], false, true, false, false),
        (&b"other1"[..], &b"other2"[..], false, false, true, false),
        (&b"other1"[..], &b"other2"[..], false, false, false, true),
        (&b"target"[..], &b"target"[..], false, false, false, false),
    ] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("aaa-new"), b"unrelated").unwrap();
        std::fs::write(source.path().join("first"), b"source").unwrap();
        std::fs::hard_link(source.path().join("first"), source.path().join("second")).unwrap();
        std::fs::write(destination.path().join("first"), first_payload).unwrap();
        std::fs::write(destination.path().join("second"), second_payload).unwrap();
        for root in [source.path(), destination.path()] {
            for name in ["first", "second"] {
                let path = root.join(name);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
                filetime::set_file_mtime(
                    &path,
                    filetime::FileTime::from_unix_time(1_600_000_000, 123),
                )
                .unwrap();
            }
        }
        if permissions {
            std::fs::set_permissions(
                destination.path().join("first"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let before = std::fs::metadata(destination.path().join("first")).unwrap();
        let mut config = SyncConfig::test_default();
        config.preserve.hardlinks = true;
        config.preserve.permissions = permissions;
        config.dry_run = preview;
        config.comparison.checksum = checksum;
        if exclude_second {
            config.filter_engine.add_exclude("second").unwrap();
        }
        let result = execute(source.path().into(), destination.path().into(), config).await;
        let conflict = !checksum && !exclude_second && first_payload != second_payload;
        if conflict {
            let error: SyncError = result.unwrap_err();
            assert!(
                error.to_string().contains("incompatible byte commitments"),
                "{error:?}"
            );
            assert!(
                !destination.path().join("aaa-new").exists(),
                "preflight allowed an earlier unrelated write"
            );
            assert_eq!(
                std::fs::read(destination.path().join("first")).unwrap(),
                first_payload
            );
            assert_eq!(
                std::fs::read(destination.path().join("second")).unwrap(),
                second_payload
            );
            let after = std::fs::metadata(destination.path().join("first")).unwrap();
            assert_eq!(
                (
                    before.ino(),
                    before.mode(),
                    before.mtime(),
                    before.mtime_nsec(),
                    before.ctime(),
                    before.ctime_nsec(),
                    before.nlink()
                ),
                (
                    after.ino(),
                    after.mode(),
                    after.mtime(),
                    after.mtime_nsec(),
                    after.ctime(),
                    after.ctime_nsec(),
                    after.nlink()
                )
            );
        } else {
            result.unwrap();
            let expected_first: &[u8] = if checksum { b"source" } else { first_payload };
            let expected_second: &[u8] = if checksum { b"source" } else { second_payload };
            assert_eq!(
                std::fs::read(destination.path().join("first")).unwrap(),
                expected_first
            );
            assert_eq!(
                std::fs::read(destination.path().join("second")).unwrap(),
                expected_second
            );
            if checksum {
                assert_eq!(
                    std::fs::metadata(destination.path().join("first"))
                        .unwrap()
                        .ino(),
                    std::fs::metadata(destination.path().join("second"))
                        .unwrap()
                        .ino()
                );
            }
        }
        assert_eq!(
            std::fs::read(source.path().join("first")).unwrap(),
            b"source"
        );
        assert_eq!(
            std::fs::read(source.path().join("second")).unwrap(),
            b"source"
        );
    }
}
