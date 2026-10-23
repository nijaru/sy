use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::namespace::NamespaceQualification;
use futures::TryStreamExt;

#[cfg(target_os = "macos")]
#[tokio::test]
async fn missing_parent_declaration_neither_creates_nor_adopts_future_directories() {
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(source_dir.path().join("b")).unwrap();
    std::fs::write(source_dir.path().join("b/original"), b"SOURCE").unwrap();
    let source = SourceRoot::open(source_dir.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let rooted = RootedFs::open(destination_dir.path().to_path_buf())
        .await
        .unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    namespace
        .qualify(crate::engine::namespace::NamespaceParent::Directory(
            RelativePath::new("a/b").unwrap(),
        ))
        .await
        .unwrap();
    namespace.seal().await.unwrap();
    let destination = operand::DestinationRoot::Present(rooted.clone());
    let path = RelativePath::new("a/b/new").unwrap();
    effects
        .validate_effect(
            &destination,
            path.clone(),
            SourceEffectKind::LeafPublication,
        )
        .await
        .unwrap();
    assert!(!destination_dir.path().join("a").exists());

    // A declaration is not a lease or a creator receipt for any future parent.
    std::os::unix::fs::symlink(source_dir.path(), destination_dir.path().join("a")).unwrap();
    assert!(
        matches!(effects.validate_effect(&destination, path.clone(), SourceEffectKind::LeafPublication).await,
        Err(RootedFsError::DestinationChanged(changed)) if changed == Path::new("a"))
    );
    rooted.local_source_effects = Some(effects);
    assert!(rooted.begin_staged_file_blocking(&path).is_err());
    assert!(!source_dir.path().join("b/new").exists());
    assert_eq!(
        std::fs::read(source_dir.path().join("b/original")).unwrap(),
        b"SOURCE"
    );
    assert_eq!(
        std::fs::read_dir(source_dir.path().join("b"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn native_staging_repeats_original_validation_without_preflight_permission() {
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("original"), b"source").unwrap();
    let source = SourceRoot::open(source_dir.path().to_path_buf())
        .await
        .unwrap();
    // Preserve the unqualified staging regression with a genuinely opaque
    // original scope; a fully enumerated native TREE now has a positive lane.
    std::fs::create_dir(source_dir.path().join("opaque")).unwrap();
    source
        .entries(crate::engine::scan::ScanRequest {
            max_depth: Some(1),
            ..Default::default()
        })
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let rooted = RootedFs::open(destination_dir.path().to_path_buf())
        .await
        .unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    namespace.seal().await.unwrap();
    rooted.local_source_effects = Some(effects);
    let path = RelativePath::new("publication").unwrap();
    assert!(matches!(
        rooted.begin_staged_file_blocking(&path),
        Err(RootedFsError::SourceEffectUnqualified(_))
    ));
    assert_eq!(
        std::fs::read_dir(destination_dir.path()).unwrap().count(),
        0
    );

    // Late validation cannot adopt even an unselected changed SOURCE record.
    std::fs::write(source_dir.path().join("original"), b"changed SOURCE").unwrap();
    assert!(matches!(
        rooted.begin_staged_file_blocking(&path),
        Err(RootedFsError::SourceBindingChanged(_))
    ));
    assert_eq!(
        std::fs::read_dir(destination_dir.path()).unwrap().count(),
        0
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn directory_timestamps_have_an_inode_only_source_exclusion_footprint() {
    use std::os::unix::fs::MetadataExt;
    fn state(path: &Path) -> [u64; 8] {
        let stat = std::fs::metadata(path).unwrap();
        [
            stat.dev(),
            stat.ino(),
            stat.nlink(),
            stat.mode() as u64,
            stat.mtime() as u64,
            stat.mtime_nsec() as u64,
            stat.ctime() as u64,
            stat.ctime_nsec() as u64,
        ]
    }
    let fixture = tempfile::tempdir().unwrap();
    let ancestor = fixture.path().join("ancestor");
    let source_path = ancestor.join("source");
    std::fs::create_dir_all(&source_path).unwrap();
    std::fs::write(source_path.join("file"), b"SOURCE").unwrap();
    let before = [state(&source_path), state(&source_path.join("file"))];
    let source = SourceRoot::open(source_path.clone()).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    let path = RelativePath::new("ancestor").unwrap();
    let protected = RelativePath::new("ancestor/source").unwrap();
    for directory in [&path, &protected] {
        namespace
            .qualify(crate::engine::namespace::NamespaceParent::Directory(
                directory.clone(),
            ))
            .await
            .unwrap();
    }
    namespace.seal().await.unwrap();
    rooted.local_source_effects = Some(effects);
    let (_, expected) = rooted.path_identity_blocking(&path).unwrap().unwrap();
    let stamp = Timestamp::new(1234, 567_890_123).unwrap();
    let stat = std::fs::metadata(&ancestor).unwrap();
    let access = (stat.atime(), stat.atime_nsec());
    // This IS a SOURCE ancestor. Timestamp-only admission must not pretend it
    // proved ancestry absence, and must not affect any protected descendant.
    rooted
        .finalize_directory_blocking(
            &path,
            expected,
            None,
            Some(stamp),
            &DirectoryPreservation::default(),
        )
        .unwrap();
    let modified = std::fs::metadata(&ancestor).unwrap();
    assert_eq!(
        (modified.mtime(), modified.mtime_nsec()),
        (1234, 567_890_123)
    );
    assert_eq!((modified.atime(), modified.atime_nsec()), access);
    assert_eq!(
        [state(&source_path), state(&source_path.join("file"))],
        before
    );
    source.rooted().verify_source_root_path_blocking().unwrap();
    let target_before = state(&ancestor);
    for (mode, preservation) in [
        (Some(0o700), DirectoryPreservation::default()),
        (
            None,
            DirectoryPreservation {
                xattrs: Some(vec![]),
                ..Default::default()
            },
        ),
        (
            None,
            DirectoryPreservation {
                bsd_flags: Some(0),
                ..Default::default()
            },
        ),
        #[cfg(feature = "acl")]
        (
            None,
            DirectoryPreservation {
                acl: Some(String::new()),
                ..Default::default()
            },
        ),
    ] {
        assert!(matches!(
            rooted.finalize_directory_blocking(&path, expected, mode, Some(stamp), &preservation),
            Err(RootedFsError::SourceEffectUnqualified(_))
        ));
        assert_eq!(state(&ancestor), target_before);
    }
    let (_, identity) = rooted.path_identity_blocking(&protected).unwrap().unwrap();
    assert!(matches!(
        rooted.finalize_directory_blocking(
            &protected,
            identity,
            None,
            Some(stamp),
            &DirectoryPreservation::default()
        ),
        Err(RootedFsError::SourceEffectUnqualified(_))
    ));
    assert_eq!(
        [state(&source_path), state(&source_path.join("file"))],
        before
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn directory_timestamp_rechecks_destination_after_source_census() {
    use std::os::unix::fs::MetadataExt;
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("file"), b"SOURCE").unwrap();
    let target = destination_dir.path().join("dir");
    std::fs::create_dir(&target).unwrap();
    let source = SourceRoot::open(source_dir.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let rooted = RootedFs::open(destination_dir.path().to_path_buf())
        .await
        .unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    let path = RelativePath::new("dir").unwrap();
    namespace
        .qualify(crate::engine::namespace::NamespaceParent::Directory(
            path.clone(),
        ))
        .await
        .unwrap();
    namespace.seal().await.unwrap();
    rooted.local_source_effects = Some(effects);
    let (_, expected) = rooted.path_identity_blocking(&path).unwrap().unwrap();
    let (reached, wait) = tokio::sync::oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    rooted.pause_mutation_at(
        0,
        PublicationPause {
            point: PublicationPausePoint::AfterSourceValidation,
            reached,
            resume: resumed,
        },
    );
    let work = tokio::task::spawn_blocking(move || {
        rooted.finalize_directory_blocking(
            &path,
            expected,
            None,
            Some(Timestamp::new(1234, 0).unwrap()),
            &DirectoryPreservation::default(),
        )
    });
    wait.await.unwrap();
    let protected = source_dir.path().join("arrival");
    std::fs::rename(&target, &protected).unwrap();
    std::fs::create_dir(&target).unwrap();
    let before = std::fs::metadata(&protected).unwrap();
    resume.send(()).unwrap();
    let error = work.await.unwrap().unwrap_err();
    assert!(
        matches!(error, RootedFsError::DestinationChanged(_)),
        "{error}"
    );
    let after = std::fs::metadata(&protected).unwrap();
    assert_eq!(
        (
            after.ino(),
            after.nlink(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec()
        ),
        (
            before.ino(),
            before.nlink(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec()
        )
    );
    assert_eq!(
        std::fs::read(source_dir.path().join("file")).unwrap(),
        b"SOURCE"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn selected_late_foreign_alias_or_binding_change_precedes_native_publication() {
    for foreign_alias in [true, false] {
        let fixture = tempfile::tempdir().unwrap();
        let original = fixture.path().join("foo");
        std::fs::write(&original, b"SOURCE").unwrap();
        let source = SourceRoot::open(fixture.path().to_path_buf())
            .await
            .unwrap();
        source
            .selected_entries(RelativePath::new("foo").unwrap(), Default::default(), false)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let effects = LocalSourceEffects::seal(&source).await.unwrap();
        let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
        let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects);
        let path = RelativePath::new("bar").unwrap();
        let mut staged = rooted.begin_staged_file_blocking(&path).unwrap();
        std::io::Write::write_all(staged.file_mut(), b"staged").unwrap();
        staged.prepare_publication_blocking().unwrap();
        if foreign_alias {
            std::fs::hard_link(&original, fixture.path().join("foreign")).unwrap();
        } else {
            std::fs::rename(&original, fixture.path().join("saved")).unwrap();
            std::fs::write(&original, b"foreign binding").unwrap();
        }
        let error = staged.commit().unwrap_err();
        // Original SOURCE failure still blocks publication, but does not revoke
        // creation-time ownership of untouched, disjoint private staging.
        assert!(
            matches!(error, RootedFsError::SourceBindingChanged(_)),
            "{error}"
        );
        assert!(!fixture.path().join("bar").exists());
        assert!(std::fs::read_dir(fixture.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".sy-stage-")
        }));
        if foreign_alias {
            assert_eq!(std::fs::read(&original).unwrap(), b"SOURCE");
            assert_eq!(
                std::fs::read(fixture.path().join("foreign")).unwrap(),
                b"SOURCE"
            );
        } else {
            assert_eq!(std::fs::read(&original).unwrap(), b"foreign binding");
            assert_eq!(
                std::fs::read(fixture.path().join("saved")).unwrap(),
                b"SOURCE"
            );
        }
    }
}
#[tokio::test]
async fn pending_root_guard_precedes_permanent_parent_creation() {
    let fixture = tempfile::tempdir().unwrap();
    let source_dir = fixture.path().join("source");
    std::fs::create_dir(&source_dir).unwrap();
    std::fs::write(source_dir.join("original"), b"source").unwrap();
    let source = SourceRoot::open(source_dir).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let bound = operand::BoundDestination::open(
        fixture.path().join("missing/root"),
        operand::SourceShape::Directory { basename: None },
    )
    .await
    .unwrap();
    let (bound, namespace) = bound.root.with_namespace_qualification().await.unwrap();
    namespace.seal().await.unwrap();
    let bound = bound.with_local_source_effects(effects);
    assert!(matches!(
        bound.acquire_local().await,
        Err(RootedFsError::SourceEffectUnqualified(_))
    ));
    assert!(!fixture.path().join("missing").exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn regular_metadata_needs_both_object_absence_and_the_actual_named_fd() {
    let fixture = tempfile::tempdir().unwrap();
    let source_dir = fixture.path().join("source");
    let destination_dir = fixture.path().join("destination");
    std::fs::create_dir(&source_dir).unwrap();
    std::fs::create_dir(&destination_dir).unwrap();
    std::fs::write(source_dir.join("original"), b"SOURCE").unwrap();
    std::fs::hard_link(source_dir.join("original"), destination_dir.join("alias")).unwrap();
    std::fs::write(destination_dir.join("old"), b"OLD").unwrap();
    std::fs::write(destination_dir.join("foreign"), b"foreign").unwrap();
    let source = SourceRoot::open(source_dir).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let parent = File::open(&destination_dir).unwrap();
    for (name, object_name, allowed) in [
        ("old", "old", true),
        ("old", "foreign", false),
        ("alias", "alias", false),
    ] {
        let object = File::open(destination_dir.join(object_name)).unwrap();
        let result = effects.admit_native_effect_blocking(SourceEffect {
            kind: SourceEffectKind::InodeMetadata,
            parent: parent.as_raw_fd(),
            name: OsStr::new(name),
            object: Some(object.as_raw_fd()),
            path: Path::new(name),
        });
        assert_eq!(result.is_ok(), allowed, "{name}/{object_name}: {result:?}");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn link_delta_admission_needs_actual_named_regular_fd_and_source_absence() {
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("original"), b"SOURCE").unwrap();
    std::fs::hard_link(
        source_dir.path().join("original"),
        destination_dir.path().join("alias"),
    )
    .unwrap();
    std::fs::write(destination_dir.path().join("owned"), b"created bytes").unwrap();
    std::fs::create_dir(destination_dir.path().join("directory")).unwrap();
    let source = SourceRoot::open(source_dir.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let parent = File::open(destination_dir.path()).unwrap();
    for (name, object, allowed) in [
        ("owned", Some("owned"), true),
        ("owned", None, false),
        ("owned", Some("alias"), false),
        ("alias", Some("alias"), false),
        ("directory", Some("directory"), false),
        ("absent", Some("owned"), false),
    ] {
        let object = object.map(|name| File::open(destination_dir.path().join(name)).unwrap());
        let result = effects.admit_native_effect_blocking(SourceEffect {
            kind: SourceEffectKind::LinkCountChange,
            parent: parent.as_raw_fd(),
            name: OsStr::new(name),
            object: object.as_ref().map(AsRawFd::as_raw_fd),
            path: Path::new(name),
        });
        if allowed {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(RootedFsError::SourceEffectUnqualified(_))),
                "{name}: {result:?}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn mandatory_tree_census_io_error_is_not_an_absence_certificate() {
    use std::os::unix::fs::PermissionsExt;
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir().unwrap();
    let source = SourceRoot::open(source_dir.path().to_path_buf())
        .await
        .unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let rooted = RootedFs::open(destination_dir.path().to_path_buf())
        .await
        .unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    namespace.seal().await.unwrap();
    rooted.local_source_effects = Some(effects.clone());
    let path = RelativePath::new("publication").unwrap();
    let mut staged = rooted.begin_staged_file_blocking(&path).unwrap();
    std::io::Write::write_all(staged.file_mut(), b"staged").unwrap();
    staged.prepare_publication_blocking().unwrap();
    std::fs::set_permissions(source_dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    effects.validate_originals_blocking().unwrap();
    let result = staged.commit();
    std::fs::set_permissions(source_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err();
    assert!(
        error
            .to_string()
            .to_lowercase()
            .contains("permission denied"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_dir(destination_dir.path()).unwrap().count(),
        0
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn tree_late_new_name_cannot_hide_behind_unchanged_originals() {
    for hardlink in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let source_dir = fixture.path().join("source");
        let destination_dir = fixture.path().join("destination");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&destination_dir).unwrap();
        std::fs::write(source_dir.join("original"), b"SOURCE").unwrap();
        std::fs::write(destination_dir.join("old"), b"OLD").unwrap();
        let source = SourceRoot::open(source_dir.clone()).await.unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let effects = LocalSourceEffects::seal(&source).await.unwrap();
        let rooted = RootedFs::open(destination_dir.clone()).await.unwrap();
        let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects.clone());
        let path = RelativePath::new("publication").unwrap();
        let mut staged = rooted.begin_staged_file_blocking(&path).unwrap();
        std::io::Write::write_all(staged.file_mut(), b"staged").unwrap();
        staged.prepare_publication_blocking().unwrap();
        let old_stamp = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(&source_dir).unwrap(),
        );
        if hardlink {
            std::fs::hard_link(destination_dir.join("old"), source_dir.join("arrival")).unwrap();
        } else {
            std::fs::write(source_dir.join("arrival"), b"foreign").unwrap();
        }
        filetime::set_file_mtime(&source_dir, old_stamp).unwrap();
        // Every original still matches, including the deliberately stamp-blind
        // directory observation. Only mandatory exact enumeration sees arrival.
        effects.validate_originals_blocking().unwrap();
        let error = staged.commit().unwrap_err();
        assert!(error.to_string().contains("source"), "{error}");
        assert_eq!(std::fs::read_dir(&destination_dir).unwrap().count(), 1);
        assert!(!destination_dir.join("publication").exists());
        assert_eq!(
            std::fs::read(source_dir.join("original")).unwrap(),
            b"SOURCE"
        );
        assert_eq!(std::fs::read(destination_dir.join("old")).unwrap(), b"OLD");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn old_state_is_rechecked_after_source_census_before_retirement_or_setters() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for metadata_only in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let destination_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("original"), b"SOURCE").unwrap();
        let old = destination_dir.path().join("old");
        std::fs::write(&old, b"OLD").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o640)).unwrap();
        let source = SourceRoot::open(source_dir.path().to_path_buf())
            .await
            .unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let effects = LocalSourceEffects::seal(&source).await.unwrap();
        let rooted = RootedFs::open(destination_dir.path().to_path_buf())
            .await
            .unwrap();
        let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects);
        let path = RelativePath::new("old").unwrap();
        let expected = rooted.path_identity_blocking(&path).unwrap().unwrap().1;
        let stage = if metadata_only {
            None
        } else {
            let mut stage = rooted.begin_staged_file_blocking(&path).unwrap();
            std::io::Write::write_all(stage.file_mut(), b"NEW").unwrap();
            stage.prepare_publication_blocking().unwrap();
            Some(stage)
        };
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            usize::from(!metadata_only),
            PublicationPause {
                point: PublicationPausePoint::AfterSourceValidation,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            if let Some(stage) = stage {
                stage.commit().map(|_| ())
            } else {
                rooted
                    .apply_observed_preservation_blocking(
                        &path,
                        EntryKind::File,
                        expected,
                        Some(0o600),
                        None,
                        &MetadataPreservation::default(),
                    )
                    .map(|_| ())
            }
        });
        wait.await.unwrap();
        // The new alias is in a directory already exhausted by census. Original
        // SOURCE records still match, but the captured OLD state is now stale.
        let alias = source_dir.path().join("arrival");
        std::fs::hard_link(&old, &alias).unwrap();
        let before = std::fs::metadata(&alias).unwrap();
        resume.send(()).unwrap();
        let error = work.await.unwrap().unwrap_err();
        assert!(
            matches!(error, RootedFsError::DestinationChanged(_)),
            "{error}"
        );
        let after = std::fs::metadata(&alias).unwrap();
        assert_eq!(
            (
                before.ino(),
                before.nlink(),
                before.ctime(),
                before.ctime_nsec(),
                before.mode()
            ),
            (
                after.ino(),
                after.nlink(),
                after.ctime(),
                after.ctime_nsec(),
                after.mode()
            ),
        );
        assert_eq!(std::fs::read(&old).unwrap(), b"OLD");
        assert_eq!(std::fs::read(&alias).unwrap(), b"OLD");
        assert_eq!(
            std::fs::read_dir(destination_dir.path()).unwrap().count(),
            1
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn terminal_source_does_not_revoke_owned_private_abort_or_drop_cleanup() {
    for action in ["abort", "drop", "metadata", "commit", "empty-directory"] {
        let source_dir = tempfile::tempdir().unwrap();
        let destination_dir = tempfile::tempdir().unwrap();
        let original = source_dir.path().join("original");
        std::fs::write(&original, b"SOURCE").unwrap();
        let source = SourceRoot::open(source_dir.path().to_path_buf())
            .await
            .unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let effects = LocalSourceEffects::seal(&source).await.unwrap();
        let rooted = RootedFs::open(destination_dir.path().to_path_buf())
            .await
            .unwrap();
        let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects.clone());
        let path = RelativePath::new("publication").unwrap();
        if action == "empty-directory" {
            let namespace = rooted
                .begin_namespace_blocking(
                    path.as_path(),
                    ExpectedDestination::Absent,
                    StagingOrigin::Created,
                )
                .unwrap();
            std::fs::write(&original, b"changed SOURCE").unwrap();
            assert!(matches!(
                effects.validate_originals_blocking(),
                Err(RootedFsError::SourceBindingChanged(_))
            ));
            namespace.abort().unwrap();
        } else {
            let mut staged = rooted.begin_staged_file_blocking(&path).unwrap();
            std::io::Write::write_all(staged.file_mut(), b"staged").unwrap();
            std::fs::write(&original, b"changed SOURCE").unwrap();
            assert!(matches!(
                effects.validate_originals_blocking(),
                Err(RootedFsError::SourceBindingChanged(_))
            ));
            assert!(matches!(
                effects.validate_originals_blocking(),
                Err(RootedFsError::SourceRaceCutoff)
            ));
            match action {
                "abort" => staged.abort().unwrap(),
                "drop" => drop(staged),
                "metadata" => {
                    assert!(matches!(
                        staged.apply_metadata_blocking(Some(0o644), None),
                        Err(RootedFsError::SourceRaceCutoff)
                    ));
                    drop(staged);
                }
                "commit" => assert!(matches!(
                    staged.commit(),
                    Err(RootedFsError::SourceRaceCutoff)
                )),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            std::fs::read_dir(destination_dir.path()).unwrap().count(),
            0,
            "{action}"
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"changed SOURCE");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn acquired_private_directory_cannot_adopt_original_source_before_first_stat() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for mode in [0o700, 0o755] {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        let destination_path = fixture.path().join("destination");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&destination_path).unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(mode)).unwrap();
        let held = File::open(&source_path).unwrap();
        xattr::set(&source_path, "user.sy-source-birth", b"original").unwrap();
        #[cfg(feature = "acl")]
        acl_macos::write_fd_entries(
            held.as_raw_fd(),
            &[exacl::AclEntry::allow_group(
                "abcdefab-cdef-abcd-efab-cdef0000000c",
                exacl::Perm::READ,
                exacl::Flag::empty(),
            )],
        )
        .unwrap();
        let source = SourceRoot::open(source_path.clone()).await.unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let effects = LocalSourceEffects::seal(&source).await.unwrap();
        let rooted = RootedFs::open(destination_path.clone()).await.unwrap();
        let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects);
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        rooted.pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterPrivateDirectoryCreation,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            rooted
                .begin_staged_file_blocking(&RelativePath::new("file").unwrap())
                .map(drop)
        });
        wait.await.unwrap();
        let private = std::fs::read_dir(&destination_path)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::rename(&private, fixture.path().join("genuine-private")).unwrap();
        std::fs::rename(&source_path, &private).unwrap();
        let before = held.metadata().unwrap();
        let before_xattrs = read_xattrs_from_file(&held).unwrap();
        #[cfg(feature = "acl")]
        let before_acl = read_acl_entries_fd(&held).unwrap();
        resume.send(()).unwrap();
        let result = work.await.unwrap();
        assert!(result.is_err(), "{result:?}");
        let after = held.metadata().unwrap();
        assert_eq!(
            (
                before.ino(),
                before.nlink(),
                before.mode(),
                before.ctime(),
                before.ctime_nsec()
            ),
            (
                after.ino(),
                after.nlink(),
                after.mode(),
                after.ctime(),
                after.ctime_nsec()
            ),
        );
        assert_eq!(read_xattrs_from_file(&held).unwrap(), before_xattrs);
        #[cfg(feature = "acl")]
        assert_eq!(read_acl_entries_fd(&held).unwrap(), before_acl);
        assert!(private.is_dir());
        assert_eq!(std::fs::read_dir(&private).unwrap().count(), 0);
        assert!(!destination_path.join("file").exists());
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn private_cleanup_rechecks_enclosing_destination_ancestors() {
    use crate::engine::namespace::NamespaceParent;
    use std::os::unix::fs::MetadataExt;
    let fixture = tempfile::tempdir().unwrap();
    let source_path = fixture.path().join("source");
    let destination_path = fixture.path().join("destination");
    std::fs::create_dir(&source_path).unwrap();
    std::fs::create_dir_all(destination_path.join("a/b")).unwrap();
    let source = SourceRoot::open(source_path.clone()).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let rooted = RootedFs::open(destination_path.clone()).await.unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    namespace
        .qualify(NamespaceParent::Directory(
            RelativePath::new("a/b").unwrap(),
        ))
        .await
        .unwrap();
    namespace.seal().await.unwrap();
    rooted.local_source_effects = Some(effects);
    let stage = rooted
        .begin_staged_file_blocking(&RelativePath::new("a/b/file").unwrap())
        .unwrap();
    let private_name = stage.namespace.staging_dir_name.clone();
    let parking = fixture.path().join("parking");
    std::fs::rename(destination_path.join("a/b"), &parking).unwrap();
    std::fs::remove_dir(destination_path.join("a")).unwrap();
    // Preserve the exact DST root, final parent and private directory, but put
    // original SOURCE around them. Checking only the final parent misses this.
    std::fs::rename(&source_path, destination_path.join("a")).unwrap();
    std::fs::rename(&parking, destination_path.join("a/b")).unwrap();
    let protected = destination_path
        .join("a/b")
        .join(private_name)
        .join("contents");
    let before = std::fs::metadata(&protected).unwrap();
    let result = stage.abort();
    assert!(
        matches!(result, Err(RootedFsError::StagingAbortFailed { .. })),
        "{result:?}"
    );
    let after = std::fs::metadata(&protected).unwrap();
    assert_eq!(
        (
            before.ino(),
            before.nlink(),
            before.ctime(),
            before.ctime_nsec()
        ),
        (
            after.ino(),
            after.nlink(),
            after.ctime(),
            after.ctime_nsec()
        ),
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn private_cleanup_refuses_outside_source_alias_and_relocated_creator_directory() {
    use std::os::unix::fs::MetadataExt;
    for relocation in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let destination_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("original"), b"SOURCE").unwrap();
        let source = SourceRoot::open(source_dir.path().to_path_buf())
            .await
            .unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let effects = LocalSourceEffects::seal(&source).await.unwrap();
        let rooted = RootedFs::open(destination_dir.path().to_path_buf())
            .await
            .unwrap();
        let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects.clone());
        let path = RelativePath::new("publication").unwrap();
        let mut staged = rooted.begin_staged_file_blocking(&path).unwrap();
        std::io::Write::write_all(staged.file_mut(), b"staged").unwrap();
        let private = destination_dir
            .path()
            .join(&staged.namespace.staging_dir_name);
        let foreign = source_dir.path().join("foreign");
        let protected = if relocation {
            std::fs::rename(&private, &foreign).unwrap();
            // Also leave a same-name foreign replacement at the original DST.
            std::fs::create_dir(&private).unwrap();
            std::fs::write(private.join("contents"), b"foreign DST").unwrap();
            foreign.join("contents")
        } else {
            std::fs::hard_link(private.join("contents"), &foreign).unwrap();
            foreign.clone()
        };
        // Poison SOURCE explicitly; cleanup must use neither recapture nor a
        // terminal-catalogue waiver as an excuse to unlink the foreign alias.
        assert!(effects
            .admit_native_effect_blocking(SourceEffect {
                kind: SourceEffectKind::LeafPublication,
                parent: staged.namespace.parent_fd.as_raw_fd(),
                name: &staged.namespace.destination_name,
                object: None,
                path: path.as_path(),
            })
            .is_err());
        let before = std::fs::metadata(&protected).unwrap();
        assert!(matches!(
            staged.abort(),
            Err(RootedFsError::StagingAbortFailed { .. })
        ));
        let after = std::fs::metadata(&protected).unwrap();
        assert_eq!(
            (
                before.ino(),
                before.nlink(),
                before.ctime(),
                before.ctime_nsec()
            ),
            (
                after.ino(),
                after.nlink(),
                after.ctime(),
                after.ctime_nsec()
            )
        );
        assert_eq!(std::fs::read(&protected).unwrap(), b"staged");
        assert_eq!(
            std::fs::read(private.join("contents")).unwrap(),
            if relocation {
                b"foreign DST".as_slice()
            } else {
                b"staged".as_slice()
            }
        );
        assert!(!destination_dir.path().join("publication").exists());
    }
}
