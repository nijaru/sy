use super::*;
use crate::endpoint::source_root::SourceRoot;
use futures::TryStreamExt;

fn request() -> crate::engine::scan::ScanRequest {
    crate::engine::scan::ScanRequest {
        follow_symlinks: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn absolute_resets_keep_original_physical_members_and_native_readers() {
    for native_address in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("dir/deeper")).unwrap();
        std::fs::write(fixture.path().join("dir/deeper/data"), b"original DATA").unwrap();
        // The held remote-style root has no local operator-address transcript.
        // Its FD-derived pathname must be natively qualified, not trusted.
        let (rooted, source) = if native_address {
            (
                RootedFs::open(fixture.path().to_path_buf()).await.unwrap(),
                None,
            )
        } else {
            let source = SourceRoot::open(fixture.path().to_path_buf())
                .await
                .unwrap();
            (source.rooted(), Some(source))
        };
        let address = if native_address {
            fixture.path().canonicalize().unwrap()
        } else {
            fixture.path().to_path_buf()
        };
        let entry_target = format!("{}/.//dir///bridge", address.display());
        let bridge_target = format!("{}/dir/deeper/data", address.display());
        std::os::unix::fs::symlink(&entry_target, fixture.path().join("entry")).unwrap();
        std::os::unix::fs::symlink(&bridge_target, fixture.path().join("dir/bridge")).unwrap();
        let entries = match source {
            Some(source) => source.entries(request()),
            None => rooted.source_entry_stream(request()),
        }
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
        assert_eq!(entries.len(), 5);
        for name in ["entry", "dir/bridge", "dir/deeper/data"] {
            let path = RelativePath::new(name).unwrap();
            let entry = entries.iter().find(|entry| entry.path == path).unwrap();
            let mut file = rooted.open_source_following_blocking(&path).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"original DATA");
            assert_eq!(
                rooted
                    .source_entry_metadata_blocking(&path, request())
                    .unwrap()
                    .identity,
                entry.identity
            );
        }
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        assert_eq!(
            inventory.record_count(),
            6,
            "ROOT resets cannot invent physical members"
        );
        inventory.validate_originals_blocking().unwrap();
        assert!(inventory.excludes_objects_blocking(&[]).unwrap());
        let protected = object_key(rooted.root_fd.as_raw_fd(), None)
            .unwrap()
            .unwrap();
        assert!(!inventory.excludes_objects_blocking(&[protected]).unwrap());
        let mut owner = rooted.source_observations.lock().unwrap();
        let original = owner.active().unwrap().require(Path::new("entry")).unwrap();
        assert_eq!(
            original.target.unwrap().as_os_str(),
            OsStr::new(&entry_target)
        );
        assert_eq!(
            original
                .resolution
                .iter()
                .filter(|step| matches!(step, ResolutionStep::Root(_)))
                .count(),
            2
        );
    }
}

#[tokio::test]
async fn absolute_reset_codec_requires_each_literal_root_boundary_and_native_view() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("data"), b"DATA").unwrap();
    std::os::unix::fs::symlink(fixture.path().join("data"), fixture.path().join("entry")).unwrap();
    let source = SourceRoot::open(fixture.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let rooted = source.rooted();
    let mut owner = rooted.source_observations.lock().unwrap();
    let original = owner.active().unwrap().require(Path::new("entry")).unwrap();
    let encoded = original.encode();
    for length in 0..encoded.len() {
        assert!(Observation::decode(&encoded[..length]).is_err());
    }
    assert!(original.matches(&Observation::decode(&encoded).unwrap(), true));
    for mutation in 0..5 {
        let mut changed = Observation::decode(&encoded).unwrap();
        if mutation == 0 {
            changed.resolution.remove(0);
        } else {
            let ResolutionStep::Root(reset) = &mut changed.resolution[0] else {
                panic!("missing reset");
            };
            match mutation {
                1 => reset.address = LinkTarget::new(OsStr::new("/unrelated-root")),
                2 => reset.named.kind = libc::S_IFREG as u64,
                3 => reset.object_key.0 = [0; 32],
                _ => changed.target = Some(LinkTarget::new(OsStr::new("data"))),
            }
        }
        assert!(Observation::decode(&changed.encode()).is_err());
    }
    for bytes in [vec![2], [vec![1], u32::MAX.to_le_bytes().to_vec()].concat()] {
        assert!(ResolutionStep::decode(&mut bytes.as_slice()).is_err());
    }
}

#[tokio::test]
async fn absolute_targets_cannot_escape_reenter_or_follow_directory_aliases() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("root");
    let outside = fixture.path().join("outside");
    let prefix_collision = fixture.path().join("root-other");
    for path in [&root, &outside, &prefix_collision] {
        std::fs::create_dir(path).unwrap();
    }
    for path in [&root, &outside, &prefix_collision] {
        std::fs::write(path.join("data"), b"unchanged").unwrap();
    }
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    for target in [
        outside.join("data"),
        prefix_collision.join("data"),
        root.join("../outside/data"),
        root.join("../root/data"),
        outside.join("../root/data"),
        root.join("escape/data"),
    ] {
        std::os::unix::fs::symlink(&target, root.join("entry")).unwrap();
        let source = SourceRoot::open(root.clone()).await.unwrap();
        let rooted = source.rooted();
        let _attempt = rooted.start_source_scan_blocking().unwrap();
        assert!(
            matches!(rooted.record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new("entry"), true),
            Err(RootedFsError::SourceFollowUnsupported(path)) if path == Path::new("entry"))
        );
        std::fs::remove_file(root.join("entry")).unwrap();
    }
    assert_eq!(std::fs::read(outside.join("data")).unwrap(), b"unchanged");
    assert_eq!(
        std::fs::read(prefix_collision.join("data")).unwrap(),
        b"unchanged"
    );
}

