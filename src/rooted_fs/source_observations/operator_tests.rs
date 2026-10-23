use super::*;
use crate::endpoint::source_root::SourceRoot;
use futures::TryStreamExt;

#[tokio::test]
async fn ordinary_and_followed_readers_keep_the_authenticated_opening_parent_not_a_fresh_alias() {
    use std::os::unix::fs::MetadataExt;
    for followed in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let selected = fixture.path().join("selected");
        let foreign = fixture.path().join("foreign");
        let held = fixture.path().join("held");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(selected.join("file"), b"original DATA").unwrap();
        std::fs::hard_link(selected.join("file"), foreign.join("file")).unwrap();
        if followed {
            std::os::unix::fs::symlink("selected/file", fixture.path().join("entry")).unwrap();
        }
        let original_parent = std::fs::metadata(&selected).unwrap().ino();
        let source = SourceRoot::open(fixture.path().to_path_buf())
            .await
            .unwrap();
        source
            .entries(crate::engine::scan::ScanRequest {
                follow_symlinks: followed,
                ..Default::default()
            })
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let rooted = source.rooted();
        let worker_root = rooted.clone();
        let (reached_binding, binding) = tokio::sync::oneshot::channel();
        let (resume_binding, wait_binding) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceBinding,
                reached: reached_binding,
                resume: wait_binding,
            },
        );
        let worker = tokio::task::spawn_blocking(move || {
            worker_root.with_source_follow_binding(
                &RelativePath::new(if followed { "entry" } else { "selected/file" }).unwrap(),
                followed,
                |parent, name, _| {
                    let opening_parent = stat_fd(parent)?.st_ino as u64;
                    let mut file = open_file_at(parent, name)?;
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    Ok((opening_parent, bytes))
                },
            )
        });
        binding.await.unwrap();
        std::fs::rename(&selected, &held).unwrap();
        std::fs::rename(&foreign, &selected).unwrap();
        let (reached_read, read) = tokio::sync::oneshot::channel();
        let (resume_read, wait_read) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceValidation,
                reached: reached_read,
                resume: wait_read,
            },
        );
        resume_binding.send(()).unwrap();
        read.await.unwrap();
        // Restore before post-validation. Both paths named the same hardlinked FILE;
        // only the actual parent cursor distinguishes the foreign opening route.
        std::fs::rename(&selected, &foreign).unwrap();
        std::fs::rename(&held, &selected).unwrap();
        resume_read.send(()).unwrap();
        let (opening_parent, bytes) = worker.await.unwrap().unwrap();
        assert_eq!(opening_parent, original_parent);
        assert_eq!(bytes, b"original DATA");
    }
}

#[tokio::test]
async fn selected_terminal_cursor_is_pinned_before_native_read() {
    use std::os::unix::fs::MetadataExt;
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path().join("target-parent");
    std::fs::create_dir(&parent).unwrap();
    let target = parent.join("data");
    let link = fixture.path().join("selected");
    std::fs::write(&target, b"original").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let bound = BoundSource::open_local_following(link, false)
        .await
        .unwrap();
    let SourceShape::Leaf { name } = bound.shape else {
        panic!("not selected")
    };
    let source = SourceRoot::from_rooted(bound.rooted).unwrap();
    source
        .selected_entries(
            name.clone(),
            crate::engine::scan::ScanRequest {
                follow_symlinks: true,
                ..Default::default()
            },
            false,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let original_inode = std::fs::metadata(&target).unwrap().ino();
    let rooted = source.rooted();
    let (reached, wait) = tokio::sync::oneshot::channel();
    let (resume, receive) = std::sync::mpsc::channel();
    rooted.pause_mutation_at(
        0,
        PublicationPause {
            point: PublicationPausePoint::AfterSourceValidation,
            reached,
            resume: receive,
        },
    );
    let worker = tokio::task::spawn_blocking(move || {
        rooted.with_source_follow_binding(&name, true, |parent, name, _| {
            // An outside lexical substitution must never select this syscall.
            let stat = stat_at_no_follow(parent, name)?;
            assert_eq!(stat.st_ino as u64, original_inode);
            let mut file = open_file_at(parent, name)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"original");
            Ok(())
        })
    });
    wait.await.unwrap();
    std::fs::rename(&parent, fixture.path().join("held-parent")).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(&target, b"outside substitution").unwrap();
    resume.send(()).unwrap();
    assert!(matches!(
        worker.await.unwrap(),
        Err(RootedFsError::RootChanged(_))
    ));
    assert_eq!(std::fs::read(&target).unwrap(), b"outside substitution");
}

