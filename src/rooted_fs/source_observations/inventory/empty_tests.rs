use super::*;
use futures::TryStreamExt;
use std::os::unix::fs::PermissionsExt;

async fn scan(source: &crate::endpoint::source_root::SourceRoot, depth: usize) -> SourceInventory {
    source
        .entries(crate::engine::scan::ScanRequest {
            max_depth: Some(depth),
            ..Default::default()
        })
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    source.rooted().seal_source_inventory_blocking().unwrap()
}

#[tokio::test]
async fn native_empty_root_and_unreadable_pruned_directory_need_no_enumeration() {
    for root_only in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("source");
        std::fs::create_dir(&path).unwrap();
        let opaque = if root_only {
            path.clone()
        } else {
            path.join("opaque")
        };
        if !root_only {
            std::fs::create_dir(&opaque).unwrap();
            std::fs::set_permissions(&opaque, std::fs::Permissions::from_mode(0o0)).unwrap();
        }
        let source = crate::endpoint::source_root::SourceRoot::open(path)
            .await
            .unwrap();
        let inventory = scan(&source, usize::from(!root_only)).await;
        let candidate = File::create(fixture.path().join("independent")).unwrap();
        let candidate = object_key(candidate.as_raw_fd(), None).unwrap().unwrap();
        assert!(inventory.excludes_objects_blocking(&[candidate]).unwrap());
        let relative = if root_only {
            Path::new("")
        } else {
            Path::new("opaque")
        };
        let key = path_key(relative);
        let protected = {
            let mut owner = inventory.rooted.source_observations.lock().unwrap();
            let c = owner.active().unwrap();
            assert_eq!(
                c.disposition(key).unwrap(),
                SourceDirectoryDisposition::Opaque
            );
            assert_eq!(c.child_count(key).unwrap(), 0);
            assert!(c.empty_directory_stamp(key).unwrap().is_some());
            c.require(relative).unwrap().object_key.unwrap()
        };
        assert!(!inventory.excludes_objects_blocking(&[protected]).unwrap());
        std::fs::set_permissions(&opaque, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[tokio::test]
async fn empty_certificate_rejects_late_aliases_and_empty_again_directories() {
    for remove_alias in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("source");
        let opaque = path.join("opaque");
        std::fs::create_dir_all(&opaque).unwrap();
        let candidate_path = fixture.path().join("independent");
        std::fs::write(&candidate_path, b"unaltered").unwrap();
        let file = File::open(&candidate_path).unwrap();
        let candidate = object_key(file.as_raw_fd(), None).unwrap().unwrap();
        let source = crate::endpoint::source_root::SourceRoot::open(path)
            .await
            .unwrap();
        let inventory = scan(&source, 1).await;
        assert!(inventory.excludes_objects_blocking(&[candidate]).unwrap());
        std::fs::hard_link(&candidate_path, opaque.join("late-source-alias")).unwrap();
        if remove_alias {
            std::fs::remove_file(opaque.join("late-source-alias")).unwrap();
        }
        assert!(matches!(inventory.excludes_objects_blocking(&[candidate]),
            Err(RootedFsError::SourceBindingChanged(path)) if path == Path::new("opaque")));
        assert_eq!(std::fs::read(&candidate_path).unwrap(), b"unaltered");
    }
}

#[tokio::test]
async fn replay_cannot_close_originally_nonempty_directory_but_new_run_can() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("source");
    let opaque = path.join("opaque");
    std::fs::create_dir_all(&opaque).unwrap();
    std::fs::write(opaque.join("original-hidden"), b"SOURCE").unwrap();
    std::fs::set_permissions(&opaque, std::fs::Permissions::from_mode(0o111)).unwrap();
    let file = File::create(fixture.path().join("independent")).unwrap();
    let candidate = object_key(file.as_raw_fd(), None).unwrap().unwrap();
    let source = crate::endpoint::source_root::SourceRoot::open(path)
        .await
        .unwrap();
    let inventory = scan(&source, 1).await;
    assert!(!inventory.excludes_objects_blocking(&[candidate]).unwrap());
    std::fs::set_permissions(&opaque, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_file(opaque.join("original-hidden")).unwrap();
    let replayed = scan(&source, 1).await;
    assert!(!replayed.excludes_objects_blocking(&[candidate]).unwrap());
    let next = source.rooted().next_source_run_blocking().unwrap();
    let next = crate::endpoint::source_root::SourceRoot::from_rooted(next).unwrap();
    assert!(scan(&next, 1)
        .await
        .excludes_objects_blocking(&[candidate])
        .unwrap());
}
