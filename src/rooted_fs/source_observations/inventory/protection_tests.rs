use super::*;
use crate::endpoint::source_root::SourceRoot;
use futures::TryStreamExt;
use std::os::unix::fs::PermissionsExt;

async fn scan(
    source: &SourceRoot,
    local: bool,
) -> std::result::Result<Vec<crate::engine::domain::Entry>, crate::engine::reconcile::BoxError> {
    let request = crate::engine::scan::ScanRequest {
        max_depth: Some(1),
        ..Default::default()
    };
    if local {
        source.entries(request).try_collect().await
    } else {
        source
            .rooted()
            .source_entry_stream(request)
            .try_collect()
            .await
    }
}

#[tokio::test]
async fn protected_metadata_stays_readless_unfollowed_and_outside_sync_selection() {
    for local in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let directory = fixture.path().join("pruned/deep");
        std::fs::create_dir_all(&directory).unwrap();
        let unreadable = directory.join("file");
        std::fs::write(&unreadable, b"SOURCE").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::os::unix::fs::symlink("loop", directory.join("loop")).unwrap();
        let source = SourceRoot::open(fixture.path().to_path_buf())
            .await
            .unwrap();
        let entries = scan(&source, local).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.as_path(), Path::new("pruned"));
        let inventory = source.rooted().seal_source_inventory_blocking().unwrap();
        assert_eq!(inventory.record_count(), 5);
        inventory.validate_originals_blocking().unwrap();
        {
            let rooted = source.rooted();
            let mut owner = rooted.source_observations.lock().unwrap();
            let c = owner.active().unwrap();
            let link = c.require(Path::new("pruned/deep/loop")).unwrap();
            assert!(!link.followed);
            assert_eq!(link.named.kind, libc::S_IFLNK as u64);
            assert_eq!(link.target.unwrap().as_os_str(), OsStr::new("loop"));
            assert_eq!(c.protected_count, 3);
            assert_eq!(
                c.disposition(path_key(Path::new("pruned"))).unwrap(),
                SourceDirectoryDisposition::Protected
            );
        }
        assert_eq!(scan(&source, local).await.unwrap().len(), 1);
        assert_eq!(
            source
                .rooted()
                .seal_source_inventory_blocking()
                .unwrap()
                .record_count(),
            5
        );
        inventory.validate_originals_blocking().unwrap();
        std::fs::set_permissions(unreadable, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[tokio::test]
async fn protected_metadata_replay_rejects_arrivals_missing_and_replaced_names() {
    for local in [false, true] {
        for change in ["arrival", "missing", "replacement"] {
            let fixture = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(fixture.path().join("pruned/deep")).unwrap();
            let original = fixture.path().join("pruned/deep/file");
            std::fs::write(&original, b"SOURCE").unwrap();
            let source = SourceRoot::open(fixture.path().to_path_buf())
                .await
                .unwrap();
            scan(&source, local).await.unwrap();
            let inventory = source.rooted().seal_source_inventory_blocking().unwrap();
            match change {
                "arrival" => {
                    std::fs::write(fixture.path().join("pruned/deep/arrival"), b"foreign").unwrap()
                }
                "missing" => std::fs::remove_file(&original).unwrap(),
                _ => {
                    std::fs::rename(&original, fixture.path().join("displaced")).unwrap();
                    std::fs::write(&original, b"SOURCE").unwrap();
                }
            }
            assert!(
                scan(&source, local).await.is_err(),
                "{change} must not be adopted"
            );
            assert!(matches!(
                inventory.validate_originals_blocking(),
                Err(RootedFsError::SourceRaceCutoff)
            ));
        }
    }
}

#[tokio::test]
async fn closing_during_metadata_census_stops_and_cannot_seal_partial_protection() {
    let fixture = tempfile::tempdir().unwrap();
    for name in 0..64 {
        std::fs::write(fixture.path().join(format!("file{name}")), b"SOURCE").unwrap();
    }
    let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    // A depth-zero producer has no sync entries. Its metadata phase still
    // must observe receiver closure rather than scan an entire hidden tree.
    let attempt = rooted.start_source_scan_blocking().unwrap();
    let active = std::sync::atomic::AtomicUsize::new(0);
    attempt
        .finish(true, || active.fetch_add(1, Ordering::Relaxed) < 8)
        .unwrap();
    let mut owner = rooted.source_observations.lock().unwrap();
    let CatalogueState::Active(catalogue) = &mut *owner else {
        panic!("retained original catalogue")
    };
    assert!(catalogue.count > 1 && catalogue.count < 65);
    assert!(!catalogue.completed);
    drop(owner);
    assert!(matches!(
        rooted.seal_source_inventory_blocking(),
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn moved_original_deep_member_remains_protected_with_unchanged_scope_roots() {
    for local in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("source");
        std::fs::create_dir_all(root.join("pruned/deep")).unwrap();
        let original = root.join("pruned/deep/file");
        std::fs::write(&original, b"SOURCE").unwrap();
        let file = File::open(&original).unwrap();
        let candidate = object_key(file.as_raw_fd(), None).unwrap().unwrap();
        let source = SourceRoot::open(root.clone()).await.unwrap();
        scan(&source, local).await.unwrap();
        let inventory = source.rooted().seal_source_inventory_blocking().unwrap();
        assert!(!inventory.excludes_objects_blocking(&[candidate]).unwrap());
        let before = [
            stat_fd(source.rooted().root_fd.as_raw_fd()).unwrap(),
            stat_at_no_follow(source.rooted().root_fd.as_raw_fd(), OsStr::new("pruned")).unwrap(),
        ]
        .map(|stat| NativeObservation::from_stat(&stat).unwrap());
        let moved = fixture.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        let after = [
            stat_fd(source.rooted().root_fd.as_raw_fd()).unwrap(),
            stat_at_no_follow(source.rooted().root_fd.as_raw_fd(), OsStr::new("pruned")).unwrap(),
        ]
        .map(|stat| NativeObservation::from_stat(&stat).unwrap());
        assert!(
            before == after,
            "deep move leaves both considered scope roots unchanged"
        );
        assert!(
            matches!(inventory.validate_originals_blocking(), Err(RootedFsError::SourceBindingChanged(path)) if path == Path::new("pruned/deep/file"))
        );
        assert_eq!(std::fs::read(moved).unwrap(), b"SOURCE");
    }
}
