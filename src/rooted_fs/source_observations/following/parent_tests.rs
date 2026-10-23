use super::*;
#[cfg(target_os = "macos")]
use crate::endpoint::source_root::SourceRoot;
#[cfg(target_os = "macos")]
use futures::TryStreamExt;

fn supported(root: RawFd) -> bool {
    match beneath::directory(root, Path::new("."), Path::new("entry")) {
        Ok(_) => true,
        Err(RootedFsError::SourceFollowUnsupported(_)) => false,
        Err(error) => panic!("mandatory native qualification failed: {error}"),
    }
}

#[test]
fn root_prefix_authenticates_raw_names_codec_and_supplied_parent() {
    let fixture = tempfile::tempdir().unwrap();
    let name = OsStr::new("dir-é");
    let directory_path = fixture.path().join(name);
    std::fs::create_dir(&directory_path).unwrap();
    std::fs::write(directory_path.join("data"), b"DATA").unwrap();
    std::os::unix::fs::symlink("./data", directory_path.join("link")).unwrap();
    let root = File::open(fixture.path()).unwrap();
    let directory = File::open(&directory_path).unwrap();
    let path = Path::new(name).join("link");
    let (original, _, terminal) = inspect_with_stat(
        ResolutionRoot::Held(root.as_raw_fd()),
        directory.as_raw_fd(),
        &path,
        true,
    )
    .unwrap();
    assert_eq!(original.resolution_start, 1);
    assert_eq!(original.resolution[0].entry().unwrap().name, name);
    assert!(original.matches(&Observation::decode(&original.encode()).unwrap(), true));
    let terminal = terminal.unwrap();
    let mut file = open_file_at(terminal.parent.as_raw_fd(), &terminal.name).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"DATA");
    // APFS rejects invalid UTF-8 directory names, so exercise their exact tape
    // representation separately from native lookup on this filesystem.
    let mut raw = Observation::decode(&original.encode()).unwrap();
    raw.resolution[0].entry_mut().unwrap().name = OsStr::from_bytes(b"dir-\xff").to_os_string();
    raw.parent = key([0; 32], &raw.resolution[0].entry_mut().unwrap().name);
    assert!(raw.matches(&Observation::decode(&raw.encode()).unwrap(), true));
    for mutation in 0..3 {
        let mut changed = Observation::decode(&original.encode()).unwrap();
        match mutation {
            0 => changed.resolution_start = 0,
            1 => changed.parent = [0; 32],
            _ => {
                changed.resolution[0].entry_mut().unwrap().target =
                    Some(LinkTarget::new(OsStr::new("data")))
            }
        }
        assert!(Observation::decode(&changed.encode()).is_err());
    }
    let foreign = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("./data", foreign.path().join("link")).unwrap();
    let foreign_parent = File::open(foreign.path()).unwrap();
    assert!(matches!(
        inspect_with_stat(
            ResolutionRoot::Held(root.as_raw_fd()),
            foreign_parent.as_raw_fd(),
            &path,
            true
        ),
        Err(RootedFsError::SourceBindingChanged(_))
    ));
}