#[tokio::test]
async fn selected_followed_file_metadata_does_not_require_data_read_permission() {
    use crate::endpoint::Endpoint;
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let target = target_root.path().join("data");
    let link = fixture.path().join("selected");
    std::fs::write(&target, b"readless").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let bound = BoundSource::open_local_following(link, false)
        .await
        .unwrap();
    let SourceShape::Leaf { name } = bound.shape else {
        panic!("not selected")
    };
    let source = SourceRoot::from_rooted(bound.rooted).unwrap();
    let entries = source
        .selected_entries(
            name.clone(),
            crate::engine::scan::ScanRequest {
                follow_symlinks: true,
                ..Default::default()
            },
            false,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(entries[0].size, 8);
    let metadata = source
        .endpoint()
        .metadata_following(name.as_path())
        .await
        .unwrap();
    assert_eq!(metadata.size, 8);
    assert_eq!(metadata.mode & 0o7777, 0);
    // No selected-name removal authority follows from metadata observation.
    assert_eq!(
        std::fs::read_link(fixture.path().join("selected")).unwrap(),
        target
    );
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn operator_link_retarget_to_same_effective_root_is_terminal() {
    let fixture = tempfile::tempdir().unwrap();
    let directory = fixture.path().join("tree");
    let link = fixture.path().join("selected");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("file"), b"original").unwrap();
    std::os::unix::fs::symlink("tree", &link).unwrap();
    // Directory syntax intentionally follows the selected link without a
    // descendant-follow request. Its literal remains original provenance.
    let bound = crate::rooted_fs::operand::BoundSource::open_local(link.join("."), true)
        .await
        .unwrap();
    let source = SourceRoot::from_rooted(bound.rooted).unwrap();
    let entries: Vec<_> = source
        .entries(Default::default())
        .try_collect()
        .await
        .unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&directory, &link).unwrap();
    assert!(matches!(
        source.validate_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
    assert!(source
        .rooted()
        .open_regular_blocking(&entries[0].path)
        .is_err());
    // Restoring the spelling never repairs this run.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("tree", &link).unwrap();
    assert!(matches!(
        source.validate_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
}

#[tokio::test]
async fn native_link_parent_traversal_and_non_utf8_spelling_are_retained() {
    let fixture = tempfile::tempdir().unwrap();
    let actual = fixture.path().join("actual");
    std::fs::create_dir_all(actual.join("nested")).unwrap();
    #[cfg(target_os = "linux")]
    let name = OsStr::from_bytes(b"source-\xff");
    // APFS rejects ill-formed UTF-8 directory-entry bytes at native creation.
    #[cfg(not(target_os = "linux"))]
    let name = OsStr::new("source-é");
    let root = actual.join(name);
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), b"native").unwrap();
    std::os::unix::fs::symlink("actual/nested", fixture.path().join("link")).unwrap();
    let source = SourceRoot::open(fixture.path().join("link/..").join(name))
        .await
        .unwrap();
    let entries: Vec<_> = source
        .entries(Default::default())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    source.validate_blocking().unwrap();
    let mut bytes = Vec::new();
    source
        .rooted()
        .open_regular_blocking(&entries[0].path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"native");
    // Lexical link/.. would have selected the fixture, not actual.
    assert!(!fixture.path().join(name).exists());
}

#[tokio::test]
async fn explicit_directory_link_selection_captures_native_absolute_target_prefixes() {
    let fixture = tempfile::tempdir().unwrap();
    let target = fixture.path().join("ancestor/tree");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("file"), b"original").unwrap();
    let link = fixture.path().join("selected");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let bound = crate::rooted_fs::operand::BoundSource::open_local_following(link, true)
        .await
        .unwrap();
    assert!(matches!(
        bound.shape,
        SourceShape::Directory { basename: None }
    ));
    let source = SourceRoot::from_rooted(bound.rooted).unwrap();
    let entries: Vec<_> = source
        .entries(Default::default())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let parked = fixture.path().join("parked");
    std::fs::rename(fixture.path().join("ancestor"), &parked).unwrap();
    std::fs::create_dir(fixture.path().join("ancestor")).unwrap();
    std::fs::rename(parked.join("tree"), &target).unwrap();
    assert!(matches!(
        source.validate_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
}

#[tokio::test]
async fn new_run_reuses_operator_provenance_without_reusing_child_inventory() {
    let fixture = tempfile::tempdir().unwrap();
    let ancestor = fixture.path().join("ancestor");
    let path = ancestor.join("tree");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("first"), b"original").unwrap();
    let source = SourceRoot::open(path.clone()).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    std::fs::write(path.join("next"), b"new run").unwrap();
    let next = source.begin_run().await.unwrap();
    let entries: Vec<_> = next
        .entries(Default::default())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    // The old run did not adopt the new name and remains a distinct inventory.
    assert!(source
        .rooted()
        .validate_source_observation_blocking(&RelativePath::new("next").unwrap())
        .is_err());
    let parked = fixture.path().join("parked");
    std::fs::rename(&ancestor, &parked).unwrap();
    std::fs::create_dir(&ancestor).unwrap();
    std::fs::rename(parked.join("tree"), &path).unwrap();
    assert!(matches!(
        next.begin_run().await,
        Err(RootedFsError::RootChanged(_))
    ));
}

#[tokio::test]
async fn selected_regular_leaf_follow_policy_preserves_original_observation() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("file");
    std::fs::write(&path, b"original regular SOURCE").unwrap();
    let bound = BoundSource::open_local_following(path, false)
        .await
        .unwrap();
    let SourceShape::Leaf { name } = bound.shape else {
        panic!("expected selected regular leaf")
    };
    let source = SourceRoot::from_rooted(bound.rooted).unwrap();
    let entries: Vec<_> = source
        .selected_entries(
            name.clone(),
            crate::engine::scan::ScanRequest {
                follow_symlinks: true,
                ..Default::default()
            },
            false,
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, EntryKind::File);
    source.validate_blocking().unwrap();
    let rooted = source.rooted();
    let inventory = rooted.seal_source_inventory_blocking().unwrap();
    assert_eq!(inventory.record_count(), 2);
    {
        let mut owner = rooted.source_observations.lock().unwrap();
        let catalogue = owner.active().unwrap();
        let original = catalogue.require(name.as_path()).unwrap();
        assert!(!original.followed, "ordinary names are not followed scopes");
        assert!(original.named == original.effective);
        assert!(original.target.is_none());
    }
    let mut bytes = Vec::new();
    rooted
        .open_regular_blocking(&name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"original regular SOURCE");
}

#[tokio::test]
async fn selected_followed_leaf_retarget_to_same_inode_rejects_scan() {
    let fixture = tempfile::tempdir().unwrap();
    let first = fixture.path().join("first");
    let second = fixture.path().join("second");
    let link = fixture.path().join("selected");
    std::fs::write(&first, b"original").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    std::os::unix::fs::symlink("first", &link).unwrap();
    let bound = BoundSource::open_local_following(link.clone(), false)
        .await
        .unwrap();
    let SourceShape::Leaf { name } = bound.shape else {
        panic!("expected followed leaf")
    };
    let source = SourceRoot::from_rooted(bound.rooted).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("second", &link).unwrap();
    let entries = source
        .selected_entries(
            name,
            crate::engine::scan::ScanRequest {
                follow_symlinks: true,
                ..Default::default()
            },
            false,
        )
        .try_collect::<Vec<_>>()
        .await;
    assert!(
        entries.is_err(),
        "equal referent inode does not repair a changed link"
    );
    assert!(
        source.validate_blocking().is_err(),
        "failed scan is terminal"
    );
    assert_eq!(std::fs::read(first).unwrap(), b"original");
    assert_eq!(std::fs::read(second).unwrap(), b"original");
}

#[tokio::test]
async fn selected_followed_file_scan_rejects_foreign_target_ancestor() {
    selected_followed_file_rejects_foreign_target_ancestor(false).await;
}

#[tokio::test]
async fn selected_followed_file_reads_reject_foreign_target_ancestor() {
    selected_followed_file_rejects_foreign_target_ancestor(true).await;
}

async fn selected_followed_file_rejects_foreign_target_ancestor(reads: bool) {
    use crate::endpoint::{existing, Endpoint};
    use std::os::unix::fs::MetadataExt;

    for absolute in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let ancestor = fixture.path().join("a");
        let parent = ancestor.join("p");
        let file = parent.join("file");
        let link = fixture.path().join("selected");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(&file, b"original selected target").unwrap();
        let target = if absolute {
            file.clone()
        } else {
            PathBuf::from("a/p/file")
        };
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let bound = BoundSource::open_local_following(link.clone(), false)
            .await
            .unwrap();
        let SourceShape::Leaf { name } = bound.shape else {
            panic!("expected original selected link name")
        };
        assert_eq!(name.as_path(), Path::new("selected"));
        let source = SourceRoot::from_rooted(bound.rooted).unwrap();
        let rooted = source.rooted();
        let root = stat_fd(rooted.root_fd.as_raw_fd()).unwrap();
        assert_eq!(
            root.st_ino as u64,
            std::fs::metadata(fixture.path()).unwrap().ino()
        );
        let request = crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        let entries = source
            .selected_entries(name.clone(), request, false)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, EntryKind::File);
        // A completed run copies this same original selected-link observation
        // and its target tape; it must neither recapture nor rehome the root.
        let source = source.begin_run().await.unwrap();
        let next_rooted = source.rooted();
        assert!(Arc::ptr_eq(&rooted.root_fd, &next_rooted.root_fd));
        let rooted = next_rooted;
        let entries = source
            .selected_entries(name.clone(), request, false)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            rooted
                .seal_source_inventory_blocking()
                .unwrap()
                .record_count(),
            2
        );
        {
            let mut owner = rooted.source_observations.lock().unwrap();
            let catalogue = owner.active().unwrap();
            let original = catalogue.require(name.as_path()).unwrap();
            assert_eq!(original.named.kind, libc::S_IFLNK as u64);
            assert_eq!(original.effective.kind, libc::S_IFREG as u64);
            assert!(original.followed);
            assert_eq!(
                original.target.as_ref().map(|literal| literal.as_os_str()),
                Some(target.as_os_str())
            );
        }
        let endpoint = source.endpoint();
        let reader = endpoint
            .open_native_file_following(name.as_path())
            .await
            .unwrap()
            .unwrap();
        rooted
            .validate_source_reader_blocking(&name, &reader)
            .unwrap();
        let original_link = NativeObservation::from_stat(
            &stat_at_no_follow(rooted.root_fd.as_raw_fd(), OsStr::new("selected")).unwrap(),
        )
        .unwrap();
        let original_parent = std::fs::metadata(&parent).unwrap();
        let original_file = std::fs::metadata(&file).unwrap();
        let parked = fixture.path().join("original-a");
        std::fs::rename(&ancestor, &parked).unwrap();
        std::fs::create_dir(&ancestor).unwrap();
        std::fs::rename(parked.join("p"), &parent).unwrap();
        let current_link = NativeObservation::from_stat(
            &stat_at_no_follow(rooted.root_fd.as_raw_fd(), OsStr::new("selected")).unwrap(),
        )
        .unwrap();
        assert!(original_link.matches(current_link, true));
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(
            std::fs::metadata(&parent).unwrap().ino(),
            original_parent.ino()
        );
        assert_eq!(std::fs::metadata(&file).unwrap().ino(), original_file.ino());
        assert_eq!(std::fs::read(&file).unwrap(), b"original selected target");
        if reads {
            let error = endpoint
                .open_native_file_following(name.as_path())
                .await
                .unwrap_err();
            let crate::error::SyncError::Io(error) = error else {
                panic!("lost typed source binding error")
            };
            assert!(matches!(
                error
                    .get_ref()
                    .and_then(|cause| cause.downcast_ref::<RootedFsError>()),
                Some(RootedFsError::RootChanged(_))
            ));
            assert!(matches!(
                existing::fingerprint(rooted.clone(), entries[0].clone(), Default::default()).await,
                Err(existing::ExistingDestinationError::Rooted(
                    RootedFsError::RootChanged(_)
                ))
            ));
            assert!(matches!(
                rooted.validate_source_reader_blocking(&name, &reader),
                Err(RootedFsError::RootChanged(_))
            ));
        } else {
            let error = source
                .selected_entries(name, request, false)
                .try_collect::<Vec<_>>()
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<RootedFsError>(),
                    Some(RootedFsError::RootChanged(_))
                ),
                "{error:?}"
            );
        }
    }
}