#[tokio::test]
async fn absolute_discovery_does_not_close_a_pruned_physical_directory() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("dir")).unwrap();
    std::fs::write(fixture.path().join("dir/data"), b"DATA").unwrap();
    std::os::unix::fs::symlink(
        fixture.path().join("dir/data"),
        fixture.path().join("entry"),
    )
    .unwrap();
    let source = SourceRoot::open(fixture.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(request().shallow(1))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let inventory = source.rooted().seal_source_inventory_blocking().unwrap();
    assert_eq!(inventory.record_count(), 3);
    assert!(!inventory.excludes_objects_blocking(&[]).unwrap());
}

#[tokio::test]
async fn absolute_data_change_is_retryable_without_replacing_original_root_authority() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("data"), b"original DATA").unwrap();
    std::os::unix::fs::symlink(fixture.path().join("data"), fixture.path().join("entry")).unwrap();
    let source = SourceRoot::open(fixture.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let rooted = source.rooted();
    std::fs::write(fixture.path().join("data"), b"new DATA").unwrap();
    let path = RelativePath::new("entry").unwrap();
    assert!(matches!(
        rooted.open_source_following_blocking(&path),
        Err(RootedFsError::SourceBindingChanged(_))
    ));
    let next = rooted.retry_source_run_blocking().unwrap();
    let source = SourceRoot::from_rooted(next.clone()).unwrap();
    source
        .entries(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let mut file = next.open_source_following_blocking(&path).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"new DATA");
    assert!(rooted.validate_source_observation_blocking(&path).is_err());
}

#[tokio::test]
async fn absolute_qualification_replays_original_ancestors_and_keeps_failure_sticky() {
    let fixture = tempfile::tempdir().unwrap();
    let ancestor = fixture.path().join("ancestor");
    let root = ancestor.join("parent/root");
    std::fs::create_dir_all(&root).unwrap();
    let source = SourceRoot::open(root.clone()).await.unwrap();
    let rooted = source.rooted();
    let address = LinkTarget::new(root.as_os_str());
    let parked = fixture.path().join("parked");
    std::fs::rename(&ancestor, &parked).unwrap();
    std::fs::create_dir(&ancestor).unwrap();
    std::fs::rename(parked.join("parent"), ancestor.join("parent")).unwrap();
    {
        let mut owner = rooted.source_observations.lock().unwrap();
        assert!(matches!(
            owner
                .active()
                .unwrap()
                .qualify_absolute_root(&rooted, &address, Path::new("entry")),
            Err(RootedFsError::RootChanged(_))
        ));
    }
    std::fs::rename(ancestor.join("parent"), parked.join("parent")).unwrap();
    std::fs::remove_dir(&ancestor).unwrap();
    std::fs::rename(&parked, &ancestor).unwrap();
    assert!(matches!(
        rooted.retry_source_run_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
}

#[tokio::test]
async fn absolute_qualification_cannot_adopt_a_fresh_native_view_over_the_original() {
    use std::os::unix::fs::FileExt;
    let fixture = tempfile::tempdir().unwrap();
    let source = SourceRoot::open(fixture.path().to_path_buf())
        .await
        .unwrap();
    let rooted = source.rooted();
    let mut owner = rooted.source_observations.lock().unwrap();
    let c = owner.active().unwrap();
    let mut original = c.require(Path::new("")).unwrap();
    // Inject an authenticated but different originally observed mount view.
    // Device/inode/type and the current native key still agree with each other;
    // fresh equality must never replace the original authority.
    original.object_key.as_mut().unwrap().0[16] ^= 1;
    let bytes = original.encode();
    let mut locator = c.index.get_bytes([0; 32]).unwrap().unwrap();
    assert_eq!(
        bytes.len() as u64,
        u64::from_le_bytes(locator[8..16].try_into().unwrap())
    );
    c.file
        .write_all_at(&bytes, u64::from_le_bytes(locator[..8].try_into().unwrap()))
        .unwrap();
    locator[16..].copy_from_slice(blake3::hash(&bytes).as_bytes());
    c.index.set_bytes([0; 32], locator).unwrap();
    let address = LinkTarget::new(fixture.path().as_os_str());
    assert!(matches!(
        c.qualify_absolute_root(&rooted, &address, Path::new("entry")),
        Err(RootedFsError::RootChanged(_))
    ));
}

#[tokio::test]
async fn native_root_relocation_preserves_relative_reads_not_obsolete_absolute_addresses() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("root");
    let moved = fixture.path().join("moved");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("data"), b"original DATA").unwrap();
    std::os::unix::fs::symlink(
        root.canonicalize().unwrap().join("data"),
        root.join("absolute"),
    )
    .unwrap();
    std::os::unix::fs::symlink("data", root.join("relative")).unwrap();
    let rooted = RootedFs::open(root.clone()).await.unwrap();
    rooted
        .source_entry_stream(request())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("data"), b"outside replacement").unwrap();
    let mut reader = rooted
        .open_source_following_blocking(&RelativePath::new("relative").unwrap())
        .unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"original DATA");
    assert!(matches!(
        rooted.open_source_following_blocking(&RelativePath::new("absolute").unwrap()),
        Err(RootedFsError::SourceBindingChanged(_))
    ));
    assert_eq!(
        std::fs::read(root.join("data")).unwrap(),
        b"outside replacement"
    );
}
