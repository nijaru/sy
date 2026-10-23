use super::*;
use crate::endpoint::source_root::SourceRoot;
use futures::TryStreamExt;

#[test]
fn sibling_chain_bounds_cycles_and_unsupported_targets() {
    for length in [1, MAX_FOLLOW_LINKS, MAX_FOLLOW_LINKS + 1] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("data"), b"DATA").unwrap();
        for index in 0..length {
            let target = if index + 1 == length {
                "data".to_string()
            } else {
                format!("link-{}", index + 1)
            };
            std::os::unix::fs::symlink(target, root.path().join(format!("link-{index}"))).unwrap();
        }
        let parent = File::open(root.path()).unwrap();
        let result = inspect(parent.as_raw_fd(), Path::new("link-0"), true);
        if length <= MAX_FOLLOW_LINKS {
            let observation = result.unwrap();
            assert_eq!(observation.resolution.len(), length);
            assert_eq!(
                observation.resolution.last().unwrap().name,
                OsStr::new("data")
            );
            assert!(observation.matches(&Observation::decode(&observation.encode()).unwrap(), true));
        } else {
            assert!(matches!(
                result,
                Err(RootedFsError::SourceFollowUnsupported(_))
            ));
        }
    }
    for target in [
        "first",
        "middle",
        "./first",
        "./middle",
        "./../data",
        "../data",
        "/data",
        "directory",
        "missing",
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("data"), b"DATA").unwrap();
        std::fs::create_dir(root.path().join("directory")).unwrap();
        std::os::unix::fs::symlink("middle", root.path().join("first")).unwrap();
        std::os::unix::fs::symlink(target, root.path().join("middle")).unwrap();
        let parent = File::open(root.path()).unwrap();
        assert!(matches!(
            inspect(parent.as_raw_fd(), Path::new("first"), true),
            Err(RootedFsError::SourceFollowUnsupported(path)) if path == Path::new("first")
        ));
    }
}

#[test]
fn original_and_resolution_link_literals_compare_exact_raw_bytes() {
    for (literal, different) in [
        ("./data", "data"),
        ("dir/./data", "dir/data"),
        ("dir//data", "dir/data"),
    ] {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(literal, root.path().join("link")).unwrap();
        let parent = File::open(root.path()).unwrap();
        let original = inspect(parent.as_raw_fd(), Path::new("link"), false).unwrap();
        let mut other = Observation::decode(&original.encode()).unwrap();
        assert_eq!(
            other.target.as_ref().unwrap().as_os_str(),
            OsStr::new(literal)
        );
        other.target = Some(LinkTarget::new(OsStr::new(different)));
        assert!(
            !original.matches(&other, true),
            "native stat equality cannot erase literal spelling"
        );
        let step = ResolutionStep {
            name: original.name,
            named: original.named,
            object_key: original.object_key,
            target: original.target,
        };
        let mut bytes = Vec::new();
        step.encode(&mut bytes);
        let mut changed = ResolutionStep::decode(&mut bytes.as_slice()).unwrap();
        changed.target = other.target;
        assert!(!same_resolution(&[step], &[changed]));
    }
}

#[test]
fn observed_link_retyped_between_stat_and_readlink_is_a_source_race() {
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("data", root.path().join("middle")).unwrap();
    let parent = File::open(root.path()).unwrap();
    let name = OsStr::new("middle");
    assert_eq!(
        stat_at_no_follow(parent.as_raw_fd(), name).unwrap().st_mode & libc::S_IFMT,
        libc::S_IFLNK
    );
    // External native retyping in the exact stat/readlink window. No SOURCE
    // contents were read and the kernel must not follow the replacement.
    std::fs::rename(root.path().join(name), root.path().join("held-link")).unwrap();
    std::fs::write(root.path().join(name), b"replacement").unwrap();
    assert!(matches!(target_at(parent.as_raw_fd(), name),
        Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::EINVAL)));
    assert!(
        matches!(observed_target_at(parent.as_raw_fd(), name, Path::new("first")),
        Err(RootedFsError::SourceBindingChanged(path)) if path == Path::new("first"))
    );
}

#[test]
fn sibling_chain_codec_rejects_truncation_unbounded_and_disconnected_hops() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("data"), b"DATA").unwrap();
    std::os::unix::fs::symlink("middle", root.path().join("first")).unwrap();
    std::os::unix::fs::symlink("data", root.path().join("middle")).unwrap();
    let parent = File::open(root.path()).unwrap();
    let mut observation = inspect(parent.as_raw_fd(), Path::new("first"), true).unwrap();
    let bytes = observation.encode();
    for length in 0..bytes.len() {
        assert!(Observation::decode(&bytes[..length]).is_err());
    }
    let mut steps = Vec::new();
    for step in &observation.resolution {
        step.encode(&mut steps);
    }
    let count_offset = bytes.len() - steps.len() - 4;
    let mut oversized = bytes.clone();
    oversized[count_offset..count_offset + 4]
        .copy_from_slice(&((MAX_FOLLOW_STEPS + 1) as u32).to_le_bytes());
    assert!(Observation::decode(&oversized).is_err());
    observation.resolution[0].name = OsString::from("foreign");
    assert!(Observation::decode(&observation.encode()).is_err());
    observation.resolution[0].name = OsString::from("middle");
    observation.resolution[0].target = Some(LinkTarget::new(OsStr::new("first")));
    assert!(Observation::decode(&observation.encode()).is_err());
    observation.resolution[0].target = Some(LinkTarget::new(OsStr::new(&"x".repeat(MAX_TARGET))));
    assert!(
        Observation::decode(&observation.encode()).is_err(),
        "the aggregate transcript byte budget applies independently of hop count"
    );
}