#[tokio::test]
async fn completed_followed_selected_file_runs_refresh_only_file_data() {
    use crate::endpoint::Endpoint;
    use std::os::unix::fs::MetadataExt;

    for absolute in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let ancestor = fixture.path().join("a");
        let parent = ancestor.join("p");
        let file = parent.join("file");
        let link = fixture.path().join("selected");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(&file, b"first run").unwrap();
        let target = if absolute {
            file.clone()
        } else {
            PathBuf::from("a/p/file")
        };
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let bound = BoundSource::open_local_following(link.clone(), false)
            .await
            .unwrap();
        let SourceShape::Leaf { name } = bound.shape else {
            panic!("expected selected link")
        };
        let mut source = SourceRoot::from_rooted(bound.rooted).unwrap();
        let original_root = source.rooted();
        let request = crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        source
            .selected_entries(name.clone(), request, false)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let original_link = std::fs::symlink_metadata(&link).unwrap();
        let original_parent = std::fs::metadata(&parent).unwrap();
        let original_file = std::fs::metadata(&file).unwrap();
        for (replace, data) in [
            (false, b"fresh in-place DATA".as_slice()),
            (true, b"new FILE object"),
        ] {
            if replace {
                let replacement = parent.join("replacement");
                std::fs::write(&replacement, data).unwrap();
                std::fs::rename(replacement, &file).unwrap();
                assert_ne!(std::fs::metadata(&file).unwrap().ino(), original_file.ino());
            } else {
                std::fs::write(&file, data).unwrap();
                assert_eq!(std::fs::metadata(&file).unwrap().ino(), original_file.ino());
            }
            let next = source.begin_run().await.unwrap();
            let rooted = next.rooted();
            assert!(Arc::ptr_eq(&original_root.root_fd, &rooted.root_fd));
            assert!(!Arc::ptr_eq(
                &source.rooted().source_observations,
                &rooted.source_observations
            ));
            let entries = next
                .selected_entries(name.clone(), request, false)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].size, data.len() as u64);
            assert_eq!(entries[0].kind, EntryKind::File);
            let mut reader = next
                .endpoint()
                .open_native_file_following(name.as_path())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            rooted
                .validate_source_reader_blocking(&name, &reader)
                .unwrap();
            assert_eq!(bytes, data);
            assert_eq!(
                rooted
                    .seal_source_inventory_blocking()
                    .unwrap()
                    .record_count(),
                2
            );
            assert_eq!(
                std::fs::symlink_metadata(&link).unwrap().ino(),
                original_link.ino()
            );
            assert_eq!(std::fs::read_link(&link).unwrap(), target);
            assert_eq!(
                std::fs::metadata(&parent).unwrap().ino(),
                original_parent.ino()
            );
            source = next;
        }
        // New FILE authority must not recapture the original target ancestors.
        let parked = fixture.path().join("original-a");
        std::fs::rename(&ancestor, &parked).unwrap();
        std::fs::create_dir(&ancestor).unwrap();
        std::fs::rename(parked.join("p"), &parent).unwrap();
        assert!(matches!(
            source.begin_run().await,
            Err(RootedFsError::RootChanged(_))
        ));
    }
}

