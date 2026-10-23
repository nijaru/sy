use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::protocol::{Frame, FrameFlags, FrameKind, Operation, WireMetadata};
use crate::remote::router::{FrameRouter, RouterRole};
use crate::remote::runtime::ClientRemoteSession;
use crate::rooted_fs::operand::{BoundSource, SourceShape};
use crate::rooted_fs::RootedFs;
use bytes::Bytes;
use futures::TryStreamExt;
use std::io::Read;
use std::os::macos::fs::MetadataExt as MacMetadataExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[tokio::test]
async fn followed_file_metadata_uses_readless_or_held_source_through_ack() {
    for (extended, late_change) in [(false, false), (true, false), (true, true)] {
        let source_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        let selected = source_root.path().join("selected");
        let output = destination.path().join("selected");
        std::fs::write(&target, b"SOURCE").unwrap();
        std::fs::write(&output, b"OLD!!!").unwrap();
        let mut old_reader = std::fs::File::open(&output).unwrap();
        let mode = if extended { 0o600 } else { 0o000 };
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o640)).unwrap();
        let modified = filetime::FileTime::from_unix_time(123, 456_000_000);
        filetime::set_file_mtime(&target, modified).unwrap();
        filetime::set_file_mtime(&output, modified).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
        let acl = if extended {
            xattr::set(&target, "user.sy-source-metadata", b"target").unwrap();
            xattr::set(&output, "user.sy-stale", b"remove").unwrap();
            #[cfg(feature = "acl")]
            {
                let mut acl = exacl::getfacl(&target, None).unwrap();
                // SAFETY: getuid has no arguments or memory preconditions.
                let uid = unsafe { libc::getuid() };
                acl.push(exacl::AclEntry::allow_user(
                    &uid.to_string(),
                    exacl::Perm::READ,
                    exacl::Flag::empty(),
                ));
                exacl::setfacl(&[&target], &acl, None).unwrap();
            }
            let path = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: path is live and NUL terminated; UF_HIDDEN permits cleanup.
            assert_eq!(unsafe { libc::chflags(path.as_ptr(), libc::UF_HIDDEN) }, 0);
            #[cfg(feature = "acl")]
            let acl = Some(exacl::to_string(&exacl::getfacl(&target, None).unwrap()).unwrap());
            #[cfg(not(feature = "acl"))]
            let acl: Option<String> = None;
            acl
        } else {
            None
        };
        std::os::unix::fs::symlink(&target, &selected).unwrap();
        let stamp = |path: &Path| {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            (
                metadata.dev(),
                metadata.ino(),
                metadata.nlink(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        };
        let originals = [stamp(&selected), stamp(&target)];
        let bound = BoundSource::open_local_following(selected.clone(), false)
            .await
            .unwrap();
        let SourceShape::Leaf { name } = bound.shape else {
            panic!("not selected")
        };
        let authority = SourceRoot::from_rooted(bound.rooted).unwrap();
        let source = authority
            .selected_entries(
                name,
                crate::engine::scan::ScanRequest {
                    metadata: crate::engine::scan::EntryMetadataRequest {
                        identity: true,
                        unix_mode: true,
                        ..Default::default()
                    },
                    follow_symlinks: true,
                    ..Default::default()
                },
                false,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        let rooted = RootedFs::open(destination.path().to_path_buf())
            .await
            .unwrap();
        let old = rooted
            .entry_stream(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        assert_eq!(source.size, old.size);
        assert_eq!(source.modified, old.modified);
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut reader, mut writer) = tokio::io::split(server_io);
        let raced_target = target.clone();
        let server = tokio::spawn(async move {
            let opened = crate::remote::server_handshake(&mut reader, &mut writer)
                .await
                .unwrap();
            let mut router =
                FrameRouter::start(reader, writer, RouterRole::Server, Default::default()).unwrap();
            let incoming = router.incoming().recv().await.unwrap().unwrap();
            let frame = incoming.first.frame();
            assert_eq!(
                frame.kind(),
                FrameKind::Metadata,
                "metadata-only work must not request DATA"
            );
            assert_eq!(frame.flags(), FrameFlags::FINAL | FrameFlags::ACK_REQUIRED);
            let stream_id = frame.stream_id();
            let metadata = WireMetadata::decode(frame.payload()).unwrap();
            let relative = crate::remote::path::decode_relative_path(
                metadata.path.clone(),
                opened.client.platform.os,
            )
            .unwrap();
            let crate::protocol::WireMetadataTarget::Observed { kind, identity } =
                metadata.target();
            assert_eq!(kind, crate::protocol::WireEntryKind::File);
            let xattrs = metadata.xattrs().map(|attrs| {
                attrs
                    .entries()
                    .iter()
                    .map(|entry| {
                        (
                            crate::remote::xattr::os_string_from_name(entry.name()),
                            entry.value().to_vec(),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            let acl = metadata.acl().map(|acl| acl.text().to_owned());
            drop(incoming);
            let identity = tokio::task::spawn_blocking(move || {
                let identity = rooted
                    .apply_observed_preservation_blocking(
                        &relative,
                        EntryKind::File,
                        EntryIdentity::from_bytes(identity),
                        metadata.unix_mode(),
                        metadata
                            .modified()
                            .map(|(s, ns)| Timestamp::new(s, ns).unwrap()),
                        &crate::rooted_fs::MetadataPreservation {
                            xattrs: xattrs.as_deref(),
                            acl: acl.as_deref(),
                            bsd_flags: metadata.bsd_flags(),
                        },
                    )
                    .unwrap();
                if late_change {
                    std::fs::write(raced_target, b"CHANGED").unwrap();
                }
                identity
            })
            .await
            .unwrap();
            router
                .sender()
                .send(
                    Frame::new(
                        FrameKind::Ack,
                        FrameFlags::empty(),
                        stream_id,
                        Bytes::copy_from_slice(identity.as_bytes()),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let (reader, writer) = tokio::io::split(client_io);
        let session = ClientRemoteSession::connect(
            reader,
            writer,
            Operation::Push,
            destination.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let executor = RemotePushExecutor::new(
            authority,
            session.request_handle(),
            Scheduler::new(Default::default()).unwrap(),
            Default::default(),
        )
        .with_xattrs(extended)
        .with_acls(extended && cfg!(feature = "acl"))
        .with_bsd_flags(extended);
        let policy = ExecutionPolicy {
            preserve_permissions: true,
            preserve_times: true,
        };
        let work = lower_sync_op(
            SyncOp::Metadata {
                source,
                destination: old,
            },
            policy,
        )
        .unwrap()
        .unwrap();
        let result = executor.execute(work).await;
        if late_change {
            let Err(RemotePushError::SourceCompletionUncertain(error)) = result else {
                panic!("expected committed SOURCE completion failure: {result:?}");
            };
            assert!(
                matches!(*error, RemotePushError::Remote(RemoteSessionError::Transfer(
                    crate::remote::transfer::RemoteTransferError::RootedFs(
                        crate::rooted_fs::RootedFsError::SourceBindingChanged(ref path)
                    )
                )) if path == Path::new("selected")),
                "{error:?}"
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!([stamp(&selected), stamp(&target)], originals);
        }
        server.await.unwrap();
        let metadata = std::fs::metadata(&output).unwrap();
        assert_eq!(metadata.mode() & 0o777, mode);
        assert_eq!(
            (metadata.mtime(), metadata.mtime_nsec()),
            (123, 456_000_000)
        );
        // The simple one-link destination stays in place. Reading the already
        // open OLD descriptor also works after the requested mode becomes 000.
        assert_eq!(metadata.ino(), old_reader.metadata().unwrap().ino());
        let mut bytes = Vec::new();
        old_reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes, b"OLD!!!",
            "quick-equal metadata must retain OLD DATA"
        );
        if extended {
            assert_eq!(
                xattr::get(&output, "user.sy-source-metadata").unwrap(),
                Some(b"target".to_vec())
            );
            assert_eq!(xattr::get(&output, "user.sy-stale").unwrap(), None);
            assert_eq!(metadata.st_flags() & libc::UF_HIDDEN, libc::UF_HIDDEN);
            #[cfg(feature = "acl")]
            assert_eq!(
                Some(exacl::to_string(&exacl::getfacl(&output, None).unwrap()).unwrap()),
                acl
            );
            #[cfg(not(feature = "acl"))]
            assert_eq!(acl, None);
        }
        assert_eq!(stamp(&selected), originals[0]);
        assert_eq!(std::fs::read_link(&selected).unwrap(), target);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
    }
}