#[tokio::test]
async fn sibling_chain_retarget_between_same_inode_names_invalidates_data_and_metadata() {
    for metadata in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("data"), b"DATA").unwrap();
        std::fs::hard_link(root.path().join("data"), root.path().join("same-inode")).unwrap();
        std::os::unix::fs::symlink("middle", root.path().join("first")).unwrap();
        std::os::unix::fs::symlink("data", root.path().join("middle")).unwrap();
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
        std::fs::remove_file(root.path().join("middle")).unwrap();
        std::os::unix::fs::symlink("same-inode", root.path().join("middle")).unwrap();
        let path = RelativePath::new("first").unwrap();
        let result = if metadata {
            rooted
                .source_entry_metadata_blocking(&path, request)
                .map(drop)
        } else {
            rooted.open_source_following_blocking(&path).map(drop)
        };
        assert!(matches!(
            result,
            Err(RootedFsError::SourceBindingChanged(path)) if path == Path::new("first")
        ));
        assert_eq!(std::fs::read(root.path().join("data")).unwrap(), b"DATA");
        assert!(matches!(
            rooted.validate_source_observation_blocking(&path),
            Err(RootedFsError::SourceRaceCutoff)
        ));
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sibling_chain_discovery_must_match_physical_intermediate_in_either_order() {
    for first_before_middle in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("data"), b"DATA").unwrap();
        std::fs::hard_link(root.path().join("data"), root.path().join("same-inode")).unwrap();
        std::os::unix::fs::symlink("middle", root.path().join("first")).unwrap();
        std::os::unix::fs::symlink("data", root.path().join("middle")).unwrap();
        let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
        let rooted = source.rooted();
        let attempt = rooted.start_source_scan_blocking().unwrap();
        let names = if first_before_middle {
            ["first", "middle"]
        } else {
            ["middle", "first"]
        };
        rooted
            .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new(names[0]), true)
            .unwrap();
        std::fs::remove_file(root.path().join("middle")).unwrap();
        std::os::unix::fs::symlink("same-inode", root.path().join("middle")).unwrap();
        rooted
            .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new(names[1]), true)
            .unwrap();
        for name in ["data", "same-inode"] {
            rooted
                .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new(name), true)
                .unwrap();
        }
        rooted
            .complete_source_directory_blocking(Path::new(""))
            .unwrap();
        attempt.finish(true).unwrap();
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        assert_eq!(inventory.record_count(), 5);
        assert!(
            !inventory.excludes_objects_blocking(&[]).unwrap(),
            "equal DATA does not reconcile contradictory intermediate LINK originals"
        );
        assert!(matches!(
            inventory.validate_originals_blocking(),
            Err(RootedFsError::SourceBindingChanged(_))
        ));
    }
}

#[tokio::test]
async fn sibling_chain_spilled_hops_are_authenticated_before_replay() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("data"), b"DATA").unwrap();
    std::os::unix::fs::symlink("middle", root.path().join("first")).unwrap();
    std::os::unix::fs::symlink("data", root.path().join("middle")).unwrap();
    let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
    let rooted = source.rooted();
    source
        .entries(crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        })
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    {
        let mut owner = rooted.source_observations.lock().unwrap();
        let c = owner.active().unwrap();
        let locator = c
            .index
            .get_bytes(path_key(Path::new("first")))
            .unwrap()
            .unwrap();
        let offset = u64::from_le_bytes(locator[..8].try_into().unwrap());
        let observation = c.require(Path::new("first")).unwrap();
        let bytes = observation.encode();
        let mut steps = Vec::new();
        for step in &observation.resolution {
            step.encode(&mut steps);
        }
        let start = bytes.len() - steps.len();
        let mut first = Vec::new();
        observation.resolution[0].encode(&mut first);
        let literal = start + first.len() - 4;
        assert_eq!(&bytes[literal..literal + 4], b"data");
        c.file
            .seek(SeekFrom::Start(offset + literal as u64))
            .unwrap();
        c.file.write_all(b"fake").unwrap();
    }
    let path = RelativePath::new("first").unwrap();
    assert!(rooted.open_source_following_blocking(&path).is_err());
    assert!(matches!(
        rooted.open_source_following_blocking(&path),
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
}
