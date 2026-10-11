//! Shared-inode refusal and namespace substitution across preservation fields.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use xattr::FileExt;

#[tokio::test]
async fn native_preservation_is_effect_free_only_when_entire_request_matches() {
    use std::os::unix::fs::MetadataExt;

    for shared in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("file");
        std::fs::write(&path, b"original bytes").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let file = File::open(&path).unwrap();
        file.set_xattr("user.sy-idempotent", b"same\0bytes")
            .unwrap();
        file.set_xattr("user.sy-empty", b"").unwrap();
        let requested_acl = acl(&file);
        #[cfg(feature = "acl")]
        apply_acl_fd(&file, requested_acl.as_deref().unwrap()).unwrap();
        let flags = requested_flags();
        #[cfg(target_os = "macos")]
        apply_bsd_flags_fd(&file, flags.unwrap()).unwrap();
        if shared {
            std::fs::hard_link(&path, fixture.path().join("source-alias")).unwrap();
        }
        let rooted = RootedFs::open(fixture.path().into()).await.unwrap();
        // Include native labels, not just the fixture's user attributes.
        let attributes = bounded_xattrs::read_from_file(&file).unwrap();
        let metadata = file.metadata().unwrap();
        let modified =
            Timestamp::new(metadata.mtime(), metadata.mtime_nsec().try_into().unwrap()).unwrap();
        let mode = metadata.mode() & 0o7777;
        let initial = rooted
            .path_identity_blocking(&relative())
            .unwrap()
            .unwrap()
            .1;
        #[cfg(feature = "acl")]
        let native_acl = read_acl_entries_fd(&file).unwrap();
        // Repeat with the ORIGINAL observation, not a newly adopted token.
        // Numeric principal text also exercises native qualifier normalization.
        for _ in 0..2 {
            let result = rooted
                .apply_observed_preservation_blocking(
                    &relative(),
                    EntryKind::File,
                    initial,
                    Some(mode),
                    Some(modified),
                    &MetadataPreservation {
                        xattrs: Some(&attributes),
                        acl: requested_acl.as_deref(),
                        bsd_flags: flags,
                    },
                )
                .unwrap();
            assert_eq!(
                result, initial,
                "matching preservation changed inode metadata"
            );
            assert_eq!(
                rooted
                    .path_identity_blocking(&relative())
                    .unwrap()
                    .unwrap()
                    .1,
                initial
            );
            assert_eq!(bounded_xattrs::read_from_file(&file).unwrap(), attributes);
            #[cfg(feature = "acl")]
            assert_eq!(read_acl_entries_fd(&file).unwrap(), native_acl);
            #[cfg(target_os = "macos")]
            assert_eq!(stat_fd(file.as_raw_fd()).unwrap().st_flags, flags.unwrap());
        }
        // Single-field entry points share the same effect-free permission.
        rooted
            .write_xattrs_blocking(&relative(), EntryKind::File, &attributes)
            .unwrap();
        #[cfg(feature = "acl")]
        rooted
            .write_acl_blocking(
                &relative(),
                EntryKind::File,
                requested_acl.as_deref().unwrap(),
            )
            .unwrap();
        #[cfg(target_os = "macos")]
        rooted
            .write_bsd_flags_blocking(&relative(), EntryKind::File, flags.unwrap())
            .unwrap();
        assert_eq!(
            rooted
                .path_identity_blocking(&relative())
                .unwrap()
                .unwrap()
                .1,
            initial
        );
        if shared {
            // One differing field refuses the WHOLE otherwise-matching request,
            // including an ACL/flags-only difference late in application order.
            let mut different_attributes = attributes.clone();
            different_attributes.push((OsString::from("user.sy-new"), b"new".to_vec()));
            let mut requests = vec![
                (
                    Some(mode ^ 0o100),
                    Some(modified),
                    Some(attributes.as_slice()),
                    requested_acl.as_deref(),
                    flags,
                ),
                (
                    Some(mode),
                    Some(Timestamp::UNIX_EPOCH),
                    Some(attributes.as_slice()),
                    requested_acl.as_deref(),
                    flags,
                ),
                (
                    Some(mode),
                    Some(modified),
                    Some(different_attributes.as_slice()),
                    requested_acl.as_deref(),
                    flags,
                ),
            ];
            if requested_acl.is_some() {
                requests.push((
                    Some(mode),
                    Some(modified),
                    Some(attributes.as_slice()),
                    Some(""),
                    flags,
                ));
            }
            if flags.is_some() {
                requests.push((
                    Some(mode),
                    Some(modified),
                    Some(attributes.as_slice()),
                    requested_acl.as_deref(),
                    Some(0),
                ));
            }
            #[cfg(all(feature = "acl", target_os = "macos"))]
            let reordered_acl = {
                let mut entries = exacl::from_str(requested_acl.as_deref().unwrap()).unwrap();
                entries.reverse();
                exacl::to_string(&entries).unwrap()
            };
            #[cfg(all(feature = "acl", target_os = "macos"))]
            requests.push((
                Some(mode),
                Some(modified),
                Some(attributes.as_slice()),
                Some(&reordered_acl),
                flags,
            ));
            for (mode, modified, xattrs, acl, bsd_flags) in requests {
                let result = rooted.apply_observed_preservation_blocking(
                    &relative(),
                    EntryKind::File,
                    initial,
                    mode,
                    modified,
                    &MetadataPreservation {
                        xattrs,
                        acl,
                        bsd_flags,
                    },
                );
                assert!(
                    matches!(
                        result,
                        Err(RootedFsError::SharedFileMetadata { links: 2, .. })
                    ),
                    "{result:?}"
                );
                assert_eq!(
                    rooted
                        .path_identity_blocking(&relative())
                        .unwrap()
                        .unwrap()
                        .1,
                    initial
                );
            }
        }
        if shared {
            for result in [
                rooted.write_xattrs_blocking(&relative(), EntryKind::File, &[]),
                #[cfg(feature = "acl")]
                rooted.write_acl_blocking(&relative(), EntryKind::File, ""),
                #[cfg(target_os = "macos")]
                rooted.write_bsd_flags_blocking(&relative(), EntryKind::File, 0),
            ] {
                assert!(
                    matches!(
                        result,
                        Err(RootedFsError::SharedFileMetadata { links: 2, .. })
                    ),
                    "{result:?}"
                );
            }
            assert_eq!(
                rooted
                    .path_identity_blocking(&relative())
                    .unwrap()
                    .unwrap()
                    .1,
                initial
            );
        }
        #[cfg(feature = "acl")]
        if !shared {
            // Clearing a genuinely nontrivial ACL still performs native SET,
            // then a repeated clear must be effect-free (Linux retains base ACL).
            rooted
                .write_acl_blocking(&relative(), EntryKind::File, "")
                .unwrap();
            let cleared = rooted
                .path_identity_blocking(&relative())
                .unwrap()
                .unwrap()
                .1;
            assert_ne!(cleared, initial);
            assert_ne!(read_acl_entries_fd(&file).unwrap(), native_acl);
            #[cfg(target_os = "linux")]
            assert_eq!(read_acl_entries_fd(&file).unwrap(), exacl::from_mode(mode));
            #[cfg(target_os = "macos")]
            assert!(read_acl_entries_fd(&file).unwrap().is_empty());
            rooted
                .write_acl_blocking(&relative(), EntryKind::File, "")
                .unwrap();
            assert_eq!(
                rooted
                    .path_identity_blocking(&relative())
                    .unwrap()
                    .unwrap()
                    .1,
                cleared
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"original bytes");
        if shared {
            assert_eq!(
                std::fs::read(fixture.path().join("source-alias")).unwrap(),
                b"original bytes"
            );
        }
    }
}