#[test]
fn parents_keep_root_context_literal_provenance_and_codec_bounds() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("dir")).unwrap();
    std::fs::write(fixture.path().join("file"), b"root DATA").unwrap();
    std::os::unix::fs::symlink("../file", fixture.path().join("dir/link")).unwrap();
    std::os::unix::fs::symlink("dir/../dir/link", fixture.path().join("entry")).unwrap();
    let root = File::open(fixture.path()).unwrap();
    let directory = File::open(fixture.path().join("dir")).unwrap();
    for (name, parent, prefix) in [
        ("entry", root.as_raw_fd(), 0),
        ("dir/link", directory.as_raw_fd(), 1),
    ] {
        let result = inspect_with_stat(
            ResolutionRoot::Held(root.as_raw_fd()),
            parent,
            Path::new(name),
            true,
        );
        if !supported(root.as_raw_fd()) {
            assert!(matches!(
                result,
                Err(RootedFsError::SourceFollowUnsupported(_))
            ));
            continue;
        }
        let (original, _, terminal) = result.unwrap();
        assert_eq!(original.resolution_start, prefix);
        assert!(original.resolution.iter().any(
            |step| matches!(step, ResolutionStep::Entry(entry) if entry.name == OsStr::new(".."))
        ));
        assert!(original.matches(&Observation::decode(&original.encode()).unwrap(), true));
        let terminal = terminal.unwrap();
        let mut reader = open_file_at(terminal.parent.as_raw_fd(), &terminal.name).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"root DATA");
        if prefix != 0 {
            let mut changed = Observation::decode(&original.encode()).unwrap();
            changed.resolution_start += 1;
            assert!(Observation::decode(&changed.encode()).is_err());
            let mut changed = Observation::decode(&original.encode()).unwrap();
            changed.parent = [0; 32];
            assert!(Observation::decode(&changed.encode()).is_err());
        }
    }
    for target in [
        "dir/../../file".to_string(),
        format!(
            "../{}/file",
            fixture.path().file_name().unwrap().to_str().unwrap()
        ),
        "file/../file".to_string(),
        "missing/../file".to_string(),
    ] {
        std::os::unix::fs::symlink(target, fixture.path().join("refuse")).unwrap();
        assert!(matches!(
            inspect(
                ResolutionRoot::Held(root.as_raw_fd()),
                root.as_raw_fd(),
                Path::new("refuse"),
                true
            ),
            Err(RootedFsError::SourceFollowUnsupported(_))
        ));
        std::fs::remove_file(fixture.path().join("refuse")).unwrap();
    }
    if supported(root.as_raw_fd()) {
        let original = inspect(
            ResolutionRoot::Held(root.as_raw_fd()),
            directory.as_raw_fd(),
            Path::new("dir/link"),
            true,
        )
        .unwrap();
        let moved = tempfile::tempdir().unwrap();
        std::fs::rename(fixture.path(), moved.path().join("relocated")).unwrap();
        let (observed, _, terminal) = inspect_with_stat(
            ResolutionRoot::Held(root.as_raw_fd()),
            directory.as_raw_fd(),
            Path::new("dir/link"),
            true,
        )
        .unwrap();
        assert!(
            original.matches(&observed, true),
            "relative ROOT authority survives relocation without recapturing an address"
        );
        let terminal = terminal.unwrap();
        let mut file = open_file_at(terminal.parent.as_raw_fd(), &terminal.name).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"root DATA");
    }
}

