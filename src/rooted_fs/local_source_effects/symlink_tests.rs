//! Native LINK admission, original ownership, and late namespace races.
use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::namespace::NamespaceQualification;
use futures::TryStreamExt;

#[tokio::test]
async fn private_symlink_metadata_rechecks_creator_binding_and_source_aliases() {
    for relocate in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        let destination_path = fixture.path().join("destination");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&destination_path).unwrap();
        std::fs::write(source_path.join("original"), b"SOURCE").unwrap();
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
                point: PublicationPausePoint::BeforeSymlinkMetadata,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            rooted.publish_symlink_blocking(
                &RelativePath::new("new").unwrap(),
                Path::new("new target"),
                ExpectedDestination::Absent,
                Some(Timestamp::new(100, 0).unwrap()),
            )
        });
        wait.await.unwrap();
        let private = std::fs::read_dir(&destination_path)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let staged = private.join("contents");
        let directory = File::open(&private).unwrap();
        let held = open_symlink_at(directory.as_raw_fd(), OsStr::new("contents")).unwrap();
        let protected = source_path.join("arrival");
        if relocate {
            std::fs::rename(&private, &protected).unwrap();
        } else {
            std::fs::hard_link(&staged, &protected).unwrap();
        }
        let before = stat_fd(held.as_raw_fd()).unwrap();
        resume.send(()).unwrap();
        assert!(work.await.unwrap().is_err());
        let after = stat_fd(held.as_raw_fd()).unwrap();
        assert!(
            staging::same_observation(&before, &after),
            "relocate={relocate}: SOURCE alias was changed or unlinked"
        );
        let link = if relocate {
            protected.join("contents")
        } else {
            protected
        };
        assert_eq!(std::fs::read_link(link).unwrap(), Path::new("new target"));
        assert!(!destination_path.join("new").exists());
        assert_eq!(
            std::fs::read(source_path.join("original")).unwrap(),
            b"SOURCE"
        );
    }
}

#[tokio::test]
async fn symlink_creation_cannot_adopt_foreign_entries_before_its_first_observation() {
    for source_alias in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        let destination_path = fixture.path().join("destination");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&destination_path).unwrap();
        std::os::unix::fs::symlink("new target", source_path.join("link")).unwrap();
        let source = SourceRoot::open(source_path.clone()).await.unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let foreign = if source_alias {
            source_path.join("link")
        } else {
            let foreign = fixture.path().join("foreign");
            std::os::unix::fs::symlink("original target", &foreign).unwrap();
            foreign
        };
        let parent = File::open(foreign.parent().unwrap()).unwrap();
        let held = open_symlink_at(parent.as_raw_fd(), foreign.file_name().unwrap()).unwrap();
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
                point: PublicationPausePoint::AfterSymlinkCreation,
                reached,
                resume: resumed,
            },
        );
        let work = tokio::task::spawn_blocking(move || {
            rooted.publish_symlink_blocking(
                &RelativePath::new("new").unwrap(),
                Path::new("new target"),
                ExpectedDestination::Absent,
                Some(Timestamp::new(100, 0).unwrap()),
            )
        });
        wait.await.unwrap();
        let private = std::fs::read_dir(&destination_path)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let staged = private.join("contents");
        std::fs::rename(&foreign, &staged).unwrap();
        let before = stat_fd(held.as_raw_fd()).unwrap();
        resume.send(()).unwrap();
        assert!(work.await.unwrap().is_err());
        let after = stat_fd(held.as_raw_fd()).unwrap();
        assert!(
            staging::same_observation(&before, &after),
            "foreign SOURCE was changed or unlinked"
        );
        assert_eq!(
            std::fs::read_link(&staged).unwrap(),
            Path::new(if source_alias {
                "new target"
            } else {
                "original target"
            })
        );
        assert!(!destination_path.join("new").exists());
    }
}

#[tokio::test]
async fn symlink_timestamp_uses_the_observed_fd_and_refuses_late_source_aliases() {
    use std::os::unix::fs::MetadataExt;
    for race in ["name", "parent", "root"] {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        let destination_path = fixture.path().join("destination");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&destination_path).unwrap();
        let target = source_path.join("target");
        std::fs::write(&target, b"SOURCE target").unwrap();
        std::os::unix::fs::symlink(&target, source_path.join("link")).unwrap();
        std::fs::create_dir(destination_path.join("dir")).unwrap();
        let destination = destination_path.join("dir/link");
        std::os::unix::fs::symlink(&target, &destination).unwrap();
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
            .qualify(crate::engine::namespace::NamespaceParent::Directory(
                RelativePath::new("dir").unwrap(),
            ))
            .await
            .unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects);
        let path = RelativePath::new("dir/link").unwrap();
        let stamp = Timestamp::new(100, 123).unwrap();
        let state = |path: &Path| {
            let stat = std::fs::symlink_metadata(path).unwrap();
            (
                stat.ino(),
                stat.nlink(),
                stat.mtime(),
                stat.mtime_nsec(),
                stat.ctime(),
                stat.ctime_nsec(),
            )
        };
        let target_before = state(&target);
        let source_before = state(&source_path.join("link"));
        let expected = rooted.path_identity_blocking(&path).unwrap().unwrap().1;
        let expected = rooted
            .apply_observed_preservation_blocking(
                &path,
                EntryKind::Symlink,
                expected,
                None,
                Some(stamp),
                &MetadataPreservation::default(),
            )
            .unwrap();
        assert_eq!(
            state(&target),
            target_before,
            "futimens followed the link target"
        );
        assert_eq!(state(&source_path.join("link")), source_before);
        let changed = std::fs::symlink_metadata(&destination).unwrap();
        assert_eq!((changed.mtime(), changed.mtime_nsec()), (100, 123));

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
            rooted.apply_observed_preservation_blocking(
                &path,
                EntryKind::Symlink,
                expected,
                None,
                Some(Timestamp::new(200, 0).unwrap()),
                &MetadataPreservation::default(),
            )
        });
        wait.await.unwrap();
        let protected = match race {
            "name" => {
                std::fs::remove_file(&destination).unwrap();
                std::fs::hard_link(source_path.join("link"), &destination).unwrap();
                source_path.join("link")
            }
            "parent" => {
                std::fs::rename(destination_path.join("dir"), source_path.join("arrival")).unwrap();
                std::fs::create_dir(destination_path.join("dir")).unwrap();
                source_path.join("arrival/link")
            }
            "root" => {
                std::fs::rename(&destination_path, source_path.join("arrival")).unwrap();
                std::fs::create_dir(&destination_path).unwrap();
                source_path.join("arrival/dir/link")
            }
            _ => unreachable!(),
        };
        let before = state(&protected);
        resume.send(()).unwrap();
        let error = work.await.unwrap().unwrap_err();
        assert!(
            matches!(
                error,
                RootedFsError::DestinationChanged(_) | RootedFsError::RootChanged(_)
            ),
            "{race}: {error}"
        );
        assert_eq!(
            state(&protected),
            before,
            "{race} modified relocated SOURCE"
        );
        assert_eq!(state(&target), target_before);
    }
}