#[tokio::test]
async fn effect_free_preservation_revalidates_observation_and_session_closure() {
    for shared in [false, true] {
        for cancel in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let path = fixture.path().join("file");
            std::fs::write(&path, b"original bytes").unwrap();
            if shared {
                std::fs::hard_link(&path, fixture.path().join("alias")).unwrap();
            }
            let mut rooted = RootedFs::open(fixture.path().into()).await.unwrap();
            let expected = rooted
                .path_identity_blocking(&relative())
                .unwrap()
                .unwrap()
                .1;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            let admission = Arc::new(PublicationAdmission::default());
            rooted.bind_session_mutations(Arc::clone(&admission), false);
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
                    expected,
                    Some(mode),
                    None,
                    &MetadataPreservation::default(),
                )
            });
            let paused = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
            if cancel {
                admission.close();
            } else {
                // Foreign same-inode edit after comparison must not be adopted
                // as a successful result, even though requested mode still matches.
                xattr::set(&path, "user.sy-foreign", b"foreign").unwrap();
            }
            let before_resume = crate::endpoint::local_identity::metadata_identity(
                &std::fs::metadata(&path).unwrap(),
                EntryKind::File,
            )
            .unwrap();
            let _ = resume_tx.send(());
            let result = worker.await.unwrap();
            assert!(matches!(paused, Ok(Ok(()))));
            if cancel {
                assert!(
                    matches!(result, Err(RootedFsError::CommitCancelled)),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(RootedFsError::DestinationChanged(_))),
                    "{result:?}"
                );
            }
            assert_eq!(
                crate::endpoint::local_identity::metadata_identity(
                    &std::fs::metadata(&path).unwrap(),
                    EntryKind::File
                )
                .unwrap(),
                before_resume
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"original bytes");
        }
    }
}

