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
