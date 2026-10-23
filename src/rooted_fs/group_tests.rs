//! Native group ownership and partial-publication safety contracts.
use super::*;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;

fn relative(path: &str) -> RelativePath {
    RelativePath::new(path).unwrap()
}

fn prepared(rooted: &RootedFs) -> RootedStagedFile {
    prepared_with_mode(rooted, 0o600)
}

fn prepared_with_mode(rooted: &RootedFs, mode: u32) -> RootedStagedFile {
    let mut staged = rooted
        .begin_staged_file_blocking(&relative("first"))
        .unwrap();
    staged.file_mut().write_all(b"group bytes").unwrap();
    staged.apply_metadata_blocking(Some(mode), None).unwrap();
    assert_eq!(
        staged.staged_hash_blocking().unwrap(),
        blake3::hash(b"group bytes")
    );
    staged
}

#[test]
fn group_publishes_two_members_from_original_anchor_and_retains_creator_fd() {
    for replace in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        if replace {
            std::fs::write(root.path().join("first"), b"old shared bytes").unwrap();
            std::fs::hard_link(root.path().join("first"), root.path().join("nested/second"))
                .unwrap();
            std::fs::hard_link(root.path().join("first"), root.path().join("unselected")).unwrap();
        }
        let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
        let expected = |path: &str| {
            if replace {
                ExpectedDestination::Unchanged(
                    rooted
                        .path_identity_blocking(&relative(path))
                        .unwrap()
                        .unwrap()
                        .1,
                )
            } else {
                ExpectedDestination::Absent
            }
        };
        // Capture both OLD identities before any shared-old retirement.
        let first_expected = expected("first");
        let second_expected = expected("nested/second");
        let mut staged = prepared_with_mode(&rooted, 0);
        let creator_fd = staged.file_mut().as_raw_fd();
        let mut retained = staged.try_clone_file().unwrap();
        let anchor = root
            .path()
            .join(&staged.namespace.staging_dir_name)
            .join("contents");
        // Preparation is inode-level, including restrictive permissions.
        // No visible member reopen can support publication/final proof now.
        let original = stat_fd(creator_fd).unwrap();
        let mut group = staged.into_group_blocking().unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        for (path, expectation) in [
            ("first", first_expected),
            ("nested/second", second_expected),
        ] {
            let receipt = group
                .publish_member_blocking(&relative(path), expectation, &admission, || Ok(()))
                .unwrap();
            assert_eq!(receipt.path, relative(path));
            let (parent, leaf) = rooted.open_parent_blocking(Path::new(path)).unwrap();
            let named = stat_at_optional(parent.as_raw_fd(), &leaf)
                .unwrap()
                .unwrap();
            assert!(staging::same_inode(&named, &original));
            assert_eq!(named.st_mode & 0o7777, 0);
            assert!(anchor.is_file());
        }
        let finalized = group
            .finish_blocking(relative("first"), None, admission)
            .unwrap();
        assert_eq!(finalized.member_count(), 2);
        assert!(!anchor.parent().unwrap().exists());
        assert!(staging::same_inode(
            &original,
            &stat_fd(creator_fd).unwrap()
        ));
        assert_eq!(stat_fd(creator_fd).unwrap().st_nlink, 2);
        for path in ["first", "nested/second"] {
            finalized
                .proof_for_member_blocking(&relative(path))
                .unwrap()
                .revalidate_blocking(&rooted)
                .unwrap();
        }
        retained.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        retained.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"group bytes");
        if replace {
            assert_eq!(
                std::fs::read(root.path().join("unselected")).unwrap(),
                b"old shared bytes"
            );
        }
    }
}