#[cfg(all(target_os = "macos", feature = "acl"))]
#[tokio::test]
async fn acl_read_denial_does_not_forbid_exclusive_set_or_prove_shared_noop() {
    use std::os::unix::fs::MetadataExt;

    for shared in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("file");
        std::fs::write(&path, b"retained bytes").unwrap();
        let file = File::open(&path).unwrap();
        let entries = [exacl::AclEntry::deny_user(
            &file.metadata().unwrap().uid().to_string(),
            exacl::Perm::READSECURITY,
            exacl::Flag::empty(),
        )];
        if shared {
            std::fs::hard_link(&path, fixture.path().join("alias")).unwrap();
        }
        exacl::setfacl(&[&path], &entries, None).unwrap();
        let rooted = RootedFs::open(fixture.path().into()).await.unwrap();
        // macOS also denies pathname stat under this ACE, but descriptor stat
        // remains available. Do not weaken required namespace read contracts.
        let expected = identity_from_stat(&stat_fd(file.as_raw_fd()).unwrap()).unwrap();
        // This is an actual native READ_SECURITY denial, not a mock or a gate
        // that silently marks the production backend unsupported under root.
        let required_read = rooted.read_open_file_acl_blocking(&file, &relative());
        assert!(
            matches!(required_read, Err(RootedFsError::Io(ref error)) if error.raw_os_error() == Some(libc::EACCES)),
            "{required_read:?}"
        );
        let denied_text = exacl::to_string(&entries).unwrap();
        if shared {
            let result = rooted.apply_observed_preservation_blocking(
                &relative(),
                EntryKind::File,
                expected,
                None,
                None,
                &MetadataPreservation {
                    acl: Some(&denied_text),
                    ..Default::default()
                },
            );
            assert!(
                matches!(
                    result,
                    Err(RootedFsError::SharedFileMetadata { links: 2, .. })
                ),
                "{result:?}"
            );
            assert_eq!(
                identity_from_stat(&stat_fd(file.as_raw_fd()).unwrap()),
                Some(expected)
            );
            // Required reads still report permission errors after refusal.
            assert!(
                matches!(read_acl_entries_fd(&file), Err(RootedFsError::Io(ref error)) if error.raw_os_error() == Some(libc::EACCES))
            );
            exacl::setfacl(&[&path], &[], None).unwrap();
        } else {
            // The shared low-level SET boundary (also used by private staging)
            // needs no pathname-stat permission and must not gain an ACL READ gate.
            apply_acl_fd(&file, "").unwrap();
            assert!(exacl::getfacl(&path, None).unwrap().is_empty());
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"retained bytes");
    }
}

