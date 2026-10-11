//! Retired aliases may follow only this root's own exact native transitions.
use super::*;
use std::io::Write;
use xattr::FileExt;

#[test]
fn prepared_alias_follows_own_retirements_but_refuses_foreign_held_inode_edits() {
    for foreign_edit in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"old bytes").unwrap();
        for name in ["b", "c"] {
            std::fs::hard_link(root.path().join("a"), root.path().join(name)).unwrap();
        }
        let rooted = RootedFs::open_blocking_for_worker(root.path().to_path_buf()).unwrap();
        let path = |name| RelativePath::new(name).unwrap();
        let held_old = rooted.open_regular_blocking(&path("c")).unwrap();
        let scanned = identity_from_stat(&stat_fd(held_old.as_raw_fd()).unwrap()).unwrap();
        let expected = ExpectedDestination::Unchanged(scanned);
        // Prepare before the other aliases are replaced: late admission must
        // translate authority too, not just staging preparation.
        let mut last = rooted
            .begin_staged_file_with_expectation_blocking(&path("c"), expected)
            .unwrap();
        last.file_mut().write_all(b"new c").unwrap();
        for name in ["a", "b"] {
            let mut replacement = rooted
                .begin_staged_file_with_expectation_blocking(&path(name), expected)
                .unwrap();
            replacement.file_mut().write_all(name.as_bytes()).unwrap();
            replacement
                .commit()
                .unwrap()
                .finalize_blocking(None)
                .unwrap();
        }
        assert_eq!(std::fs::read(root.path().join("c")).unwrap(), b"old bytes");
        rooted
            .copy_file_blocking(&path("c"), &path("backup"), scanned)
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("backup")).unwrap(),
            b"old bytes"
        );
        let current = identity_from_stat(&stat_fd(held_old.as_raw_fd()).unwrap()).unwrap();
        assert_eq!(
            rooted
                .retired_destination_identity_blocking(scanned)
                .unwrap(),
            current
        );
        if foreign_edit {
            // Xattrs leave all quick-check properties intact. This unrelated
            // ctime change is not an own unlink and must not be adopted by a
            // fresh stat, even when the descriptor is the original OLD inode.
            held_old
                .set_xattr("user.sy-lineage-foreign", b"foreign")
                .unwrap();
            let foreign = identity_from_stat(&stat_fd(held_old.as_raw_fd()).unwrap()).unwrap();
            assert_ne!(
                rooted.retirement.lock().unwrap().resolve(scanned).unwrap(),
                foreign
            );
            assert!(matches!(
                rooted.copy_file_blocking(&path("c"), &path("foreign-backup"), scanned),
                Err(RootedFsError::CopySourceChanged(_))
            ));
            assert!(!root.path().join("foreign-backup").exists());
            assert!(matches!(
                last.commit(),
                Err(RootedFsError::DestinationChanged(_))
            ));
            assert_eq!(std::fs::read(root.path().join("c")).unwrap(), b"old bytes");
            assert_eq!(
                held_old.get_xattr("user.sy-lineage-foreign").unwrap(),
                Some(b"foreign".to_vec())
            );
        } else {
            last.commit().unwrap().finalize_blocking(None).unwrap();
            assert_eq!(std::fs::read(root.path().join("c")).unwrap(), b"new c");
        }
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"a");
        assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"b");
    }
}