#[test]
fn group_foreign_anchor_substitution_or_metadata_edit_retains_artifacts() {
    for substitute in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("second"), b"old second").unwrap();
        let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
        let staged = prepared(&rooted);
        let retained = staged.try_clone_file().unwrap();
        let anchor = root
            .path()
            .join(&staged.namespace.staging_dir_name)
            .join("contents");
        let mut group = staged.into_group_blocking().unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        group
            .publish_member_blocking(
                &relative("first"),
                ExpectedDestination::Absent,
                &admission,
                || Ok(()),
            )
            .unwrap();
        if substitute {
            std::fs::rename(&anchor, root.path().join("moved-anchor")).unwrap();
            std::fs::write(&anchor, b"foreign private name").unwrap();
        } else {
            retained
                .set_permissions(std::fs::Permissions::from_mode(0o640))
                .unwrap();
        }
        assert!(matches!(
            group.publish_member_blocking(
                &relative("second"),
                ExpectedDestination::SnapshotAtOpen,
                &admission,
                || Ok(())
            ),
            Err(RootedFsError::PartialGroupPublication { committed_members: 1, source, .. })
                if matches!(*source, RootedFsError::StagingEntryChanged(_))
        ));
        assert_eq!(
            std::fs::read(root.path().join("second")).unwrap(),
            b"old second"
        );
        assert!(matches!(
            group.abort_blocking(),
            Err(RootedFsError::StagingAbortFailed { .. })
        ));
        assert!(anchor.exists());
        assert_eq!(
            std::fs::read(&anchor).unwrap(),
            if substitute {
                b"foreign private name".as_slice()
            } else {
                b"group bytes".as_slice()
            }
        );
    }
}

#[test]
fn late_member_validation_failure_preserves_old_and_cannot_finalize_earlier_publication() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("second"), b"old second").unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let staged = prepared(&rooted);
    let retained = staged.try_clone_file().unwrap();
    let anchor_dir = root.path().join(&staged.namespace.staging_dir_name);
    let mut group = staged.into_group_blocking().unwrap();
    let admission = Arc::new(PublicationAdmission::default());
    group
        .publish_member_blocking(
            &relative("first"),
            ExpectedDestination::Absent,
            &admission,
            || Ok(()),
        )
        .unwrap();
    assert!(matches!(
        group.publish_member_blocking(
            &relative("second"),
            ExpectedDestination::SnapshotAtOpen,
            &admission,
            || Err(RootedFsError::SourceMetadataChanged(PathBuf::from(
                "source"
            ))),
        ),
        Err(RootedFsError::PartialGroupPublication { committed_members: 1, source, .. })
            if matches!(*source, RootedFsError::SourceMetadataChanged(_))
    ));
    assert_eq!(stat_fd(retained.as_raw_fd()).unwrap().st_nlink, 2);
    assert_eq!(
        std::fs::read(root.path().join("first")).unwrap(),
        b"group bytes"
    );
    assert_eq!(
        std::fs::read(root.path().join("second")).unwrap(),
        b"old second"
    );
    assert!(matches!(
        group.finish_blocking(relative("first"), None, admission),
        Err(RootedFsError::IncompleteStagedGroup)
    ));
    assert!(!anchor_dir.exists());
    assert_eq!(stat_fd(retained.as_raw_fd()).unwrap().st_nlink, 1);
}

#[test]
fn native_member_commit_failure_accounts_for_owned_alias_abort_unlink() {
    // Write-denied destination parents force native commit failure after the
    // member-private alias exists. This fixture requires an ordinary user.
    // SAFETY: geteuid has no pointer arguments or side effects.
    assert_ne!(unsafe { libc::geteuid() }, 0);
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("nested");
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("second"), b"old second").unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let staged = prepared(&rooted);
    let retained = staged.try_clone_file().unwrap();
    let mut group = staged.into_group_blocking().unwrap();
    let admission = Arc::new(PublicationAdmission::default());
    group
        .publish_member_blocking(
            &relative("first"),
            ExpectedDestination::Absent,
            &admission,
            || Ok(()),
        )
        .unwrap();
    let error = group
        .publish_member_blocking(
            &relative("nested/second"),
            ExpectedDestination::SnapshotAtOpen,
            &admission,
            || {
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500))?;
                Ok(())
            },
        )
        .unwrap_err();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The owned private alias is unlinked even though its now-write-denied
    // parent retains the empty private directory as a reported artifact.
    assert!(matches!(
        error,
        RootedFsError::PartialGroupPublication { committed_members: 1, source, .. }
            if matches!(*source, RootedFsError::StagingOperationAbortFailed { .. })
    ));
    assert_eq!(stat_fd(retained.as_raw_fd()).unwrap().st_nlink, 2);
    assert_eq!(std::fs::read(parent.join("second")).unwrap(), b"old second");
    assert_eq!(
        std::fs::read(root.path().join("first")).unwrap(),
        b"group bytes"
    );
    group.abort_blocking().unwrap();
    assert_eq!(stat_fd(retained.as_raw_fd()).unwrap().st_nlink, 1);
}

