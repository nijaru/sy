use super::*;
use crate::endpoint::source_root::SourceRoot;
use futures::TryStreamExt;

fn request() -> crate::engine::scan::ScanRequest {
    crate::engine::scan::ScanRequest {
        follow_symlinks: true,
        ..Default::default()
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn forward_file_and_nested_chain_keep_exact_physical_membership() {
    for (target, bridge, dots) in [
        ("dir/bridge", "deeper/data", 0),
        ("./dir/./bridge", "./deeper/./data", 4),
        (".//dir///.//bridge", ".//deeper///.//data", 4),
    ] {
        for rooted_scanner in [false, true] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("dir/deeper")).unwrap();
            std::fs::write(root.path().join("dir/deeper/data"), b"original DATA").unwrap();
            std::os::unix::fs::symlink(target, root.path().join("entry")).unwrap();
            std::os::unix::fs::symlink(bridge, root.path().join("dir/bridge")).unwrap();
            let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
            let rooted = source.rooted();
            let entries = if rooted_scanner {
                rooted.source_entry_stream(request())
            } else {
                source.entries(request())
            }
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
            assert_eq!(entries.len(), 5);
            for name in ["entry", "dir/bridge", "dir/deeper/data"] {
                let entry = entries
                    .iter()
                    .find(|entry| entry.path.as_path() == Path::new(name))
                    .unwrap();
                assert!(entry.is_file());
                assert_eq!(entry.size, b"original DATA".len() as u64);
                let mut file = rooted.open_source_following_blocking(&entry.path).unwrap();
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"original DATA");
                let metadata = rooted
                    .source_entry_metadata_blocking(&entry.path, request())
                    .unwrap();
                assert_eq!(metadata.identity, entry.identity);
            }
            let inventory = rooted.seal_source_inventory_blocking().unwrap();
            assert_eq!(
                inventory.record_count(),
                6,
                "target resolution must not invent physical members"
            );
            {
                let mut owner = rooted.source_observations.lock().unwrap();
                let original = owner.active().unwrap().require(Path::new("entry")).unwrap();
                assert_eq!(original.target.unwrap().as_os_str(), OsStr::new(target));
                assert_eq!(
                    original
                        .resolution
                        .iter()
                        .filter(|step| matches!(step, ResolutionStep::Entry(entry) if entry.name == OsStr::new(".")))
                        .count(),
                    dots
                );
            }
            inventory.validate_originals_blocking().unwrap();
            assert!(inventory
                .excludes_objects_blocking(&[], SourceExclusion::Objects)
                .unwrap());
            for name in [
                "entry",
                "dir",
                "dir/bridge",
                "dir/deeper",
                "dir/deeper/data",
            ] {
                let path = RelativePath::new(name).unwrap();
                let (parent, leaf) = rooted.open_parent_blocking(path.as_path()).unwrap();
                let protected = object_key(parent.as_raw_fd(), Some(&leaf))
                    .unwrap()
                    .unwrap();
                assert!(!inventory
                    .excludes_objects_blocking(&[protected], SourceExclusion::Objects)
                    .unwrap());
            }
            assert!(matches!(
                rooted.check_source_removal_support_blocking(&RelativePath::new("entry").unwrap()),
                Err(RootedFsError::SourceFollowRemovalUnsupported(_))
            ));
        }
    }
}