#[test]
fn observed_metadata_uses_exact_retirements_without_adopting_foreign_changes() {
    use std::os::unix::fs::PermissionsExt;
    for foreign_edit in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"retained bytes").unwrap();
        std::fs::hard_link(root.path().join("a"), root.path().join("b")).unwrap();
        let rooted = RootedFs::open_blocking_for_worker(root.path().to_path_buf()).unwrap();
        let a = RelativePath::new("a").unwrap();
        let b = RelativePath::new("b").unwrap();
        let held = rooted.open_regular_blocking(&b).unwrap();
        held.set_xattr("user.sy-retained", b"keep").unwrap();
        let scanned = identity_from_stat(&stat_fd(held.as_raw_fd()).unwrap()).unwrap();
        let mut replacement = rooted
            .begin_staged_file_with_expectation_blocking(
                &a,
                ExpectedDestination::Unchanged(scanned),
            )
            .unwrap();
        replacement.file_mut().write_all(b"replacement").unwrap();
        replacement
            .commit()
            .unwrap()
            .finalize_blocking(None)
            .unwrap();
        if foreign_edit {
            held.set_xattr("user.sy-foreign", b"foreign").unwrap();
        }
        let proof = crate::engine::hardlink_removals::GroupDestinationProof::Existing {
            path: b.clone(),
            identity: scanned,
        };
        let parity = proof.revalidate_blocking(&rooted);
        let attrs = rooted.read_observed_xattrs_blocking(&b, EntryKind::File, scanned);
        #[cfg(target_os = "macos")]
        let flags = rooted.read_observed_bsd_flags_blocking(&b, EntryKind::File, scanned);
        let mode_before = held.metadata().unwrap().permissions().mode() & 0o7777;
        let wanted_mode = if mode_before == 0o600 { 0o640 } else { 0o600 };
        let applied = rooted.apply_observed_preservation_blocking(
            &b,
            EntryKind::File,
            scanned,
            Some(wanted_mode),
            None,
            &MetadataPreservation::default(),
        );
        if foreign_edit {
            assert!(matches!(parity, Err(RootedFsError::DestinationChanged(_))));
            assert!(matches!(
                attrs,
                Err(RootedFsError::SourceMetadataChanged(_))
            ));
            #[cfg(target_os = "macos")]
            assert!(matches!(
                flags,
                Err(RootedFsError::SourceMetadataChanged(_))
            ));
            assert!(matches!(applied, Err(RootedFsError::DestinationChanged(_))));
            assert_eq!(
                held.metadata().unwrap().permissions().mode() & 0o7777,
                mode_before
            );
        } else {
            parity.unwrap();
            assert!(attrs
                .unwrap()
                .iter()
                .any(|(name, value)| name == "user.sy-retained" && value == b"keep"));
            #[cfg(target_os = "macos")]
            assert_eq!(flags.unwrap(), 0);
            applied.unwrap();
            assert_eq!(
                held.metadata().unwrap().permissions().mode() & 0o7777,
                wanted_mode
            );
            // A later metadata edit is not an old-inode retirement and cannot
            // be adopted by replaying the original scanned parity proof.
            assert!(matches!(
                proof.revalidate_blocking(&rooted),
                Err(RootedFsError::DestinationChanged(_))
            ));
        }
        assert_eq!(
            std::fs::read(root.path().join("b")).unwrap(),
            b"retained bytes"
        );
        assert_eq!(
            std::fs::read(root.path().join("a")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            held.get_xattr("user.sy-retained").unwrap(),
            Some(b"keep".to_vec())
        );
    }
}

#[tokio::test]
async fn held_file_validation_does_not_adopt_a_foreign_edit_before_name_check() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("file"), b"kept bytes").unwrap();
    let rooted = RootedFs::open(directory.path().to_path_buf())
        .await
        .unwrap();
    let path = RelativePath::new("file").unwrap();
    let held = rooted.open_regular_blocking(&path).unwrap();
    let scanned = identity_from_stat(&stat_fd(held.as_raw_fd()).unwrap()).unwrap();
    let worker_file = held.try_clone().unwrap();
    let worker_root = rooted.clone();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *rooted.file_observation_pause.lock().unwrap() = Some(FileObservationPause {
        reached: reached_tx,
        resume: resume_rx,
    });
    let worker = tokio::task::spawn_blocking(move || {
        worker_root.observed_regular_metadata_blocking(&worker_file, &path, scanned)
    });
    let reached = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
    held.set_xattr("user.sy-between-checks", b"foreign")
        .unwrap();
    let _ = resume_tx.send(());
    let result = worker.await.unwrap();
    assert!(
        matches!(reached, Ok(Ok(()))),
        "observation never reached the name-check boundary"
    );
    assert!(
        matches!(result, Err(RootedFsError::DestinationChanged(_))),
        "foreign state was adopted: {result:?}"
    );
    assert_eq!(
        std::fs::read(directory.path().join("file")).unwrap(),
        b"kept bytes"
    );
    assert_eq!(
        held.get_xattr("user.sy-between-checks").unwrap(),
        Some(b"foreign".to_vec())
    );
}