#[test]
fn prepared_group_preservation_survives_all_links_without_source_mutation() {
    let fixture = tempfile::tempdir().unwrap();
    let source_path = fixture.path().join("source");
    std::fs::write(&source_path, b"preserved bytes").unwrap();
    xattr::set(&source_path, "user.sy_group", b"preserved attribute").unwrap();
    let source = File::open(&source_path).unwrap();
    let source_before = stat_fd(source.as_raw_fd()).unwrap();
    // Mirror the actual complete native set, including a mandatory SELinux
    // label on Linux. A synthetic user-only set is not the source's metadata.
    let attrs = read_xattrs_from_file(&source).unwrap();
    let root = tempfile::tempdir().unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let mut staged = rooted
        .begin_staged_file_blocking(&relative("first"))
        .unwrap();
    staged.file_mut().write_all(b"preserved bytes").unwrap();
    staged
        .apply_preservation_blocking(Some(0o640), None, Some(&attrs), None)
        .unwrap();
    let original = staged.try_clone_file().unwrap();
    staged.staged_hash_blocking().unwrap();
    let mut group = staged.into_group_blocking().unwrap();
    let admission = Arc::new(PublicationAdmission::default());
    for path in ["first", "second"] {
        group
            .publish_member_blocking(
                &relative(path),
                ExpectedDestination::Absent,
                &admission,
                || Ok(()),
            )
            .unwrap();
    }
    let finalized = group
        .finish_blocking(relative("first"), None, admission)
        .unwrap();
    assert_eq!(read_xattrs_from_file(&original).unwrap(), attrs);
    assert_eq!(
        stat_fd(original.as_raw_fd()).unwrap().st_mode & 0o7777,
        0o640
    );
    finalized
        .proof_for_member_blocking(&relative("second"))
        .unwrap();
    assert!(staging::same_observation(
        &source_before,
        &stat_fd(source.as_raw_fd()).unwrap()
    ));
}

