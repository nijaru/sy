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