#[tokio::test]
async fn followed_selected_file_same_run_changes_cannot_refresh_failed_authority() {
    use crate::endpoint::Endpoint;

    for case in ["in-place", "replacement", "failed-scan", "scratch"] {
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("file");
        std::fs::write(&file, b"original").unwrap();
        let link = fixture.path().join("selected");
        std::os::unix::fs::symlink("file", &link).unwrap();
        let bound = BoundSource::open_local_following(link, false)
            .await
            .unwrap();
        let SourceShape::Leaf { name } = bound.shape else {
            panic!("expected selected link")
        };
        let source = SourceRoot::from_rooted(bound.rooted).unwrap();
        let request = crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        let reader = if case != "failed-scan" {
            source
                .selected_entries(name.clone(), request, false)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            source
                .endpoint()
                .open_native_file_following(name.as_path())
                .await
                .unwrap()
        } else {
            None
        };
        if case == "replacement" {
            let replacement = fixture.path().join("replacement");
            std::fs::write(&replacement, b"changed DATA").unwrap();
            std::fs::rename(replacement, &file).unwrap();
        } else {
            std::fs::write(&file, b"changed DATA").unwrap();
        }
        if case == "scratch" {
            {
                let rooted = source.rooted();
                let mut owner = rooted.source_observations.lock().unwrap();
                let catalogue = owner.active().unwrap();
                let offset = catalogue.operator.as_ref().unwrap().start;
                catalogue.file.seek(SeekFrom::Start(offset)).unwrap();
                let mut byte = [0];
                catalogue.file.read_exact(&mut byte).unwrap();
                catalogue.file.seek(SeekFrom::Start(offset)).unwrap();
                catalogue.file.write_all(&[byte[0] ^ 1]).unwrap();
            }
            assert!(matches!(source.begin_run().await,
                Err(RootedFsError::Io(error)) if error.kind() == io::ErrorKind::InvalidData));
        } else if let Some(reader) = reader {
            assert!(source
                .rooted()
                .validate_source_reader_blocking(&name, &reader)
                .is_err());
        } else {
            assert!(source
                .selected_entries(name.clone(), request, false)
                .try_collect::<Vec<_>>()
                .await
                .is_err());
        }
        assert!(
            source.begin_run().await.is_err(),
            "{case} must remain terminal"
        );
        std::fs::write(&file, b"original").unwrap();
        assert!(
            source.begin_run().await.is_err(),
            "restoration cannot repair {case}"
        );
    }
}