#[test]
fn native_parent_must_match_predecessor_and_original_child_edge() {
    for moved in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("a/d")).unwrap();
        std::fs::create_dir(fixture.path().join("b")).unwrap();
        std::fs::write(fixture.path().join("a/file"), b"DATA").unwrap();
        std::os::unix::fs::symlink("a/d/../file", fixture.path().join("entry")).unwrap();
        let root = File::open(fixture.path()).unwrap();
        let result = inspect(
            ResolutionRoot::Held(root.as_raw_fd()),
            root.as_raw_fd(),
            Path::new("entry"),
            true,
        );
        if !supported(root.as_raw_fd()) {
            assert!(matches!(
                result,
                Err(RootedFsError::SourceFollowUnsupported(_))
            ));
            continue;
        }
        let mut original = result.unwrap();
        let current = File::open(fixture.path().join("a/d")).unwrap();
        if moved {
            // ROOT-relative replay now reaches a fresh d, not the earlier held
            // current DIR. Do not probe the moved-out DIR's native parent.
            std::fs::rename(fixture.path().join("a/d"), outside.path().join("held")).unwrap();
            std::fs::create_dir(fixture.path().join("a/d")).unwrap();
        } else {
            let foreign = File::open(fixture.path().join("b")).unwrap();
            original.resolution[0].entry_mut().unwrap().named =
                NativeObservation::from_stat(&stat_fd(foreign.as_raw_fd()).unwrap()).unwrap();
            original.resolution[0].entry_mut().unwrap().object_key =
                object_key(foreign.as_raw_fd(), None).unwrap();
        }
        let stack = vec![(0, [0; 32]), (1, key([0; 32], OsStr::new("a")))];
        let result = native_parent(
            root.as_raw_fd(),
            Path::new("entry"),
            &original.resolution,
            &stack,
            current.as_raw_fd(),
            Path::new("a/d"),
        );
        if moved {
            assert!(matches!(
                result,
                Err(RootedFsError::SourceBindingChanged(_))
            ));
        } else {
            assert!(matches!(
                result,
                Err(RootedFsError::SourceFollowUnsupported(_))
            ));
        }
        assert_eq!(
            std::fs::read(fixture.path().join("a/file")).unwrap(),
            b"DATA"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn root_prefix_discovery_requires_exact_physical_originals_in_both_scanners() {
    for (rooted_scanner, metadata, parents) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
        (true, false, true),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("dir")).unwrap();
        let file = if parents { "file" } else { "dir/file" };
        let target = if parents { "../file" } else { "file" };
        let entry = if parents {
            "dir/../dir/link"
        } else {
            "dir/link"
        };
        std::fs::write(fixture.path().join(file), b"DATA").unwrap();
        std::os::unix::fs::symlink(target, fixture.path().join("dir/link")).unwrap();
        std::os::unix::fs::symlink(entry, fixture.path().join("entry")).unwrap();
        let source = SourceRoot::open(fixture.path().to_path_buf())
            .await
            .unwrap();
        let rooted = source.rooted();
        let request = crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        let entries = if rooted_scanner {
            rooted.source_entry_stream(request)
        } else {
            source.entries(request)
        }
        .try_collect::<Vec<_>>()
        .await;
        if parents {
            assert!(entries.is_err(), "Darwin parent discovery must fail closed");
            continue;
        }
        assert_eq!(entries.unwrap().len(), 4);
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        assert_eq!(
            inventory.record_count(),
            5,
            "prefixes and parent results are never synthetic members"
        );
        inventory.validate_originals_blocking().unwrap();
        assert!(inventory.excludes_objects_blocking(&[]).unwrap());
        for name in ["entry", "dir", "dir/link", file] {
            let path = RelativePath::new(name).unwrap();
            let (parent, leaf) = rooted.open_parent_blocking(path.as_path()).unwrap();
            let protected = object_key(parent.as_raw_fd(), Some(&leaf))
                .unwrap()
                .unwrap();
            assert!(!inventory.excludes_objects_blocking(&[protected]).unwrap());
        }
        let path = RelativePath::new("entry").unwrap();
        let foreign = tempfile::tempdir().unwrap();
        std::fs::rename(fixture.path().join("dir"), foreign.path().join("original")).unwrap();
        std::fs::create_dir(fixture.path().join("dir")).unwrap();
        std::os::unix::fs::symlink(target, fixture.path().join("dir/link")).unwrap();
        let result = if metadata {
            rooted
                .source_entry_metadata_blocking(&path, request)
                .map(drop)
        } else {
            rooted.open_source_following_blocking(&path).map(drop)
        };
        assert!(matches!(
            result,
            Err(RootedFsError::SourceBindingChanged(_))
        ));
        std::fs::remove_dir_all(fixture.path().join("dir")).unwrap();
        std::fs::rename(foreign.path().join("original"), fixture.path().join("dir")).unwrap();
        // A failed original owner stays terminal; this fixture has no
        // asynchronous native work outstanding at the new CHILD boundary.
        let next = rooted.retry_source_run_blocking().unwrap();
        let source = SourceRoot::from_rooted(next.clone()).unwrap();
        source
            .entries(request)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        next.validate_source_observation_blocking(&path).unwrap();
        assert!(rooted.validate_source_observation_blocking(&path).is_err());
    }
}

#[test]
fn parent_replay_or_refusal_keeps_constant_descriptors_on_deep_paths() {
    const CHILD: &str = "SY_TEST_PARENT_RESOLUTION_ROOT";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = File::open(Path::new(&root)).unwrap();
        for _ in 0..8 {
            let result = inspect_with_stat(
                ResolutionRoot::Held(root.as_raw_fd()),
                root.as_raw_fd(),
                Path::new("entry"),
                true,
            );
            if !supported(root.as_raw_fd()) {
                assert!(matches!(
                    result,
                    Err(RootedFsError::SourceFollowUnsupported(_))
                ));
                continue;
            }
            let (observation, _, terminal) = result.unwrap();
            assert_eq!(observation.resolution.len(), 67);
            assert!(observation.matches(&Observation::decode(&observation.encode()).unwrap(), true));
            let terminal = terminal.unwrap();
            let mut file = open_file_at(terminal.parent.as_raw_fd(), &terminal.name).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"deep DATA");
        }
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let mut path = PathBuf::new();
    for _ in 0..64 {
        path.push("d");
        std::fs::create_dir(fixture.path().join(&path)).unwrap();
    }
    std::fs::write(fixture.path().join(&path).join("data"), b"deep DATA").unwrap();
    std::os::unix::fs::symlink(path.join("../d/data"), fixture.path().join("entry")).unwrap();
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "rooted_fs::source_observations::following::parent_tests::parent_replay_or_refusal_keeps_constant_descriptors_on_deep_paths"]).env(CHILD, fixture.path());
    // SAFETY: only async-signal-safe setrlimit and stack data after fork.
    unsafe {
        child.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 32,
                rlim_max: 32,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let result = child.output().unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