#[test]
fn destination_unlinks_advance_aliases_but_source_unlink_stays_exact() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a"), b"kept bytes").unwrap();
    for name in ["b", "c"] {
        std::fs::hard_link(directory.path().join("a"), directory.path().join(name)).unwrap();
    }
    let rooted = RootedFs::open_blocking_for_worker(directory.path().to_path_buf()).unwrap();
    let path = |name| RelativePath::new(name).unwrap();
    let held = rooted.open_regular_blocking(&path("c")).unwrap();
    let scanned = identity_from_stat(&stat_fd(held.as_raw_fd()).unwrap()).unwrap();
    rooted
        .remove_destination_blocking(&path("a"), false, Some(scanned))
        .unwrap();
    assert!(matches!(
        rooted.remove_source_blocking(&path("b"), scanned),
        Err(RootedFsError::DestinationChanged(_))
    ));
    assert_eq!(
        std::fs::read(directory.path().join("b")).unwrap(),
        b"kept bytes"
    );
    rooted
        .remove_destination_blocking(&path("b"), false, Some(scanned))
        .unwrap();
    assert_eq!(
        rooted
            .retired_destination_identity_blocking(scanned)
            .unwrap(),
        identity_from_stat(&stat_fd(held.as_raw_fd()).unwrap()).unwrap()
    );
    held.set_xattr("user.sy-foreign", b"foreign").unwrap();
    assert!(matches!(
        rooted.remove_destination_blocking(&path("c"), false, Some(scanned)),
        Err(RootedFsError::DestinationChanged(_))
    ));
    assert_eq!(
        std::fs::read(directory.path().join("c")).unwrap(),
        b"kept bytes"
    );
    assert_eq!(
        held.get_xattr("user.sy-foreign").unwrap(),
        Some(b"foreign".to_vec())
    );
}