#[tokio::test]
async fn forward_target_directory_substitution_is_a_child_race_before_data_or_metadata() {
    for metadata in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::create_dir(foreign.path().join("replacement")).unwrap();
        std::fs::write(root.path().join("dir/data"), b"DATA").unwrap();
        std::fs::hard_link(
            root.path().join("dir/data"),
            foreign.path().join("replacement/data"),
        )
        .unwrap();
        std::os::unix::fs::symlink("dir/data", root.path().join("entry")).unwrap();
        let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
        let rooted = source.rooted();
        source
            .entries(request())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        std::fs::rename(root.path().join("dir"), foreign.path().join("held")).unwrap();
        std::fs::rename(foreign.path().join("replacement"), root.path().join("dir")).unwrap();
        let path = RelativePath::new("entry").unwrap();
        let result = if metadata {
            rooted
                .source_entry_metadata_blocking(&path, request())
                .map(drop)
        } else {
            rooted.open_source_following_blocking(&path).map(drop)
        };
        assert!(
            matches!(result, Err(RootedFsError::SourceBindingChanged(path)) if path == Path::new("entry"))
        );
        std::fs::rename(root.path().join("dir"), foreign.path().join("replacement")).unwrap();
        std::fs::rename(foreign.path().join("held"), root.path().join("dir")).unwrap();
        let next = rooted.retry_source_run_blocking().unwrap();
        let source = SourceRoot::from_rooted(next.clone()).unwrap();
        source
            .entries(request())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        next.validate_source_observation_blocking(&path).unwrap();
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn forward_directory_discovery_and_physical_originals_must_agree_in_either_order() {
    for directory_first in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::create_dir(foreign.path().join("replacement")).unwrap();
        std::fs::write(root.path().join("dir/data"), b"DATA").unwrap();
        std::fs::hard_link(
            root.path().join("dir/data"),
            foreign.path().join("replacement/data"),
        )
        .unwrap();
        std::os::unix::fs::symlink("dir/data", root.path().join("entry")).unwrap();
        let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
        let rooted = source.rooted();
        let attempt = rooted.start_source_scan_blocking().unwrap();
        let capture_directory = || {
            rooted
                .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new("dir"), true)
                .unwrap();
            let parent = File::open(root.path().join("dir")).unwrap();
            rooted
                .record_source_name_blocking(parent.as_raw_fd(), Path::new("dir/data"), true)
                .unwrap();
        };
        if directory_first {
            capture_directory();
        } else {
            rooted
                .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new("entry"), true)
                .unwrap();
        }
        std::fs::rename(root.path().join("dir"), foreign.path().join("held")).unwrap();
        std::fs::rename(foreign.path().join("replacement"), root.path().join("dir")).unwrap();
        if directory_first {
            rooted
                .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new("entry"), true)
                .unwrap();
            std::fs::rename(root.path().join("dir"), foreign.path().join("replacement")).unwrap();
            std::fs::rename(foreign.path().join("held"), root.path().join("dir")).unwrap();
        } else {
            capture_directory();
        }
        rooted
            .complete_source_directory_blocking(Path::new("dir"))
            .unwrap();
        rooted
            .complete_source_directory_blocking(Path::new(""))
            .unwrap();
        attempt.finish(true, || true).unwrap();
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        assert_eq!(inventory.record_count(), 4);
        assert!(
            matches!(
                inventory.excludes_objects_blocking(&[], SourceExclusion::Objects),
                Err(RootedFsError::SourceBindingChanged(_))
            ),
            "same FILE inode does not reconcile foreign DIR provenance"
        );
        assert!(matches!(
            inventory.validate_originals_blocking(),
            Err(RootedFsError::SourceRaceCutoff)
        ));
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn forward_discovery_never_closes_an_unreadable_target_directory() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::fs::write(root.path().join("dir/data"), b"DATA").unwrap();
    std::os::unix::fs::symlink(".//dir///.//data", root.path().join("entry")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    // Search permits FILE discovery, but read denial prevents a protection census.
    std::fs::set_permissions(
        root.path().join("dir"),
        std::fs::Permissions::from_mode(0o111),
    )
    .unwrap();
    let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
    let rooted = source.rooted();
    source
        .entries(request().shallow(1))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let inventory = rooted.seal_source_inventory_blocking().unwrap();
    assert_eq!(
        inventory.record_count(),
        3,
        "discovered DATA must not become an enumerated original"
    );
    assert!(!inventory
        .excludes_objects_blocking(&[], SourceExclusion::Objects)
        .unwrap());
    std::fs::set_permissions(
        root.path().join("dir"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
}

#[test]
fn forward_resolver_replays_deep_targets_under_low_fd_limit() {
    const CHILD_ROOT: &str = "SY_TEST_FORWARD_RESOLUTION_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let parent = File::open(Path::new(&root)).unwrap();
        for _ in 0..8 {
            let (observation, _, terminal) = inspect_with_stat(
                ResolutionRoot::Held(parent.as_raw_fd()),
                parent.as_raw_fd(),
                Path::new("entry"),
                true,
            )
            .unwrap();
            assert_eq!(observation.resolution.len(), 65);
            assert!(observation.matches(&Observation::decode(&observation.encode()).unwrap(), true));
            let terminal = terminal.unwrap();
            let mut file = open_file_at(terminal.parent.as_raw_fd(), &terminal.name).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"deep DATA");
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut target = PathBuf::new();
    for _ in 0..64 {
        target.push("d");
        std::fs::create_dir(root.path().join(&target)).unwrap();
    }
    target.push("data");
    std::fs::write(root.path().join(&target), b"deep DATA").unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("entry")).unwrap();
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "rooted_fs::source_observations::forward_tests::forward_resolver_replays_deep_targets_under_low_fd_limit"])
        .env(CHILD_ROOT, root.path());
    // SAFETY: only async-signal-safe syscalls and stack data after fork.
    unsafe {
        child.pre_exec(|| {
            let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut limit = limit.assume_init();
            limit.rlim_cur = limit.rlim_cur.min(32);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = child.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn forward_file_resolution_refuses_directory_links_and_lexical_shortcuts() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::fs::write(root.path().join("dir/data"), b"inside").unwrap();
    std::fs::write(outside.path().join("data"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    let parent = File::open(root.path()).unwrap();
    for target in [
        "escape/data",
        "dir/../../dir/data",
        "dir/data/.",
        "dir//../../dir/data",
        "dir/data/",
        "dir//data//",
        "/dir/data",
        "//dir/data",
        "../data",
        "dir/data/extra",
    ] {
        std::os::unix::fs::symlink(target, root.path().join("entry")).unwrap();
        assert!(matches!(
            inspect(
                ResolutionRoot::Held(parent.as_raw_fd()),
                parent.as_raw_fd(),
                Path::new("entry"),
                true
            ),
            Err(RootedFsError::SourceFollowUnsupported(_))
        ));
        std::fs::remove_file(root.path().join("entry")).unwrap();
    }
    assert_eq!(
        std::fs::read(outside.path().join("data")).unwrap(),
        b"outside"
    );
}
