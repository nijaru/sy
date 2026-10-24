use super::*;
use crate::endpoint::source_root::SourceRoot;
use futures::TryStreamExt;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

#[tokio::test]
async fn sibling_file_follow_has_physical_membership_and_authenticated_data() {
    for (rooted_scanner, chain) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let target_name = OsStr::new("target-é");
        let link_name = OsStr::new("link-é");
        let target = root.path().join(target_name);
        let link = root.path().join(link_name);
        let mut names = vec![link_name, target_name];
        let first_target = if chain {
            let middle = OsStr::new("middle-é");
            std::os::unix::fs::symlink(target_name, root.path().join(middle)).unwrap();
            names.push(middle);
            middle
        } else {
            target_name
        };
        std::os::unix::fs::symlink(first_target, &link).unwrap();
        std::fs::write(&target, b"original DATA").unwrap();
        std::fs::write(destination.path().join("independent"), b"OLD").unwrap();
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
        let originals: Vec<_> = names
            .iter()
            .map(|name| stamp(&root.path().join(name)))
            .collect();
        let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
        let rooted = source.rooted();
        let request = crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        let entries = if rooted_scanner {
            rooted
                .source_entry_stream(request)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
        } else {
            source
                .entries(request)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
        };
        assert_eq!(entries.len(), names.len());
        assert!(entries.iter().all(|entry| entry.is_file()));
        let followed = entries
            .iter()
            .find(|entry| entry.path.as_path() == Path::new(link_name))
            .unwrap();
        assert_eq!(followed.size, b"original DATA".len() as u64);
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        assert_eq!(
            inventory.record_count(),
            names.len() as u64 + 1,
            "discovery must not invent an extra physical member"
        );
        inventory.validate_originals_blocking().unwrap();
        let destination_root = RootedFs::open(destination.path().to_path_buf())
            .await
            .unwrap();
        let independent = object_key(
            destination_root.root_fd.as_raw_fd(),
            Some(OsStr::new("independent")),
        )
        .unwrap()
        .unwrap();
        assert!(inventory.excludes_objects_blocking(&[independent]).unwrap());
        for name in &names {
            let protected = object_key(rooted.root_fd.as_raw_fd(), Some(name))
                .unwrap()
                .unwrap();
            assert!(!inventory.excludes_objects_blocking(&[protected]).unwrap());
        }
        let mut reader = rooted
            .open_source_following_blocking(&followed.path)
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original DATA");
        assert_eq!(
            names
                .iter()
                .map(|name| stamp(&root.path().join(name)))
                .collect::<Vec<_>>(),
            originals
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new(first_target));
        assert!(matches!(
            rooted.check_source_removal_support_blocking(&followed.path),
            Err(RootedFsError::SourceFollowRemovalUnsupported(_))
        ));
    }
}

#[tokio::test]
async fn sibling_target_change_is_a_child_race_not_an_operator_root_replacement() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target"), b"original DATA").unwrap();
    std::fs::write(root.path().join("other"), b"fresh CHILD").unwrap();
    std::os::unix::fs::symlink("target", root.path().join("link")).unwrap();
    let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
    let rooted = source.rooted();
    let request = crate::engine::scan::ScanRequest {
        follow_symlinks: true,
        ..Default::default()
    };
    source
        .entries(request)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let inventory = rooted.seal_source_inventory_blocking().unwrap();
    std::fs::remove_file(root.path().join("link")).unwrap();
    std::os::unix::fs::symlink("other", root.path().join("link")).unwrap();
    assert!(matches!(
        inventory.validate_originals_blocking(),
        Err(RootedFsError::SourceBindingChanged(_))
    ));
    // There is no active work in this native owner test; production watch needs
    // its separately qualified join/drain token before calling this primitive.
    let next = rooted.retry_source_run_blocking().unwrap();
    let next_source = SourceRoot::from_rooted(next.clone()).unwrap();
    next_source
        .entries(request)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let relative = RelativePath::new("link").unwrap();
    let mut reader = next.open_source_following_blocking(&relative).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"fresh CHILD");
    assert!(rooted
        .validate_source_observation_blocking(&relative)
        .is_err());
}

#[test]
fn sibling_literal_is_one_raw_component_without_normalization() {
    for bytes in [b"raw-\xff".as_slice(), b"file", b"..name"] {
        assert_eq!(
            sibling_name(Path::new(OsStr::from_bytes(bytes))),
            Some(OsStr::from_bytes(bytes))
        );
    }
    for bytes in [
        b"".as_slice(),
        b".",
        b"..",
        b"./file",
        b"file/",
        b"../file",
        b"/file",
        b"dir/../file",
        b"nul\0name",
    ] {
        assert!(sibling_name(Path::new(OsStr::from_bytes(bytes))).is_none());
    }
}

#[tokio::test]
async fn sibling_discovery_and_enumerated_target_must_agree_in_either_order() {
    for target_first in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("target"), b"A").unwrap();
        std::os::unix::fs::symlink("target", root.path().join("link")).unwrap();
        let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
        let rooted = source.rooted();
        let attempt = rooted.start_source_scan_blocking().unwrap();
        let names = if target_first {
            ["target", "link"]
        } else {
            ["link", "target"]
        };
        rooted
            .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new(names[0]), true)
            .unwrap();
        std::fs::write(root.path().join("target"), b"B-version").unwrap();
        rooted
            .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new(names[1]), true)
            .unwrap();
        rooted
            .complete_source_directory_blocking(Path::new(""))
            .unwrap();
        attempt.finish(true, || true).unwrap();
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        // Authentic records and exact physical enumeration cannot repair
        // contradictory original DATA observations into a closed TREE proof.
        assert_eq!(inventory.record_count(), 3);
        assert!(!inventory.excludes_objects_blocking(&[]).unwrap());
        assert!(matches!(
            inventory.validate_originals_blocking(),
            Err(RootedFsError::SourceBindingChanged(_))
        ));
    }
}

#[tokio::test]
async fn sibling_retyped_before_data_or_metadata_is_a_retryable_child_race() {
    for metadata in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("target"), b"original").unwrap();
        std::fs::write(outside.path().join("data"), b"outside").unwrap();
        std::os::unix::fs::symlink("target", root.path().join("link")).unwrap();
        let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
        let rooted = source.rooted();
        let request = crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        source
            .entries(request)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let (reached, waiting) = tokio::sync::oneshot::channel();
        let (resume, receive) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceBinding,
                reached,
                resume: receive,
            },
        );
        let opening = rooted.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let relative = RelativePath::new("link").unwrap();
            if metadata {
                opening
                    .source_entry_metadata_blocking(&relative, request)
                    .map(drop)
            } else {
                opening.open_source_following_blocking(&relative).map(drop)
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        std::fs::rename(root.path().join("target"), root.path().join("held")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("data"), root.path().join("target"))
            .unwrap();
        resume.send(()).unwrap();
        assert!(
            matches!(worker.await.unwrap(), Err(RootedFsError::SourceBindingChanged(path)) if path == Path::new("link"))
        );
        assert_eq!(
            std::fs::read(outside.path().join("data")).unwrap(),
            b"outside"
        );
        std::fs::remove_file(root.path().join("target")).unwrap();
        std::fs::rename(root.path().join("held"), root.path().join("target")).unwrap();
        let next = rooted.retry_source_run_blocking().unwrap();
        let source = SourceRoot::from_rooted(next.clone()).unwrap();
        source
            .entries(request)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        next.validate_source_observation_blocking(&RelativePath::new("link").unwrap())
            .unwrap();
    }
}