#[cfg(all(target_os = "linux", feature = "acl"))]
#[tokio::test]
async fn acl_state_includes_directory_defaults_and_empty_request_removes_them() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("file");
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut entries = exacl::from_mode(0o700);
    entries.extend(exacl::from_mode(0o750).into_iter().map(|mut entry| {
        entry.flags |= exacl::Flag::DEFAULT;
        entry
    }));
    exacl::setfacl(&[&path], &entries, None).unwrap();
    let rooted = RootedFs::open(fixture.path().into()).await.unwrap();
    let file = File::open(&path).unwrap();
    let text = exacl::to_string(&exacl::getfacl(&path, None).unwrap()).unwrap();
    let before = stat_fd(file.as_raw_fd()).unwrap();
    for _ in 0..2 {
        rooted
            .finalize_directory_blocking(
                &relative(),
                identity_from_stat(&before).unwrap(),
                Some(0o700),
                None,
                &DirectoryPreservation {
                    acl: Some(text.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        let after = stat_fd(file.as_raw_fd()).unwrap();
        assert_eq!(
            (after.st_ctime, after.st_ctime_nsec),
            (before.st_ctime, before.st_ctime_nsec)
        );
        assert_eq!(exacl::getfacl(&path, None).unwrap(), entries);
    }
    rooted
        .finalize_directory_blocking(
            &relative(),
            identity_from_stat(&before).unwrap(),
            Some(0o700),
            None,
            &DirectoryPreservation {
                acl: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        exacl::getfacl(&path, None).unwrap(),
        exacl::from_mode(0o700)
    );
}

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
        // Namespace/shared-inode tests request 0600; keep their ACL compatible
        // so an ACL-mask conflict does not hide the ownership failure.
        #[cfg(target_os = "linux")]
        let mut entries = {
            let _ = file;
            let mut entries = exacl::from_mode(0o600);
            entries.push(exacl::AclEntry::allow_mask(
                exacl::Perm::empty(),
                exacl::Flag::empty(),
            ));
            entries
        };
        #[cfg(not(target_os = "linux"))]
        let mut entries = exacl::getfacl(fd_fixture_path(file), None).unwrap();
        entries.push(exacl::AclEntry::allow_user(
            "1",
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        #[cfg(target_os = "macos")]
        entries.push(exacl::AclEntry::deny_user(
            "1",
            exacl::Perm::WRITE,
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

#[cfg(all(feature = "acl", not(target_os = "linux")))]
fn fd_fixture_path(file: &File) -> PathBuf {
    // macOS ACL construction is path-based only in this fixture; held FD ACL
    // implementation is exercised by the operation under test below.
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

#[cfg(all(target_os = "linux", feature = "acl"))]
#[tokio::test]
async fn native_acl_outcome_is_verified_without_rewriting_its_mask() {
    use std::io::Write;

    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    std::fs::write(&source, b"source bytes").unwrap();
    let mut entries = exacl::from_mode(0o640);
    entries.push(exacl::AclEntry::allow_user(
        "12345",
        exacl::Perm::READ | exacl::Perm::WRITE,
        exacl::Flag::empty(),
    ));
    entries.push(exacl::AclEntry::allow_mask(
        exacl::Perm::READ,
        exacl::Flag::empty(),
    ));
    exacl::setfacl(&[&source], &entries, None).unwrap();
    // Canonical, native-observed ACL: the named user's write permission is
    // limited by the READ mask. chmod(0600) after ACL would silently clear it.
    let entries = exacl::getfacl(&source, None).unwrap();
    let text = exacl::to_string(&entries).unwrap();

    for (staged, kind) in [
        (false, EntryKind::File),
        (false, EntryKind::Directory),
        (true, EntryKind::File),
    ] {
        for requested_mode in [None, Some(0o640), Some(0o600)] {
            let destination = tempfile::tempdir().unwrap();
            let path = destination.path().join("file");
            if kind == EntryKind::Directory {
                std::fs::create_dir(&path).unwrap();
            } else {
                std::fs::write(&path, b"old bytes").unwrap();
            }
            let rooted = RootedFs::open(destination.path().into()).await.unwrap();
            let expected = rooted
                .path_identity_blocking(&relative())
                .unwrap()
                .unwrap()
                .1;
            let preservation = MetadataPreservation {
                acl: Some(&text),
                ..Default::default()
            };
            let result = if staged {
                let mut writer = rooted
                    .begin_staged_file_with_expectation_blocking(
                        &relative(),
                        ExpectedDestination::Unchanged(expected),
                    )
                    .unwrap();
                writer.file_mut().write_all(b"new bytes").unwrap();
                writer
                    .apply_metadata_blocking(requested_mode, None)
                    .unwrap();
                let result = writer.apply_preservation_blocking(None, Some(&text), requested_mode);
                assert_eq!(
                    exacl::getfacl(fd_alias_path(&writer.file), None).unwrap(),
                    entries
                );
                if result.is_ok() {
                    writer.commit().unwrap().finalize_blocking(None).unwrap();
                }
                result
            } else if kind == EntryKind::Directory {
                rooted.finalize_directory_blocking(
                    &relative(),
                    expected,
                    requested_mode,
                    None,
                    &DirectoryPreservation {
                        acl: Some(text.clone()),
                        ..Default::default()
                    },
                )
            } else {
                rooted
                    .apply_observed_preservation_blocking(
                        &relative(),
                        kind,
                        expected,
                        requested_mode,
                        None,
                        &preservation,
                    )
                    .map(|_| ())
            };
            if requested_mode == Some(0o600) {
                assert!(
                    matches!(
                        result,
                        Err(RootedFsError::PreservationModeConflict {
                            expected: 0o600,
                            actual: 0o640
                        })
                    ),
                    "{result:?}"
                );
            } else {
                result.unwrap();
            }
            if !staged || requested_mode != Some(0o600) {
                assert_eq!(exacl::getfacl(&path, None).unwrap(), entries);
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                    0o640
                );
            }
            if kind == EntryKind::File {
                let wanted: &[u8] = if staged && requested_mode != Some(0o600) {
                    b"new bytes"
                } else {
                    b"old bytes"
                };
                assert_eq!(std::fs::read(&path).unwrap(), wanted);
            }
        }
    }
    assert_eq!(exacl::getfacl(&source, None).unwrap(), entries);
    assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
}

#[tokio::test]
async fn exact_xattr_mirror_reports_denied_required_removal() {
    for observed in [false, true] {
        let destination = tempfile::tempdir().unwrap();
        let path = destination.path().join("file");
        std::fs::write(&path, b"unchanged").unwrap();
        let file = File::open(&path).unwrap();
        file.set_xattr("user.sy-stale", b"old").unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o444))
            .unwrap();
        let rooted = RootedFs::open(destination.path().to_path_buf())
            .await
            .unwrap();
        let expected = rooted
            .path_identity_blocking(&relative())
            .unwrap()
            .unwrap()
            .1;
        // Unlike successful fixtures, this deliberately requests removal of
        // every attribute on an unwritable inode. Exact preservation must fail,
        // not claim success while ignoring EACCES/EPERM from required removals.
        let result = if observed {
            rooted
                .apply_observed_preservation_blocking(
                    &relative(),
                    EntryKind::File,
                    expected,
                    None,
                    None,
                    &MetadataPreservation {
                        xattrs: Some(&[]),
                        ..Default::default()
                    },
                )
                .map(|_| ())
        } else {
            rooted.write_xattrs_blocking(&relative(), EntryKind::File, &[])
        };
        assert!(
            matches!(result, Err(RootedFsError::Io(ref error))
                if error.kind() == std::io::ErrorKind::PermissionDenied),
            "observed={observed}: {result:?}"
        );
        assert_eq!(
            file.get_xattr("user.sy-stale").unwrap(),
            Some(b"old".to_vec())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }
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
    let mut attrs = read_xattrs_from_file(&file).unwrap();
    attrs.push((OsString::from("user.sy-authority"), b"new".to_vec()));
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
        // Snapshot the original inode before substitution; retaining native
        // fields makes this an admissible exact mirror, not a label removal.
        let mut attrs = read_xattrs_from_file(&held).unwrap();
        attrs.push((OsString::from("user.sy-authority"), b"new".to_vec()));
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