#[tokio::test]
async fn visible_single_link_refuses_changes_after_capture_before_native_effect() {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no pointer arguments or side effects.
    assert_ne!(unsafe { libc::geteuid() }, 0);
    for delete in [false, true] {
        for change in ["metadata", "replacement", "hardlink"] {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            std::fs::write(&target, b"original old bytes").unwrap();
            let mut old = File::open(&target).unwrap();
            old.set_permissions(std::fs::Permissions::from_mode(0o0))
                .unwrap();
            assert_eq!(
                File::open(&target).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
            let relative = RelativePath::new("target").unwrap();
            let scanned = rooted.path_identity_blocking(&relative).unwrap().unwrap().1;
            let mut staged = if delete {
                None
            } else {
                let mut staged = rooted
                    .begin_staged_file_with_expectation_blocking(
                        &relative,
                        ExpectedDestination::Unchanged(scanned),
                    )
                    .unwrap();
                staged.file_mut().write_all(b"new bytes").unwrap();
                Some(staged)
            };
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            let pause = PublicationPause {
                point: PublicationPausePoint::AfterAdmission,
                reached: reached_tx,
                resume: resume_rx,
            };
            if let Some(staged) = &mut staged {
                staged.pause_publication(pause);
            } else {
                rooted.pause_mutation_at(0, pause);
            }
            let worker_root = rooted.clone();
            let worker = tokio::task::spawn_blocking(move || {
                if let Some(staged) = staged {
                    staged.commit().map(|_| ())
                } else {
                    worker_root
                        .remove_destination_blocking(&relative, false, Some(scanned))
                        .map(|_| ())
                }
            });
            let reached = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
            let mutated = (|| -> std::io::Result<()> {
                match change {
                    "metadata" => old.set_permissions(std::fs::Permissions::from_mode(0o200)),
                    "replacement" => {
                        let foreign = root.path().join("foreign");
                        std::fs::write(&foreign, b"foreign bytes")?;
                        std::fs::rename(foreign, &target)
                    }
                    "hardlink" => std::fs::hard_link(&target, root.path().join("alias")),
                    _ => unreachable!(),
                }
            })();
            let expected = stat_at_optional(rooted.root_fd.as_raw_fd(), OsStr::new("target"))
                .unwrap()
                .unwrap();
            let _ = resume_tx.send(());
            let result = worker.await.unwrap();
            reached.unwrap().unwrap();
            mutated.unwrap();
            assert!(
                matches!(result, Err(RootedFsError::DestinationChanged(_))),
                "delete={delete} change={change}: {result:?}"
            );
            let after = stat_at_optional(rooted.root_fd.as_raw_fd(), OsStr::new("target"))
                .unwrap()
                .unwrap();
            assert!(staging::same_observation(&expected, &after));
            old.seek(SeekFrom::Start(0)).unwrap();
            let mut bytes = Vec::new();
            old.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"original old bytes");
            if change == "replacement" {
                assert_eq!(std::fs::read(&target).unwrap(), b"foreign bytes");
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn unreadable_shared_and_exchange_old_entries_still_require_original_fd() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no pointer arguments or side effects.
    assert_ne!(unsafe { libc::geteuid() }, 0);
    for exchange in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, b"old").unwrap();
        let old = File::open(&target).unwrap();
        if !exchange {
            std::fs::hard_link(&target, root.path().join("alias")).unwrap();
        }
        old.set_permissions(std::fs::Permissions::from_mode(0o0))
            .unwrap();
        let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
        let relative = RelativePath::new("target").unwrap();
        let scanned = rooted.path_identity_blocking(&relative).unwrap().unwrap().1;
        if exchange {
            let mut namespace = rooted
                .begin_namespace_blocking(
                    relative.as_path(),
                    ExpectedDestination::Unchanged(scanned),
                )
                .unwrap();
            std::fs::create_dir(
                root.path()
                    .join(&namespace.staging_dir_name)
                    .join(&namespace.temp_name),
            )
            .unwrap();
            let staged = File::from(
                open_dir_at(namespace.staging_dir_fd.as_raw_fd(), &namespace.temp_name).unwrap(),
            );
            namespace.register_staging(&staged).unwrap();
            assert!(matches!(namespace.commit(),
                Err(RootedFsError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied));
        } else {
            assert!(
                matches!(rooted.remove_destination_blocking(&relative, false, Some(scanned)),
                Err(RootedFsError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
            );
            assert!(root.path().join("alias").exists());
        }
        assert_eq!(
            rooted.path_identity_blocking(&relative).unwrap().unwrap().1,
            scanned
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn unreadable_visible_old_flags_are_never_cleared_to_allow_native_mutation() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no pointer arguments or side effects.
    assert_ne!(unsafe { libc::geteuid() }, 0);
    for flags in [libc::UF_IMMUTABLE, libc::UF_APPEND] {
        for delete in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            std::fs::write(&target, b"old bytes").unwrap();
            let mut old = File::open(&target).unwrap();
            old.set_permissions(std::fs::Permissions::from_mode(0o0))
                .unwrap();
            let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
            // SAFETY: old retains the fixture inode; only user-settable flags
            // are used, and they are cleared below before any result assertions.
            assert_eq!(unsafe { libc::fchflags(old.as_raw_fd(), flags) }, 0);
            let before = stat_fd(old.as_raw_fd());
            let result = (|| -> Result<()> {
                let relative = RelativePath::new("target").unwrap();
                let scanned = rooted.path_identity_blocking(&relative)?.unwrap().1;
                if delete {
                    rooted
                        .remove_destination_blocking(&relative, false, Some(scanned))
                        .map(|_| ())
                } else {
                    let mut staged = rooted.begin_staged_file_with_expectation_blocking(
                        &relative,
                        ExpectedDestination::Unchanged(scanned),
                    )?;
                    staged.file_mut().write_all(b"replacement")?;
                    staged.commit().map(|_| ())
                }
            })();
            let after = stat_at_optional(rooted.root_fd.as_raw_fd(), OsStr::new("target"));
            // SAFETY: the same fixture FD is live; restore cleanup eligibility
            // even when the native operation or its validation unexpectedly fails.
            let cleared = unsafe { libc::fchflags(old.as_raw_fd(), 0) };
            assert_eq!(cleared, 0);
            assert!(
                matches!(result, Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::EPERM))
            );
            assert!(staging::same_observation(
                &before.unwrap(),
                &after.unwrap().unwrap()
            ));
            let mut bytes = Vec::new();
            old.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"old bytes");
        }
    }
}

#[test]
fn deletion_post_root_check_classifies_only_actual_unlinks_as_committed() {
    for case in ["removed", "absent", "nonempty"] {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("root");
        std::fs::create_dir(&original).unwrap();
        if case == "removed" {
            std::fs::write(original.join("target"), b"old").unwrap();
        } else if case == "nonempty" {
            std::fs::create_dir(original.join("target")).unwrap();
            std::fs::write(original.join("target/child"), b"retained").unwrap();
        }
        let rooted = RootedFs::open_blocking(original.clone()).unwrap();
        let path = RelativePath::new("target").unwrap();
        let expected = rooted
            .path_identity_blocking(&path)
            .unwrap()
            .map(|(_, identity)| identity);
        let outcome = rooted
            .remove_destination_blocking(&path, case == "nonempty", expected)
            .unwrap();
        let relocated = parent.path().join("held");
        std::fs::rename(&original, &relocated).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("foreign"), b"untouched").unwrap();
        let checked = outcome.verify_local_root(&rooted, &path);
        if case == "removed" {
            assert!(matches!(
                checked,
                Err(RootedFsError::DeletionCommittedFinalizationFailed { .. })
            ));
            assert!(!relocated.join("target").exists());
        } else {
            assert!(matches!(checked, Err(RootedFsError::RootChanged(_))));
            if case == "nonempty" {
                assert_eq!(
                    std::fs::read(relocated.join("target/child")).unwrap(),
                    b"retained"
                );
            }
        }
        assert_eq!(
            std::fs::read(original.join("foreign")).unwrap(),
            b"untouched"
        );
    }
}

#[test]
fn retirement_index_keeps_exact_ancestry_across_distinct_inode_chains() {
    let mut lineage = RetirementLineage::default();
    let identity = |group: u8, phase: u8| {
        let mut bytes = [0; 32];
        bytes[0] = phase;
        bytes[31] = group;
        EntryIdentity::from_bytes(bytes)
    };
    // Mixed early/late distinguishing bits exercise both radix roots and
    // replacement of their values, without an input-sized resident cache.
    for group in 0..=u8::MAX {
        lineage
            .record(identity(group, 0), identity(group, 1))
            .unwrap();
        lineage
            .record(identity(group, 1), identity(group, 2))
            .unwrap();
    }
    for group in (0..=u8::MAX).rev() {
        for phase in 0..=2 {
            assert_eq!(
                lineage.resolve(identity(group, phase)).unwrap(),
                identity(group, 2)
            );
        }
        assert_eq!(
            lineage.resolve(identity(group, 3)).unwrap(),
            identity(group, 3)
        );
    }
}