#[cfg(all(target_os = "macos", feature = "acl"))]
#[test]
fn source_acl_override_skips_only_the_discarded_old_acl_read() {
    for override_acl in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("old");
        std::fs::write(&path, b"OLD bytes").unwrap();
        xattr::set(&path, "user.sy-default", b"retained").unwrap();
        let original = File::open(&path).unwrap();
        assert_ne!(stat_fd(original.as_raw_fd()).unwrap().st_uid, 0);
        // A well-known everyone UUID avoids asynchronous account-membership
        // lookup: the fixture must deny READ_SECURITY, not test its cache.
        let denied = [exacl::AclEntry::deny_group(
            "abcdefab-cdef-abcd-efab-cdef0000000c",
            exacl::Perm::READSECURITY,
            exacl::Flag::empty(),
        )];
        acl_macos::write_fd_entries(original.as_raw_fd(), &denied).unwrap();
        let before = stat_fd(original.as_raw_fd()).unwrap();
        let rooted = RootedFs::open_blocking(root.path().into()).unwrap();
        // The held OLD descriptor is intentional: macOS also denies pathname
        // stat under this ACE. This test does not waive namespace validation.
        let required_read = rooted.read_open_file_acl_blocking(&original, &relative("old"));
        assert!(
            matches!(required_read,
                Err(RootedFsError::Io(ref error)) if error.kind() == std::io::ErrorKind::PermissionDenied),
            "{required_read:?}"
        );
        let requested = crate::endpoint::io::Preservation {
            acl: override_acl.then(String::new),
            ..Default::default()
        };
        let mut stage = prepared(&rooted);
        let operation = stage.copy_retained_metadata_blocking(&original, requested.request());
        if override_acl {
            operation.unwrap();
            stage
                .apply_preservation_blocking(None, None, None, requested.acl.as_deref())
                .unwrap();
            assert!(read_acl_entries_fd(stage.file_mut()).unwrap().is_empty());
            assert_eq!(
                read_xattrs_from_file(stage.file_mut()).unwrap(),
                read_xattrs_from_file(&original).unwrap()
            );
        } else {
            assert!(
                matches!(operation, Err(RootedFsError::Io(ref error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
            );
        }
        stage.abort().unwrap();
        assert!(staging::same_observation(
            &before,
            &stat_fd(original.as_raw_fd()).unwrap()
        ));
        assert!(
            matches!(read_acl_entries_fd(&original), Err(RootedFsError::Io(ref error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
        exacl::setfacl(&[&path], &[], None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"OLD bytes");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[test]
fn finalized_group_rejects_foreign_member_binding_and_inode_edits() {
    for replace_name in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
        let staged = prepared(&rooted);
        let mut retained = staged.try_clone_file().unwrap();
        let mut group = staged.into_group_blocking().unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        for path in ["first", "second"] {
            group
                .publish_member_blocking(
                    &relative(path),
                    ExpectedDestination::Absent,
                    &admission,
                    || Ok(()),
                )
                .unwrap();
        }
        let finalized = group
            .finish_blocking(relative("first"), None, admission)
            .unwrap();
        if replace_name {
            std::fs::rename(root.path().join("second"), root.path().join("moved")).unwrap();
            std::fs::write(root.path().join("second"), b"foreign").unwrap();
        } else {
            retained.seek(SeekFrom::Start(0)).unwrap();
            retained.write_all(b"foreign edit").unwrap();
        }
        assert!(matches!(
            finalized.proof_for_member_blocking(&relative("second")),
            Err(RootedFsError::DestinationChanged(_))
        ));
    }
}

#[cfg(target_os = "macos")]
async fn source_guarded_destination(source: &Path, destination: &Path) -> RootedFs {
    source_guarded_destination_with_parents(source, destination, &[]).await
}

#[cfg(target_os = "macos")]
async fn source_guarded_destination_with_parents(
    source: &Path,
    destination: &Path,
    parents: &[&str],
) -> RootedFs {
    use crate::endpoint::source_root::SourceRoot;
    use crate::engine::namespace::NamespaceQualification;
    use crate::rooted_fs::local_source_effects::LocalSourceEffects;
    use futures::TryStreamExt;
    let source = SourceRoot::open(source.to_path_buf()).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let rooted = RootedFs::open(destination.to_path_buf()).await.unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    for path in parents {
        namespace
            .qualify(crate::engine::namespace::NamespaceParent::Directory(
                relative(path),
            ))
            .await
            .unwrap();
    }
    namespace.seal().await.unwrap();
    rooted.local_source_effects = Some(effects);
    rooted
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn group_link_and_anchor_retirement_recheck_original_ownership_after_source_census() {
    for (finish, change) in [
        (false, "foreign-link"),
        (true, "foreign-link"),
        (false, "anchor-relocation"),
        (true, "anchor-relocation"),
        (false, "OLD-link"),
        (false, "alias-relocation"),
    ] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("original"), b"SOURCE").unwrap();
        std::fs::write(destination.path().join("second"), b"OLD second").unwrap();
        let rooted = source_guarded_destination(source.path(), destination.path()).await;
        let second_expected = ExpectedDestination::Unchanged(
            rooted
                .path_identity_blocking(&relative("second"))
                .unwrap()
                .unwrap()
                .1,
        );
        let staged = prepared(&rooted);
        let retained = staged.try_clone_file().unwrap();
        let anchor_dir = destination.path().join(&staged.namespace.staging_dir_name);
        let anchor = anchor_dir.join("contents");
        let mut group = staged.into_group_blocking().unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        group
            .publish_member_blocking(
                &relative("first"),
                ExpectedDestination::Absent,
                &admission,
                || Ok(()),
            )
            .unwrap();
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        // Member-private DIR qualification precedes link admission; finish
        // directly admits anchor retirement. Both pauses are AFTER census.
        rooted.pause_mutation_at(
            usize::from(!finish),
            PublicationPause {
                point: PublicationPausePoint::AfterSourceValidation,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            if finish {
                let result = group
                    .finish_blocking(relative("first"), None, admission)
                    .map(|_| ());
                (result, None)
            } else {
                let result = group
                    .publish_member_blocking(
                        &relative("second"),
                        second_expected,
                        &admission,
                        || Ok(()),
                    )
                    .map(|_| ());
                let cleanup = group.abort_blocking();
                (result, Some(cleanup))
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        let protected = source.path().join("arrival");
        match change {
            "foreign-link" => std::fs::hard_link(&anchor, &protected).unwrap(),
            "anchor-relocation" => std::fs::rename(&anchor_dir, &protected).unwrap(),
            "OLD-link" => {
                std::fs::hard_link(destination.path().join("second"), &protected).unwrap()
            }
            "alias-relocation" => {
                let alias_dir = std::fs::read_dir(destination.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.is_dir() && *path != anchor_dir)
                    .unwrap();
                std::fs::rename(alias_dir, &protected).unwrap();
            }
            _ => unreachable!(),
        }
        let protected_file = if change == "anchor-relocation" {
            protected.join("contents")
        } else {
            protected
        };
        let before = stat_fd(File::open(&protected_file).unwrap().as_raw_fd()).unwrap();
        resume.send(()).unwrap();
        let (result, cleanup) = work.await.unwrap();
        let error = result.unwrap_err();
        if finish {
            assert!(
                matches!(error, RootedFsError::CommittedFinalizationFailed { .. }),
                "{error}"
            );
        } else {
            assert!(
                matches!(
                    error,
                    RootedFsError::PartialGroupPublication {
                        committed_members: 1,
                        ..
                    }
                ),
                "{error}"
            );
        }
        if let Some(cleanup) = cleanup {
            if matches!(change, "OLD-link" | "alias-relocation") {
                cleanup.unwrap();
            } else {
                assert!(cleanup.is_err(), "foreign-owned anchor must be retained");
            }
        }
        assert!(
            staging::same_observation(
                &before,
                &stat_fd(File::open(&protected_file).unwrap().as_raw_fd()).unwrap()
            ),
            "{finish}/{change}"
        );
        if change == "alias-relocation" {
            assert_eq!(std::fs::read_dir(&protected_file).unwrap().count(), 0);
        }
        assert_eq!(
            std::fs::read(destination.path().join("first")).unwrap(),
            b"group bytes"
        );
        assert_eq!(
            std::fs::read(destination.path().join("second")).unwrap(),
            b"OLD second"
        );
        assert_eq!(
            stat_fd(retained.as_raw_fd()).unwrap().st_nlink,
            if change == "foreign-link" {
                3
            } else if change == "anchor-relocation" {
                2
            } else {
                1
            }
        );
        assert_eq!(
            std::fs::read_dir(destination.path()).unwrap().count(),
            if change == "foreign-link" { 3 } else { 2 },
            "no member-private artifact: {finish}/{change}"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn group_publication_rechecks_both_private_bindings_after_final_source_census() {
    for move_anchor in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("original"), b"SOURCE").unwrap();
        let rooted = source_guarded_destination(source.path(), destination.path()).await;
        let staged = prepared(&rooted);
        let anchor_dir = destination.path().join(&staged.namespace.staging_dir_name);
        let retained = staged.try_clone_file().unwrap();
        let mut group = staged.into_group_blocking().unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        group
            .publish_member_blocking(
                &relative("first"),
                ExpectedDestination::Absent,
                &admission,
                || Ok(()),
            )
            .unwrap();
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        // Private-DIR qualification, alias link and first publication admission
        // precede the final publication census. Move only after that completes.
        rooted.pause_mutation_at(
            3,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceValidation,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            let result = group.publish_member_blocking(
                &relative("second"),
                ExpectedDestination::Absent,
                &admission,
                || Ok(()),
            );
            let cleanup = group.abort_blocking();
            (result, cleanup)
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        let relocated = if move_anchor {
            anchor_dir.clone()
        } else {
            std::fs::read_dir(destination.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.is_dir() && *path != anchor_dir)
                .unwrap()
        };
        let protected = source.path().join("arrival");
        std::fs::rename(relocated, &protected).unwrap();
        let before = stat_fd(retained.as_raw_fd()).unwrap();
        resume.send(()).unwrap();
        let (result, cleanup) = work.await.unwrap();
        assert!(
            matches!(
                result,
                Err(RootedFsError::PartialGroupPublication {
                    committed_members: 1,
                    ..
                })
            ),
            "{result:?}"
        );
        assert!(
            cleanup.is_err(),
            "SOURCE-reachable group links must be retained"
        );
        assert!(!destination.path().join("second").exists());
        assert!(protected.join("contents").is_file());
        assert!(staging::same_observation(
            &before,
            &stat_fd(retained.as_raw_fd()).unwrap()
        ));
        assert_eq!(
            std::fs::read(destination.path().join("first")).unwrap(),
            b"group bytes"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn source_guarded_private_file_effects_recheck_private_binding_after_final_census() {
    for metadata in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("original"), b"SOURCE").unwrap();
        let rooted = source_guarded_destination(source.path(), destination.path()).await;
        let mut staged = prepared(&rooted);
        let anchor_dir = destination.path().join(&staged.namespace.staging_dir_name);
        let retained = staged.try_clone_file().unwrap();
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            usize::from(!metadata),
            PublicationPause {
                point: PublicationPausePoint::AfterSourceValidation,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            if metadata {
                let operation = staged.apply_metadata_blocking(Some(0o644), None);
                (operation, Some(staged.abort()))
            } else {
                (staged.commit().map(|_| ()), None)
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        let protected = source.path().join("arrival");
        std::fs::rename(anchor_dir, &protected).unwrap();
        let before = stat_fd(retained.as_raw_fd()).unwrap();
        resume.send(()).unwrap();
        let (operation, cleanup) = work.await.unwrap();
        assert!(operation.is_err(), "{metadata}");
        if let Some(cleanup) = cleanup {
            assert!(cleanup.is_err());
        }
        assert!(!destination.path().join("first").exists());
        assert_eq!(
            std::fs::read(protected.join("contents")).unwrap(),
            b"group bytes"
        );
        assert!(
            staging::same_observation(&before, &stat_fd(retained.as_raw_fd()).unwrap()),
            "{metadata}"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn source_guarded_group_rechecks_nonrepresentative_members_before_shared_inode_effects() {
    for (action, skip) in [("link", 1), ("publish", 3), ("finish", 0), ("flags", 1)] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("original"), b"SOURCE").unwrap();
        std::fs::create_dir(destination.path().join("nested")).unwrap();
        let rooted =
            source_guarded_destination_with_parents(source.path(), destination.path(), &["nested"])
                .await;
        let staged = prepared(&rooted);
        let anchor_dir = destination.path().join(&staged.namespace.staging_dir_name);
        let retained = staged.try_clone_file().unwrap();
        let mut group = staged.into_group_blocking().unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        for name in ["first", "nested/other"] {
            group
                .publish_member_blocking(
                    &relative(name),
                    ExpectedDestination::Absent,
                    &admission,
                    || Ok(()),
                )
                .unwrap();
        }
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            skip,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceValidation,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            if matches!(action, "finish" | "flags") {
                let result = group
                    .finish_blocking(
                        relative("first"),
                        (action == "flags").then_some(libc::UF_NODUMP),
                        admission,
                    )
                    .map(|_| ());
                (result, None)
            } else {
                let result = group
                    .publish_member_blocking(
                        &relative("third"),
                        ExpectedDestination::Absent,
                        &admission,
                        || Ok(()),
                    )
                    .map(|_| ());
                (result, Some(group.abort_blocking()))
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        let protected = source.path().join("arrival");
        std::fs::rename(destination.path().join("nested"), &protected).unwrap();
        let before = stat_fd(retained.as_raw_fd()).unwrap();
        resume.send(()).unwrap();
        let (result, cleanup) = work.await.unwrap();
        assert!(result.is_err(), "{action}: missing all-member barrier");
        if let Some(cleanup) = cleanup {
            assert!(cleanup.is_err(), "{action}: shared anchor must be retained");
        }
        assert!(
            staging::same_observation(&before, &stat_fd(retained.as_raw_fd()).unwrap()),
            "{action}"
        );
        assert_eq!(
            std::fs::read(protected.join("other")).unwrap(),
            b"group bytes"
        );
        assert_eq!(
            std::fs::read(destination.path().join("first")).unwrap(),
            b"group bytes"
        );
        assert!(!destination.path().join("third").exists());
        assert_eq!(anchor_dir.exists(), action != "flags", "{action}");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn terminal_source_keeps_only_certified_group_cleanup_available() {
    for relocate_member in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let original = source.path().join("original");
        std::fs::write(&original, b"SOURCE").unwrap();
        std::fs::create_dir(destination.path().join("nested")).unwrap();
        let rooted =
            source_guarded_destination_with_parents(source.path(), destination.path(), &["nested"])
                .await;
        let staged = prepared(&rooted);
        let anchor_dir = destination.path().join(&staged.namespace.staging_dir_name);
        let retained = staged.try_clone_file().unwrap();
        let mut group = staged.into_group_blocking().unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        for name in ["first", "nested/other"] {
            group
                .publish_member_blocking(
                    &relative(name),
                    ExpectedDestination::Absent,
                    &admission,
                    || Ok(()),
                )
                .unwrap();
        }
        if relocate_member {
            std::fs::rename(
                destination.path().join("nested"),
                source.path().join("arrival"),
            )
            .unwrap();
        }
        std::fs::write(&original, b"changed SOURCE").unwrap();
        assert!(rooted
            .local_source_effects
            .as_ref()
            .unwrap()
            .admit_native_effect_blocking(SourceEffect {
                kind: SourceEffectKind::LeafPublication,
                parent: rooted.root_fd.as_raw_fd(),
                name: OsStr::new("third"),
                object: None,
                path: Path::new("third"),
            })
            .is_err());
        let before = stat_fd(retained.as_raw_fd()).unwrap();
        let result = group.abort_blocking();
        assert_eq!(result.is_err(), relocate_member);
        assert_eq!(anchor_dir.exists(), relocate_member);
        if relocate_member {
            assert!(staging::same_observation(
                &before,
                &stat_fd(retained.as_raw_fd()).unwrap()
            ));
            assert_eq!(
                std::fs::read(source.path().join("arrival/other")).unwrap(),
                b"group bytes"
            );
        } else {
            assert_eq!(
                stat_fd(retained.as_raw_fd()).unwrap().st_nlink,
                before.st_nlink - 1
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn immutable_group_flags_follow_anchor_retirement_on_original_creator_fd() {
    let root = tempfile::tempdir().unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let staged = prepared(&rooted);
    let original = staged.try_clone_file().unwrap();
    let anchor_dir = root.path().join(&staged.namespace.staging_dir_name);
    let mut group = staged.into_group_blocking().unwrap();
    let admission = Arc::new(PublicationAdmission::default());
    for path in ["first", "second"] {
        group
            .publish_member_blocking(
                &relative(path),
                ExpectedDestination::Absent,
                &admission,
                || Ok(()),
            )
            .unwrap();
    }
    let result = group.finish_blocking(
        relative("first"),
        Some(libc::UF_IMMUTABLE | libc::UF_APPEND),
        admission,
    );
    // Clear only the newly-created test inode so tempfile cleanup is possible,
    // even if the assertions below fail. Production never clears OLD flags.
    let after = stat_fd(original.as_raw_fd()).unwrap();
    let binding = result
        .as_ref()
        .ok()
        .map(|finalized| finalized.proof_for_member_blocking(&relative("first")));
    apply_bsd_flags_fd(&original, 0).unwrap();
    result.unwrap();
    binding.unwrap().unwrap();
    assert!(!anchor_dir.exists());
    assert_eq!(after.st_nlink, 2);
    assert_eq!(after.st_flags, libc::UF_IMMUTABLE | libc::UF_APPEND);
}
