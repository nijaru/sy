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