#[tokio::test]
async fn classified_selected_leaf_cannot_be_recaptured_at_scan() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("file");
    std::fs::write(&path, b"original").unwrap();
    let bound = crate::rooted_fs::operand::BoundSource::open_local(path.clone(), false)
        .await
        .unwrap();
    let SourceShape::Leaf { name } = bound.shape else {
        panic!("expected leaf")
    };
    let source = SourceRoot::from_rooted(bound.rooted).unwrap();
    std::fs::rename(&path, fixture.path().join("held")).unwrap();
    std::fs::write(&path, b"original").unwrap();
    assert!(source
        .selected_entries(name, Default::default(), false)
        .try_collect::<Vec<_>>()
        .await
        .is_err());
    assert_eq!(
        std::fs::read(fixture.path().join("held")).unwrap(),
        b"original"
    );
}

#[tokio::test]
async fn operator_records_are_authenticated_but_not_source_inventory_members() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("ancestor/tree");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("file"), b"original").unwrap();
    let source = SourceRoot::open(path).await.unwrap();
    let entries: Vec<_> = source
        .entries(Default::default())
        .try_collect()
        .await
        .unwrap();
    let rooted = source.rooted();
    {
        let mut owner = rooted.source_observations.lock().unwrap();
        let catalogue = owner.active().unwrap();
        assert_eq!(catalogue.count, 2, "only TREE ROOT and its actual child");
        let offset = catalogue.operator.as_ref().unwrap().start;
        catalogue.file.seek(SeekFrom::Start(offset)).unwrap();
        let mut original = [0];
        catalogue.file.read_exact(&mut original).unwrap();
        catalogue.file.seek(SeekFrom::Start(offset)).unwrap();
        catalogue.file.write_all(&[original[0] ^ 1]).unwrap();
    }
    assert!(rooted.open_regular_blocking(&entries[0].path).is_err());
    assert!(matches!(
        source.validate_blocking(),
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
    assert!(source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .is_err());
}

// CWD is process-global. Exercise authority changes only in isolated children;
// no parallel test can observe this test's chdir/renames.
#[test]
fn captured_cwd_authority_has_native_not_absolute_ancestry() {
    const CHILD: &str = "SY_OPERATOR_CWD_TEST_CHILD";
    const TEST: &str = "rooted_fs::source_observations::operator::tests::captured_cwd_authority_has_native_not_absolute_ancestry";
    let Ok(case) = std::env::var(CHILD) else {
        for case in ["relocate", "foreign-cwd", "parent-traversal", "absolute"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, case)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    let fixture = tempfile::tempdir().unwrap();
    let ancestor = fixture.path().join("ancestor");
    let cwd = ancestor.join("cwd");
    let root = if case == "parent-traversal" {
        ancestor.join("source")
    } else {
        cwd.join("source")
    };
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("file"), b"original").unwrap();
    std::env::set_current_dir(&cwd).unwrap();
    let operand = match case.as_str() {
        "absolute" => root,
        "parent-traversal" => PathBuf::from("../source"),
        _ => PathBuf::from("source"),
    };
    let source = SourceRoot::open_blocking(operand).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let entries: Vec<_> = runtime
        .block_on(async { source.entries(Default::default()).try_collect().await })
        .unwrap();
    let parked = fixture.path().join("parked");
    std::fs::rename(&ancestor, &parked).unwrap();
    std::fs::create_dir(&ancestor).unwrap();
    std::fs::rename(parked.join("cwd"), &cwd).unwrap();
    if case == "parent-traversal" {
        std::fs::rename(parked.join("source"), ancestor.join("source")).unwrap();
    }
    if case == "foreign-cwd" {
        std::env::set_current_dir(fixture.path()).unwrap();
    } else if case == "absolute" {
        // Restore the ORIGINAL absolute ancestry; CWD itself is irrelevant.
        std::fs::rename(&cwd, parked.join("cwd")).unwrap();
        std::fs::remove_dir(&ancestor).unwrap();
        std::fs::rename(&parked, &ancestor).unwrap();
        std::env::set_current_dir(fixture.path()).unwrap();
    }
    if matches!(case.as_str(), "relocate" | "absolute") {
        source.validate_blocking().unwrap();
        let mut bytes = Vec::new();
        source
            .rooted()
            .open_regular_blocking(&entries[0].path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"original");
    } else {
        assert!(matches!(
            source.validate_blocking(),
            Err(RootedFsError::RootChanged(_))
        ));
        assert!(source
            .rooted()
            .open_regular_blocking(&entries[0].path)
            .is_err());
    }
    std::env::set_current_dir(fixture.path()).unwrap();
}

#[test]
fn original_search_only_operator_ancestors_do_not_require_enumeration() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path().join("search-only");
    let root = parent.join("source");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o111)).unwrap();
    let result = SourceRoot::open_blocking(root).and_then(|source| source.validate_blocking());
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
}

