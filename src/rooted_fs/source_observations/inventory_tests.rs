use super::*;
use futures::TryStreamExt;

// Inspect authenticated ORIGINAL payloads, not freshly adopted stat records.
fn details(
    inventory: &SourceInventory,
    ordinal: u64,
) -> (
    PathBuf,
    Observation,
    Option<SourceRootProtection>,
    Option<SourceDirectoryDisposition>,
) {
    let mut owner = inventory.rooted.source_observations.lock().unwrap();
    let catalogue = owner.active().unwrap();
    let (key, original) = catalogue.original_at(ordinal).unwrap();
    let path = catalogue.original_path(key, &original).unwrap();
    let root = (ordinal == 0).then_some(catalogue.scope.unwrap());
    let directory = if original.effective.kind == libc::S_IFDIR as u64
        && root != Some(SourceRootProtection::SelectedLeafAnchor)
    {
        Some(catalogue.disposition(key).unwrap())
    } else {
        None
    };
    (path, original, root, directory)
}

async fn source_scan(
    root: &Path,
    local: bool,
    request: crate::engine::scan::ScanRequest,
) -> RootedFs {
    let source = crate::endpoint::source_root::SourceRoot::open(root.to_path_buf())
        .await
        .unwrap();
    let rooted = source.rooted();
    let stream = if local {
        source.entries(request)
    } else {
        rooted.source_entry_stream(request)
    };
    stream.try_collect::<Vec<_>>().await.unwrap();
    rooted
}

