//! Shared-inode refusal and namespace substitution across preservation fields.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use xattr::FileExt;

fn relative() -> RelativePath {
    RelativePath::new("file").unwrap()
}

fn requested_flags() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        Some(libc::UF_NODUMP)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn acl(file: &File) -> Option<String> {
    #[cfg(feature = "acl")]
    {
        let mut entries = exacl::getfacl(fd_fixture_path(file), None).unwrap();
        entries.push(exacl::AclEntry::allow_user(
            "1",
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        Some(exacl::to_string(&entries).unwrap())
    }
    #[cfg(not(feature = "acl"))]
    {
        let _ = file;
        None
    }
}

#[cfg(feature = "acl")]
fn fd_fixture_path(file: &File) -> PathBuf {
    // macOS ACL construction is path-based only in this fixture; held FD ACL
    // implementation is exercised by the operation under test below.
    #[cfg(target_os = "linux")]
    return PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

#[tokio::test]
async fn shared_inode_preservation_refuses_before_any_requested_field() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("excluded"), b"source bytes").unwrap();
    std::fs::hard_link(
        source.path().join("excluded"),
        destination.path().join("file"),
    )
    .unwrap();
    let rooted = RootedFs::open(destination.path().to_path_buf())
        .await
        .unwrap();
    let file = std::fs::File::open(source.path().join("excluded")).unwrap();
    let before = rooted
        .path_identity_blocking(&relative())
        .unwrap()
        .unwrap()
        .1;
    let attrs = vec![(OsString::from("user.sy-authority"), b"new".to_vec())];
    let acl = acl(&file);
    let flags = requested_flags();
    let mut requests = vec![
        (Some(0o600), None, MetadataPreservation::default()),
        (
            None,
            Some(Timestamp::UNIX_EPOCH),
            MetadataPreservation::default(),
        ),
        (
            None,
            None,
            MetadataPreservation {
                xattrs: Some(&attrs),
                ..Default::default()
            },
        ),
        (
            Some(0o600),
            Some(Timestamp::UNIX_EPOCH),
            MetadataPreservation {
                xattrs: Some(&attrs),
                acl: acl.as_deref(),
                bsd_flags: flags,
            },
        ),
    ];
    if let Some(acl) = acl.as_deref() {
        requests.push((
            None,
            None,
            MetadataPreservation {
                acl: Some(acl),
                ..Default::default()
            },
        ));
    }
    if let Some(flags) = flags {
        requests.push((
            None,
            None,
            MetadataPreservation {
                bsd_flags: Some(flags),
                ..Default::default()
            },
        ));
    }
    for (mode, time, preservation) in requests {
        assert!(matches!(
            rooted.apply_observed_preservation_blocking(
                &relative(),
                EntryKind::File,
                before,
                mode,
                time,
                &preservation
            ),
            Err(RootedFsError::SharedFileMetadata { links: 2, .. })
        ));
        assert_eq!(
            rooted
                .path_identity_blocking(&relative())
                .unwrap()
                .unwrap()
                .1,
            before
        );
        assert!(file.get_xattr("user.sy-authority").unwrap().is_none());
        assert_eq!(
            std::fs::read(source.path().join("excluded")).unwrap(),
            b"source bytes"
        );
    }
}

#[tokio::test]
async fn preservation_keeps_observed_descriptor_after_foreign_namespace_substitution() {
    for symlink in [false, true] {
        let destination = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(destination.path().join("file"), b"observed").unwrap();
        std::fs::write(outside.path().join("file"), b"foreign").unwrap();
        let rooted = RootedFs::open(destination.path().to_path_buf())
            .await
            .unwrap();

        let before = rooted
            .path_identity_blocking(&relative())
            .unwrap()
            .unwrap()
            .1;
        let held = std::fs::File::open(destination.path().join("file")).unwrap();
        let acl = acl(&held);
        let attrs = vec![(OsString::from("user.sy-authority"), b"new".to_vec())];
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::BeforeAdmission,
                reached: reached_tx,
                resume: resume_rx,
            },
        );
        let worker = tokio::task::spawn_blocking(move || {
            rooted.apply_observed_preservation_blocking(
                &relative(),
                EntryKind::File,
                before,
                Some(0o600),
                Some(Timestamp::UNIX_EPOCH),
                &MetadataPreservation {
                    xattrs: Some(&attrs),
                    acl: acl.as_deref(),
                    bsd_flags: requested_flags(),
                },
            )
        });
        let paused = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
        std::fs::rename(
            destination.path().join("file"),
            destination.path().join("held"),
        )
        .unwrap();
        if symlink {
            std::os::unix::fs::symlink(
                outside.path().join("file"),
                destination.path().join("file"),
            )
            .unwrap();
        } else {
            std::fs::rename(outside.path().join("file"), destination.path().join("file")).unwrap();
        }
        let foreign = if symlink {
            outside.path().join("file")
        } else {
            destination.path().join("file")
        };
        // Our deliberate rename can change ctime. Observe the foreign inode
        // after fixture substitution, before releasing the metadata operation.
        let foreign_before = crate::endpoint::local_identity::metadata_identity(
            &std::fs::metadata(&foreign).unwrap(),
            EntryKind::File,
        )
        .unwrap();
        let _ = resume_tx.send(());
        let result = worker.await.unwrap();
        assert!(matches!(paused, Ok(Ok(()))));
        assert!(
            matches!(result, Err(RootedFsError::DestinationChanged(_))),
            "{result:?}"
        );
        let foreign_kind = crate::endpoint::local_identity::metadata_identity(
            &std::fs::metadata(&foreign).unwrap(),
            EntryKind::File,
        )
        .unwrap();
        assert_eq!(foreign_kind, foreign_before);
        assert!(std::fs::File::open(&foreign)
            .unwrap()
            .get_xattr("user.sy-authority")
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
        assert_eq!(
            held.get_xattr("user.sy-authority").unwrap().unwrap(),
            b"new"
        );
        assert_eq!(
            held.metadata().unwrap().permissions().mode() & 0o7777,
            0o600
        );
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            assert_eq!(held.metadata().unwrap().st_flags(), libc::UF_NODUMP);
            assert_eq!(std::fs::metadata(&foreign).unwrap().st_flags(), 0);
        }
    }
}