#[tokio::test]
async fn race_successor_has_fresh_children_but_old_proofs_stay_terminal() {
    use crate::endpoint::existing;
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("file");
    std::fs::write(&path, b"old").unwrap();
    std::fs::write(fixture.path().join("stable"), b"unchanged observation").unwrap();
    let source = SourceRoot::open(fixture.path().to_path_buf())
        .await
        .unwrap();
    let entries = source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let rooted = source.rooted();
    let inventory = rooted.seal_source_inventory_blocking().unwrap();
    let reader = rooted.open_regular_blocking(&entries[0].path).unwrap();
    std::fs::write(&path, b"changed").unwrap();
    let failed = source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap_err();
    assert!(matches!(
        failed.downcast_ref::<RootedFsError>(),
        Some(RootedFsError::SourceBindingChanged(_))
    ));
    assert!(rooted
        .validate_source_reader_blocking(&entries[0].path, &reader)
        .is_err());
    assert!(source.begin_run().await.is_err());
    std::fs::write(fixture.path().join("arrival"), b"new child").unwrap();
    let next = source.retry_after_source_change().await.unwrap();
    let fresh = next
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(fresh.len(), 3);
    assert!(
        rooted
            .open_regular_blocking(&RelativePath::new("stable").unwrap())
            .is_err(),
        "even unchanged DATA from a failed run must remain terminal"
    );
    assert!(fresh
        .iter()
        .any(|entry| entry.path.as_path() == Path::new("arrival")));
    next.validate().await.unwrap();
    assert!(rooted
        .validate_source_reader_blocking(&entries[0].path, &reader)
        .is_err());
    assert!(inventory.validate_originals_blocking().is_err());
    assert!(
        existing::fingerprint(rooted.clone(), entries[0].clone(), Default::default())
            .await
            .is_err()
    );
    assert!(existing::remove_observed_source_blocking(&rooted, &entries[0]).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"changed");
    // Later lifetime failure invalidates even the earlier failed owner.
    let moved = fixture.path().with_extension("held-retry-test");
    std::fs::rename(fixture.path(), &moved).unwrap();
    std::fs::create_dir(fixture.path()).unwrap();
    assert!(matches!(
        next.validate().await,
        Err(RootedFsError::RootChanged(_))
    ));
    assert!(matches!(
        source.validate().await,
        Err(RootedFsError::RootChanged(_))
    ));
    std::fs::remove_dir(fixture.path()).unwrap();
    std::fs::rename(moved, fixture.path()).unwrap();
    assert!(matches!(
        next.validate().await,
        Err(RootedFsError::RootChanged(_))
    ));
}