#[cfg(target_os = "macos")]
async fn selected_followed_inventory(path: &Path) -> SourceInventory {
    let bound =
        crate::rooted_fs::operand::BoundSource::open_local_following(path.to_path_buf(), false)
            .await
            .unwrap();
    let crate::rooted_fs::operand::SourceShape::Leaf { name } = bound.shape else {
        panic!("expected selected link")
    };
    let source = crate::endpoint::source_root::SourceRoot::from_rooted(bound.rooted).unwrap();
    source
        .selected_entries(
            name,
            crate::engine::scan::ScanRequest {
                follow_symlinks: true,
                ..Default::default()
            },
            false,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    source.rooted().seal_source_inventory_blocking().unwrap()
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn followed_selection_protects_both_objects_but_not_its_parent_anchor() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let target = fixture.path().join("target");
    std::fs::write(&target, b"DATA").unwrap();
    let selected = source.join("selected");
    std::os::unix::fs::symlink(&target, &selected).unwrap();
    let inventory = selected_followed_inventory(&selected).await;
    let (_, leaf, _, _) = details(&inventory, 1);
    let keys = inventory.selected_leaf_keys_blocking().unwrap().unwrap();
    assert!(Some(keys[0]) == leaf.object_key);
    assert!(Some(keys[1]) == leaf.followed_object_key);
    assert!(keys[0] != keys[1]);
    for key in keys {
        assert!(!inventory
            .excludes_objects_blocking(&[key], SourceExclusion::Objects)
            .unwrap());
    }
    let (path, root, _, _) = details(&inventory, 0);
    assert!(path.as_os_str().is_empty());
    assert!(inventory
        .excludes_objects_blocking(&[root.object_key.unwrap()], SourceExclusion::Objects)
        .unwrap());
    let mut foreign_view = root.object_key.unwrap();
    foreign_view.0[16] ^= 1;
    assert!(!inventory
        .excludes_objects_blocking(&[foreign_view], SourceExclusion::Objects)
        .unwrap());
    inventory.validate_originals_blocking().unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn followed_data_object_key_is_authenticated_and_cannot_be_repaired() {
    use std::os::unix::fs::FileExt;
    let fixture = tempfile::tempdir().unwrap();
    let target = fixture.path().join("target");
    let selected = fixture.path().join("selected");
    std::fs::write(&target, b"DATA").unwrap();
    std::os::unix::fs::symlink(&target, &selected).unwrap();
    let inventory = selected_followed_inventory(&selected).await;
    let (offset, byte) = {
        let mut owner = inventory.rooted.source_observations.lock().unwrap();
        let c = owner.active().unwrap();
        let (key, leaf) = c.original_at(1).unwrap();
        let object = leaf.followed_object_key.unwrap();
        let bytes = leaf.encode();
        let position = bytes
            .windows(32)
            .position(|bytes| bytes == object.0)
            .unwrap();
        let locator = c.index.get_bytes(key).unwrap().unwrap();
        let offset = u64::from_le_bytes(locator[..8].try_into().unwrap()) + position as u64;
        let byte = bytes[position];
        c.file.write_all_at(&[byte ^ 1], offset).unwrap();
        (offset, byte)
    };
    assert!(matches!(
        inventory.selected_leaf_keys_blocking(),
        Err(RootedFsError::Io(error)) if error.kind() == io::ErrorKind::InvalidData
    ));
    {
        let mut owner = inventory.rooted.source_observations.lock().unwrap();
        let CatalogueState::Active(c) = &mut *owner else {
            panic!("expected retained catalogue")
        };
        c.file.write_all_at(&[byte], offset).unwrap();
    }
    assert!(matches!(
        inventory.selected_leaf_keys_blocking(),
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
    assert_eq!(std::fs::read(&target).unwrap(), b"DATA");
    assert_eq!(std::fs::read_link(&selected).unwrap(), target);
}

#[tokio::test]
async fn inventory_records_omission_and_actual_directory_completion() {
    for local in [false, true] {
        let root = tempfile::tempdir().unwrap();
        for name in [".git", "ignored", "depth", "empty"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        std::fs::write(root.path().join(".ignore"), b"ignored/\n").unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        std::fs::hard_link(root.path().join("file"), root.path().join("alias")).unwrap();
        std::fs::create_dir(root.path().join("depth/pruned")).unwrap();
        std::fs::write(root.path().join("depth/pruned/unseen"), b"unseen").unwrap();
        std::fs::write(root.path().join("ignored/unseen"), b"unseen").unwrap();
        let rooted = source_scan(
            root.path(),
            local,
            crate::engine::scan::ScanRequest {
                include_git_dir: false,
                max_depth: Some(2),
                ..Default::default()
            },
        )
        .await;
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        inventory.validate_originals_blocking().unwrap();
        let mut paths = Vec::new();
        for ordinal in 0..inventory.record_count() {
            let (path, original, _, directory) = details(&inventory, ordinal);
            let expected = match path.to_str().unwrap() {
                "" | "depth" | "empty" => Some(SourceDirectoryDisposition::Traversed),
                ".git" => {
                    assert!(matches!(
                        directory,
                        Some(
                            SourceDirectoryDisposition::Opaque
                                | SourceDirectoryDisposition::Protected
                        )
                    ));
                    directory
                }
                "ignored" | "depth/pruned" => Some(SourceDirectoryDisposition::Protected),
                _ => None,
            };
            assert_eq!(directory, expected);
            if path == Path::new("file") || path == Path::new("alias") {
                assert_eq!(original.named.nlink, 2);
                assert!(original.named == original.effective);
                assert!(!original.followed);
                assert!(original.target.is_none());
            }
            paths.push(path);
        }
        assert_eq!(paths.len() as u64, inventory.record_count());
        assert!(paths.contains(&PathBuf::from("ignored")));
        assert!(paths.contains(&PathBuf::from("ignored/unseen")));
        assert!(paths.contains(&PathBuf::from("depth/pruned/unseen")));
        inventory.validate_originals_blocking().unwrap();
    }
}

#[tokio::test]
async fn depth_zero_tree_and_selected_parent_anchor_are_distinct() {
    for local in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("foo"), b"selected").unwrap();
        std::fs::write(root.path().join("bar"), b"unselected destination").unwrap();
        let tree = source_scan(
            root.path(),
            local,
            crate::engine::scan::ScanRequest {
                max_depth: Some(0),
                ..Default::default()
            },
        )
        .await;
        let inventory = tree.seal_source_inventory_blocking().unwrap();
        assert_eq!(inventory.record_count(), 3);
        inventory.validate_originals_blocking().unwrap();
        let (_, _, protection, directory) = details(&inventory, 0);
        assert_eq!(protection, Some(SourceRootProtection::Tree));
        assert_eq!(directory, Some(SourceDirectoryDisposition::Protected));

        let source = crate::endpoint::source_root::SourceRoot::open(root.path().to_path_buf())
            .await
            .unwrap();
        let rooted = source.rooted();
        let stream = if local {
            source.selected_entries(RelativePath::new("foo").unwrap(), Default::default(), false)
        } else {
            rooted
                .selected_source_entry_stream(RelativePath::new("foo").unwrap(), Default::default())
        };
        stream.try_collect::<Vec<_>>().await.unwrap();
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        assert_eq!(inventory.record_count(), 2);
        inventory.validate_originals_blocking().unwrap();
        let (_, _, protection, directory) = details(&inventory, 0);
        assert_eq!(protection, Some(SourceRootProtection::SelectedLeafAnchor));
        assert_eq!(
            directory, None,
            "anchor must not protect every sibling subtree"
        );
        assert_eq!(details(&inventory, 1).0, Path::new("foo"));
    }
}

#[tokio::test]
async fn failure_and_early_close_cannot_seal_partial_inventory() {
    for local in [false, true] {
        for cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let _socket = (!cancel).then(|| {
                std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap()
            });
            if cancel {
                for i in 0..600 {
                    std::fs::write(root.path().join(format!("n{i:04}")), b"x").unwrap();
                }
            }
            let source = crate::endpoint::source_root::SourceRoot::open(root.path().to_path_buf())
                .await
                .unwrap();
            let rooted = source.rooted();
            assert!(rooted.seal_source_inventory_blocking().is_err());
            let mut stream = if local {
                source.entries(Default::default())
            } else {
                rooted.source_entry_stream(Default::default())
            };
            if cancel {
                stream.try_next().await.unwrap().unwrap();
                assert!(matches!(
                    rooted.seal_source_inventory_blocking(),
                    Err(RootedFsError::SourceInventoryIncomplete)
                ));
                stream.close().await.unwrap();
            } else {
                assert!(stream.try_collect::<Vec<_>>().await.is_err());
            }
            assert!(rooted.seal_source_inventory_blocking().is_err());
        }
    }
}

#[tokio::test]
async fn original_path_reuse_validates_member_key_name_and_root() {
    let root = tempfile::tempdir().unwrap();
    let path = Path::new("parent/file");
    std::fs::create_dir(root.path().join("parent")).unwrap();
    std::fs::write(root.path().join(path), b"original").unwrap();
    let rooted = source_scan(root.path(), false, Default::default()).await;
    let inventory = rooted.seal_source_inventory_blocking().unwrap();
    let mut owner = inventory.rooted.source_observations.lock().unwrap();
    let c = owner.active().unwrap();
    let key = path_key(path);
    let mut original = c.require(path).unwrap();
    assert_eq!(c.original_path(key, &original).unwrap(), path);
    assert!(c.original_path([7; 32], &original).is_err());
    // Path replay preserves native bytes even on filesystems that reject
    // creating non-UTF-8 names. No lookup of this supplied component is needed.
    original.name = OsStr::from_bytes(b"native-\xff").to_os_string();
    assert_eq!(
        c.original_path(super::key(original.parent, &original.name), &original)
            .unwrap(),
        Path::new("parent").join(&original.name)
    );
    for name in [b"".as_slice(), b".", b"..", b"a/b", b"a\0b"] {
        original.name = OsStr::from_bytes(name).to_os_string();
        let key = super::key(original.parent, &original.name);
        assert!(c.original_path(key, &original).is_err(), "{name:?}");
    }
    let mut original = c.require(Path::new("")).unwrap();
    assert_eq!(c.original_path([0; 32], &original).unwrap(), Path::new(""));
    original.parent = [7; 32];
    assert!(c.original_path([0; 32], &original).is_err());
    original.parent = [0; 32];
    original.name = OsString::from("not-root");
    assert!(c.original_path([0; 32], &original).is_err());
}

#[tokio::test]
async fn original_path_reuse_authenticates_parents_after_member_read() {
    for ancestor in [Path::new(""), Path::new("parent")] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("parent")).unwrap();
        std::fs::write(root.path().join("parent/file"), b"original").unwrap();
        let rooted = source_scan(root.path(), false, Default::default()).await;
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        let mut owner = inventory.rooted.source_observations.lock().unwrap();
        let c = owner.active().unwrap();
        let path = Path::new("parent/file");
        let original = c.require(path).unwrap();
        let locator = c.index.get_bytes(path_key(ancestor)).unwrap().unwrap();
        let offset = u64::from_le_bytes(locator[..8].try_into().unwrap());
        // The supplied FILE is already authenticated; that must not stand in
        // for freshly authenticated parents, including the terminating ROOT.
        c.file.write_all_at(&[99], offset).unwrap();
        assert!(matches!(
            c.original_path(path_key(path), &original),
            Err(RootedFsError::Io(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
    }
}

#[tokio::test]
async fn scratch_extent_authentication_and_exact_replay_remain_terminal() {
    for fault in 0..4 {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        let rooted = source_scan(root.path(), false, Default::default()).await;
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        inventory.validate_originals_blocking().unwrap();
        {
            let mut owner = rooted.source_observations.lock().unwrap();
            let catalogue = owner.active().unwrap();
            match fault {
                0 => catalogue.file.set_len(catalogue.end - 1).unwrap(),
                1 => catalogue.file.set_len(catalogue.end + 1).unwrap(),
                2 => {
                    let locator = catalogue
                        .index
                        .get_bytes(path_key(Path::new("file")))
                        .unwrap()
                        .unwrap();
                    let offset = u64::from_le_bytes(locator[..8].try_into().unwrap());
                    catalogue.file.seek(SeekFrom::Start(offset)).unwrap();
                    catalogue.file.write_all(&[99]).unwrap();
                }
                _ => {
                    let length = catalogue.index.file.metadata().unwrap().len();
                    catalogue.index.file.set_len(length + 1).unwrap();
                }
            }
        }
        assert!(inventory.validate_originals_blocking().is_err());
        assert!(matches!(
            inventory.validate_originals_blocking(),
            Err(RootedFsError::SourceCatalogueTerminal)
        ));
        assert!(rooted.seal_source_inventory_blocking().is_err());
    }
}

#[tokio::test]
async fn repeated_scan_validates_exact_originals_and_cannot_adopt_arrivals_or_missing_names() {
    for change in 0..3 {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        let rooted = source_scan(root.path(), false, Default::default()).await;
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        rooted
            .source_entry_stream(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            rooted
                .seal_source_inventory_blocking()
                .unwrap()
                .record_count(),
            inventory.record_count()
        );
        match change {
            0 => std::fs::write(root.path().join("new"), b"arrival").unwrap(),
            1 => std::fs::remove_file(root.path().join("file")).unwrap(),
            _ => std::fs::write(root.path().join("file"), b"changed original").unwrap(),
        }
        assert!(rooted
            .source_entry_stream(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .is_err());
        assert!(inventory.validate_originals_blocking().is_err());
    }
}

#[tokio::test]
async fn selected_queries_accumulate_originals_only_until_sealed_and_tree_scope_never_narrows() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("foo"), b"first").unwrap();
    std::fs::write(root.path().join("bar"), b"second").unwrap();
    let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
    for name in ["foo", "bar", "foo"] {
        rooted
            .selected_source_entry_stream(RelativePath::new(name).unwrap(), Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
    }
    let inventory = rooted.seal_source_inventory_blocking().unwrap();
    assert_eq!(inventory.record_count(), 3);
    inventory.validate_originals_blocking().unwrap();
    std::fs::write(root.path().join("arrival"), b"new").unwrap();
    assert!(rooted
        .selected_source_entry_stream(RelativePath::new("arrival").unwrap(), Default::default())
        .try_collect::<Vec<_>>()
        .await
        .is_err());
    assert!(inventory.validate_originals_blocking().is_err());

    let tree = source_scan(root.path(), false, Default::default()).await;
    tree.selected_source_entry_stream(RelativePath::new("foo").unwrap(), Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let inventory = tree.seal_source_inventory_blocking().unwrap();
    inventory.validate_originals_blocking().unwrap();
    let (_, _, protection, directory) = details(&inventory, 0);
    assert_eq!(protection, Some(SourceRootProtection::Tree));
    assert_eq!(directory, Some(SourceDirectoryDisposition::Traversed));
}

#[tokio::test]
async fn unqualified_descendant_follow_refuses_before_target_metadata() {
    use crate::endpoint::source_root::SourceRoot;
    for case in 0..8 {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("data"), b"original").unwrap();
        let target = match case % 4 {
            0 => PathBuf::from("./../data"),
            1 => PathBuf::from("../absent/target"),
            2 => outside.path().join("absent"),
            _ => root.path().join("../absent/../data"),
        };
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let source = SourceRoot::open(root.path().to_path_buf()).await.unwrap();
        let rooted = source.rooted();
        let attempt = if case < 4 {
            rooted.start_source_scan_blocking().unwrap()
        } else {
            rooted
                .start_selected_source_scan_blocking(&RelativePath::new("link").unwrap())
                .unwrap()
        };
        let error = rooted
            .record_source_name_blocking(rooted.root_fd.as_raw_fd(), Path::new("link"), true)
            .unwrap_err();
        assert!(
            matches!(error, RootedFsError::SourceFollowUnsupported(_)),
            "{error:?}"
        );
        drop(attempt);
        assert!(matches!(
            rooted.seal_source_inventory_blocking(),
            Err(RootedFsError::SourceCatalogueTerminal)
        ));
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(
            std::fs::read(root.path().join("data")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn batch_end_barrier_rejects_moved_operator_ancestor() {
    use std::os::unix::fs::MetadataExt;
    let fixture = tempfile::tempdir().unwrap();
    let ancestor = fixture.path().join("ancestor");
    let root = ancestor.join("parent/source");
    std::fs::create_dir_all(&root).unwrap();
    for name in ["a", "b"] {
        std::fs::write(root.join(name), b"original").unwrap();
    }
    let rooted = source_scan(&root, true, Default::default()).await;
    let inventory = rooted.seal_source_inventory_blocking().unwrap();
    let original = std::fs::metadata(&root).unwrap();
    let (reached, wait) = tokio::sync::oneshot::channel();
    let (resume, receive) = std::sync::mpsc::channel();
    // Pause after the root and first native binding. Relative SOURCE ancestry
    // still works after this move; only the final operator barrier detects it.
    rooted.pause_mutation_at(
        1,
        PublicationPause {
            point: PublicationPausePoint::AfterSourceValidation,
            reached,
            resume: receive,
        },
    );
    let batch = inventory.clone();
    let worker = tokio::task::spawn_blocking(move || batch.validate_originals_blocking());
    wait.await.unwrap();
    let moved = fixture.path().join("moved");
    std::fs::rename(&ancestor, &moved).unwrap();
    let unchanged = std::fs::metadata(moved.join("parent/source")).unwrap();
    assert_eq!(
        (original.dev(), original.ino()),
        (unchanged.dev(), unchanged.ino())
    );
    resume.send(()).unwrap();
    assert!(matches!(
        worker.await.unwrap(),
        Err(RootedFsError::RootChanged(_))
    ));
    assert!(matches!(
        inventory.validate_originals_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
}

#[tokio::test]
async fn batch_rejects_substituted_ordinal_and_inverse_mapping() {
    for inverse in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::write(fixture.path().join(name), b"same bytes").unwrap();
        }
        let rooted = source_scan(fixture.path(), true, Default::default()).await;
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        {
            let mut owner = rooted.source_observations.lock().unwrap();
            let c = owner.active().unwrap();
            // Deliberately construct authenticated but inconsistent mappings;
            // authenticity alone cannot establish exact original membership.
            if inverse {
                let (key, _) = c.original_at(1).unwrap();
                let mut wrong = [0; 48];
                wrong[..8].copy_from_slice(&2u64.to_le_bytes());
                c.index
                    .set_bytes(state_key(b"ordinal", key), wrong)
                    .unwrap();
            } else {
                let locator = c.index.get_bytes(ordinal_key(2)).unwrap().unwrap();
                c.index.set_bytes(ordinal_key(1), locator).unwrap();
            }
        }
        assert!(inventory.validate_originals_blocking().is_err());
        assert!(matches!(
            inventory.validate_originals_blocking(),
            Err(RootedFsError::SourceCatalogueTerminal)
        ));
    }
}

#[tokio::test]
async fn low_fd_deep_wide_original_inventory_replay() {
    if let Some(root) = std::env::var_os("SY_TEST_SOURCE_INVENTORY_ROOT") {
        for local in [false, true] {
            // Also exercise the new queue without scanner descent: every
            // descendant must be protection-only under the same FD limit.
            let protected = source_scan(
                Path::new(&root),
                local,
                crate::engine::scan::ScanRequest {
                    max_depth: Some(0),
                    ..Default::default()
                },
            )
            .await;
            let protected = protected.seal_source_inventory_blocking().unwrap();
            assert_eq!(protected.record_count(), 1 + 48 + 1200);
            assert_eq!(
                details(&protected, 0).3,
                Some(SourceDirectoryDisposition::Protected)
            );
            let rooted = source_scan(Path::new(&root), local, Default::default()).await;
            let inventory = rooted.seal_source_inventory_blocking().unwrap();
            assert_eq!(inventory.record_count(), 1 + 48 + 1200);
            inventory.validate_originals_blocking().unwrap();
            #[cfg(target_os = "macos")]
            {
                let candidate_dir = tempfile::tempdir().unwrap();
                let candidate = File::open(candidate_dir.path()).unwrap();
                let key = object_key(candidate.as_raw_fd(), None).unwrap().unwrap();
                assert!(inventory
                    .excludes_objects_blocking(&[key], SourceExclusion::Objects)
                    .unwrap());
            }
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut path = root.path().to_path_buf();
    for _ in 0..48 {
        path.push("d");
        std::fs::create_dir(&path).unwrap();
    }
    for i in 0..1200 {
        std::fs::write(path.join(format!("n{i:04}")), b"x").unwrap();
    }
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "rooted_fs::source_observations::inventory::tests::low_fd_deep_wide_original_inventory_replay"])
        .env("SY_TEST_SOURCE_INVENTORY_ROOT", root.path());
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

#[cfg(target_os = "macos")]
#[tokio::test]
async fn derived_child_count_corruption_cannot_be_resealed_or_adopted() {
    use std::os::unix::fs::FileExt;
    for truncate in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("original"), b"SOURCE").unwrap();
        let rooted = source_scan(fixture.path(), true, Default::default()).await;
        let inventory = rooted.seal_source_inventory_blocking().unwrap();
        let candidate_dir = tempfile::tempdir().unwrap();
        let candidate = File::open(candidate_dir.path()).unwrap();
        let candidate = object_key(candidate.as_raw_fd(), None).unwrap().unwrap();
        assert!(inventory
            .excludes_objects_blocking(&[candidate], SourceExclusion::Objects)
            .unwrap());
        {
            let mut owner = rooted.source_observations.lock().unwrap();
            let c = owner.active().unwrap();
            if truncate {
                c.index
                    .file
                    .set_len(c.index.file.metadata().unwrap().len() - 1)
                    .unwrap();
            } else {
                let mut bytes = Vec::new();
                c.index.file.seek(SeekFrom::Start(0)).unwrap();
                c.index.file.read_to_end(&mut bytes).unwrap();
                let mut needle = vec![1]; // authenticated radix leaf tag
                needle.extend_from_slice(&state_key(b"children", [0; 32]));
                needle.extend_from_slice(&1u64.to_le_bytes());
                let offset = bytes
                    .windows(needle.len())
                    .position(|window| window == needle.as_slice())
                    .unwrap();
                // Change only the derived original child count, keeping extent
                // and every observation payload intact. Never mint a digest.
                c.index.file.write_at(&[0], (offset + 33) as u64).unwrap();
            }
        }
        assert!(inventory
            .excludes_objects_blocking(&[candidate], SourceExclusion::Objects)
            .is_err());
        assert!(rooted.seal_source_inventory_blocking().is_err());
        assert!(inventory
            .excludes_objects_blocking(&[candidate], SourceExclusion::Objects)
            .is_err());
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn allocation_and_private_metadata_authenticate_root_state_and_tape_extent() {
    use crate::endpoint::source_root::SourceRoot;
    use crate::engine::namespace::NamespaceQualification;
    use crate::rooted_fs::local_source_effects::LocalSourceEffects;
    for allocation in [true, false] {
        for inverse in [true, false] {
            let fixture = tempfile::tempdir().unwrap();
            let source_path = fixture.path().join("source");
            let destination = fixture.path().join("destination");
            std::fs::create_dir(&source_path).unwrap();
            std::fs::create_dir(&destination).unwrap();
            std::fs::write(source_path.join("file"), b"SOURCE").unwrap();
            let source = SourceRoot::open(source_path.clone()).await.unwrap();
            source
                .entries(Default::default())
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            let effects = LocalSourceEffects::seal(&source).await.unwrap();
            let rooted = RootedFs::open(destination.clone()).await.unwrap();
            let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
            namespace.seal().await.unwrap();
            rooted.local_source_effects = Some(effects.clone());
            let mut staged = if allocation {
                None
            } else {
                Some(
                    rooted
                        .begin_staged_file_blocking(&RelativePath::new("target").unwrap())
                        .unwrap(),
                )
            };
            let mut transaction = if allocation {
                Some(
                    rooted
                        .begin_namespace_blocking(
                            Path::new("target"),
                            ExpectedDestination::Absent,
                            StagingOrigin::Created,
                        )
                        .unwrap(),
                )
            } else {
                None
            };
            let source_rooted = source.rooted();
            {
                let mut owner = source_rooted.source_observations.lock().unwrap();
                let c = owner.active().unwrap();
                if inverse {
                    c.index
                        .set_bytes(state_key(b"ordinal", [0; 32]), [42; 48])
                        .unwrap();
                } else {
                    c.file.set_len(c.end + 1).unwrap();
                }
            }
            let before = staged
                .as_ref()
                .map(|s| stat_fd(s.file.as_raw_fd()).unwrap());
            let result = if let Some(transaction) = &mut transaction {
                transaction
                    .create_regular_staging_file_blocking()
                    .map(|_| ())
            } else {
                staged
                    .as_mut()
                    .unwrap()
                    .apply_metadata_blocking(Some(0o400), None)
            };
            assert!(
                matches!(result, Err(RootedFsError::Io(ref error)) if error.kind() == io::ErrorKind::InvalidData),
                "{result:?}"
            );
            assert!(matches!(
                effects.validate_lifetime_blocking(),
                Err(RootedFsError::SourceCatalogueTerminal)
            ));
            if let Some(staged) = staged {
                assert!(staging::same_observation(
                    &before.unwrap(),
                    &stat_fd(staged.file.as_raw_fd()).unwrap()
                ));
                staged.abort().unwrap();
            }
            if let Some(transaction) = transaction {
                assert!(stat_at_optional(
                    transaction.staging_dir_fd.as_raw_fd(),
                    &transaction.temp_name
                )
                .unwrap()
                .is_none());
                transaction.abort().unwrap();
            }
            assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
            assert_eq!(std::fs::read(source_path.join("file")).unwrap(), b"SOURCE");
        }
    }
}