#[tokio::test]
async fn cancelled_run_cutoff_is_nonblocking_and_never_enables_successor() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("file"), b"source").unwrap();
    let source = SourceRoot::open(fixture.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let attempt = source.attempt().unwrap();
    let rooted = source.rooted();
    let owner = rooted.source_observations.lock().unwrap();
    // A panic/cancellation cutoff must not wait behind native work's lock.
    drop(attempt);
    drop(owner);
    assert!(matches!(
        source.begin_run().await,
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
    assert!(matches!(
        source.retry_after_source_change().await,
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
    assert!(source.validate().await.is_err());
}

#[tokio::test]
async fn race_successor_rejects_corrupt_scratch_without_repair() {
    for corruption in ["bytes", "truncate", "ordinal"] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("file"), b"old").unwrap();
        let source = SourceRoot::open(fixture.path().to_path_buf())
            .await
            .unwrap();
        let entries = source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        std::fs::write(fixture.path().join("file"), b"new DATA").unwrap();
        let rooted = source.rooted();
        assert!(matches!(
            rooted.validate_source_observation_blocking(&entries[0].path),
            Err(RootedFsError::SourceBindingChanged(_))
        ));
        {
            let mut owner = rooted.source_observations.lock().unwrap();
            let CatalogueState::Active(c) = &mut *owner else {
                panic!("retained originals")
            };
            match corruption {
                "bytes" => {
                    c.file.seek(SeekFrom::Start(0)).unwrap();
                    c.file.write_all(&[255]).unwrap();
                }
                "truncate" => c.file.set_len(c.end - 1).unwrap(),
                "ordinal" => {
                    c.index
                        .set_bytes(inventory::ordinal_key(1), [0; 48])
                        .unwrap();
                }
                _ => unreachable!(),
            }
        }
        assert!(
            source.retry_after_source_change().await.is_err(),
            "{corruption}"
        );
        assert!(matches!(
            source.retry_after_source_change().await,
            Err(RootedFsError::SourceCatalogueTerminal)
        ));
        assert!(source.begin_run().await.is_err());
    }
}

#[tokio::test]
async fn followed_file_race_refreshes_data_not_original_link_or_target_ancestry() {
    for change in ["data", "ancestor", "link"] {
        let fixture = tempfile::tempdir().unwrap();
        let parent = fixture.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let file = parent.join("file");
        let link = fixture.path().join("selected");
        std::fs::write(&file, b"old").unwrap();
        std::os::unix::fs::symlink("parent/file", &link).unwrap();
        let bound = BoundSource::open_local_following(link.clone(), false)
            .await
            .unwrap();
        let SourceShape::Leaf { name } = bound.shape else {
            panic!("leaf")
        };
        let source = SourceRoot::from_rooted(bound.rooted).unwrap();
        let request = crate::engine::scan::ScanRequest {
            follow_symlinks: true,
            ..Default::default()
        };
        source
            .selected_entries(name.clone(), request, false)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        std::fs::write(&file, b"fresh data").unwrap();
        assert!(matches!(
            source.validate().await,
            Err(RootedFsError::SourceBindingChanged(_))
        ));
        assert!(source.begin_run().await.is_err());
        if change == "ancestor" {
            std::fs::rename(&parent, fixture.path().join("held")).unwrap();
            std::fs::create_dir(&parent).unwrap();
            std::fs::rename(fixture.path().join("held/file"), &file).unwrap();
        } else if change == "link" {
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&file, &link).unwrap();
        }
        let result = source.retry_after_source_change().await;
        if change == "data" {
            let next = result.unwrap();
            let entries = next
                .selected_entries(name, request, false)
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(entries[0].size, 10);
            assert!(source.validate().await.is_err());
        } else {
            assert!(
                matches!(result, Err(RootedFsError::RootChanged(_))),
                "{change}"
            );
        }
    }
}
