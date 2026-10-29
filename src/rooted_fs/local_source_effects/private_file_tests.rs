use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::namespace::NamespaceQualification;
use futures::TryStreamExt;
use std::os::unix::fs::MetadataExt;

async fn fixture() -> (tempfile::TempDir, SourceRoot, RootedFs) {
    let fixture = tempfile::tempdir().unwrap();
    let source_path = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    std::fs::create_dir(&source_path).unwrap();
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(source_path.join("original"), b"SOURCE").unwrap();
    std::fs::write(destination.join("target"), b"OLD").unwrap();
    let source = SourceRoot::open(source_path).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    // An alias can continue identifying the held destination after relocation.
    let alias = fixture.path().join("destination-alias");
    std::os::unix::fs::symlink(&destination, &alias).unwrap();
    let rooted = RootedFs::open(alias).await.unwrap();
    let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
    namespace.seal().await.unwrap();
    rooted.local_source_effects = Some(LocalSourceEffects::seal(&source).await.unwrap());
    (fixture, source, rooted)
}

fn stamp(path: &Path) -> [u64; 7] {
    let stat = std::fs::symlink_metadata(path).unwrap();
    [
        stat.ino(),
        stat.nlink(),
        u64::from(stat.mode()),
        stat.size(),
        stat.mtime() as u64,
        stat.ctime() as u64,
        stat.ctime_nsec() as u64,
    ]
}

fn pause(
    rooted: &RootedFs,
    point: PublicationPausePoint,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached, wait) = tokio::sync::oneshot::channel();
    let (resume, receive) = std::sync::mpsc::channel();
    rooted.pause_mutation_at(
        0,
        PublicationPause {
            point,
            reached,
            resume: receive,
        },
    );
    (wait, resume)
}

#[cfg_attr(
    target_os = "linux",
    ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4 or Btrfs"
)]
#[tokio::test]
async fn allocated_private_metadata_does_not_bypass_public_source_census() {
    let (fixture, _source, rooted) = fixture().await;
    let mut staged = rooted
        .begin_staged_file_blocking(&RelativePath::new("target").unwrap())
        .unwrap();
    std::io::Write::write_all(staged.file_mut(), b"NEW").unwrap();
    let source = fixture.path().join("source");
    std::fs::write(source.join("arrival"), b"foreign").unwrap();
    let before = [
        stamp(&source),
        stamp(&source.join("original")),
        stamp(&source.join("arrival")),
    ];
    // A genuine post-seal allocation outside SOURCE needs no unrelated FILE or
    // tree replay for its FD-only setters. Publication still requires both.
    staged.apply_metadata_blocking(Some(0o400), None).unwrap();
    assert_eq!(staged.file.metadata().unwrap().mode() & 0o777, 0o400);
    let error = staged.commit().unwrap_err();
    assert!(
        matches!(error, RootedFsError::SourceBindingChanged(_)),
        "{error}"
    );
    assert_eq!(
        before,
        [
            stamp(&source),
            stamp(&source.join("original")),
            stamp(&source.join("arrival"))
        ]
    );
    assert_eq!(
        std::fs::read(fixture.path().join("destination/target")).unwrap(),
        b"OLD"
    );
    assert_eq!(
        std::fs::read_dir(fixture.path().join("destination"))
            .unwrap()
            .count(),
        1
    );
}

#[cfg_attr(
    target_os = "linux",
    ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4 or Btrfs"
)]
#[tokio::test]
async fn private_registration_and_other_source_owners_do_not_mint_allocation_evidence() {
    for provenance in ["borrowed", "different-owner", "no-owner-at-birth"] {
        let (fixture, _source, mut rooted) = fixture().await;
        let effects = rooted.local_source_effects.clone().unwrap();
        if provenance == "no-owner-at-birth" {
            rooted.local_source_effects = None;
        }
        let mut staged = rooted
            .begin_staged_file_blocking(&RelativePath::new("target").unwrap())
            .unwrap();
        match provenance {
            "borrowed" => {
                let file = staged.try_clone_file().unwrap();
                staged.namespace.register_staging(&file).unwrap();
            }
            "different-owner" => {
                let source = SourceRoot::open(fixture.path().join("source"))
                    .await
                    .unwrap();
                source
                    .entries(Default::default())
                    .try_collect::<Vec<_>>()
                    .await
                    .unwrap();
                let other = LocalSourceEffects::seal(&source).await.unwrap();
                assert!(!effects.same_source(&other));
                staged.namespace.rooted.local_source_effects = Some(other);
            }
            "no-owner-at-birth" => staged.namespace.rooted.local_source_effects = Some(effects),
            _ => unreachable!(),
        }
        std::fs::write(fixture.path().join("source/arrival"), b"foreign").unwrap();
        let before = staged.file.metadata().unwrap().mode();
        assert!(
            matches!(
                staged.apply_metadata_blocking(Some(0o400), None),
                Err(RootedFsError::SourceBindingChanged(_))
            ),
            "{provenance}"
        );
        assert_eq!(staged.file.metadata().unwrap().mode(), before);
        staged.abort().unwrap();
        assert_eq!(
            std::fs::read(fixture.path().join("destination/target")).unwrap(),
            b"OLD"
        );
    }
}

#[cfg_attr(
    target_os = "linux",
    ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4 or Btrfs"
)]
#[tokio::test]
async fn exclusive_allocation_does_not_adopt_a_foreign_link_before_first_stat() {
    let (fixture, _source, rooted) = fixture().await;
    let (wait, resume) = pause(&rooted, PublicationPausePoint::AfterPrivateFileCreation);
    let worker = tokio::task::spawn_blocking(move || {
        rooted.begin_staged_file_blocking(&RelativePath::new("target").unwrap())
    });
    wait.await.unwrap();
    let private = std::fs::read_dir(fixture.path().join("destination"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    let contents = private.join("contents");
    let alias = fixture.path().join("source/alias");
    std::fs::hard_link(&contents, &alias).unwrap();
    let before = stamp(&alias);
    resume.send(()).unwrap();
    let error = worker.await.unwrap().unwrap_err();
    // Cleanup must derive one own link from O_EXCL, not adopt the observed two.
    assert_eq!(stamp(&alias), before);
    assert_eq!(stamp(&contents), before);
    assert_eq!(before[1], 2);
    assert!(
        matches!(error, RootedFsError::StagingOperationAbortFailed { .. }),
        "{error}"
    );
    assert_eq!(
        std::fs::read(fixture.path().join("destination/target")).unwrap(),
        b"OLD"
    );
}

#[cfg_attr(
    target_os = "linux",
    ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4 or Btrfs"
)]
#[tokio::test]
async fn private_metadata_rechecks_single_link_after_source_validation() {
    let (fixture, _source, rooted) = fixture().await;
    let mut staged = rooted
        .begin_staged_file_blocking(&RelativePath::new("target").unwrap())
        .unwrap();
    std::io::Write::write_all(staged.file_mut(), b"NEW").unwrap();
    let contents = fixture
        .path()
        .join("destination")
        .join(&staged.namespace.staging_dir_name)
        .join("contents");
    let (wait, resume) = pause(&rooted, PublicationPausePoint::AfterSourceValidation);
    let worker = tokio::task::spawn_blocking(move || {
        let result = staged.apply_metadata_blocking(Some(0o400), None);
        (staged, result)
    });
    wait.await.unwrap();
    let alias = fixture.path().join("source/alias");
    std::fs::hard_link(&contents, &alias).unwrap();
    let before = stamp(&alias);
    resume.send(()).unwrap();
    let (staged, result) = worker.await.unwrap();
    assert!(
        matches!(result, Err(RootedFsError::StagingEntryChanged(_))),
        "{result:?}"
    );
    assert!(matches!(
        staged.abort(),
        Err(RootedFsError::StagingAbortFailed { .. })
    ));
    assert_eq!(stamp(&alias), before);
    assert_eq!(std::fs::read(&alias).unwrap(), b"NEW");
    assert_eq!(
        std::fs::read(fixture.path().join("destination/target")).unwrap(),
        b"OLD"
    );
}

#[cfg_attr(
    target_os = "linux",
    ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4 or Btrfs"
)]
#[tokio::test]
async fn private_creator_prefix_cannot_follow_destination_root_relocation_into_source() {
    let (fixture, _source, rooted) = fixture().await;
    let mut staged = rooted
        .begin_staged_file_blocking(&RelativePath::new("target").unwrap())
        .unwrap();
    let private_name = staged.namespace.staging_dir_name.clone();
    let (wait, resume) = pause(&rooted, PublicationPausePoint::AfterSourceValidation);
    let worker = tokio::task::spawn_blocking(move || {
        let result = staged.apply_metadata_blocking(Some(0o400), None);
        (staged, result)
    });
    wait.await.unwrap();
    let destination = fixture.path().join("destination");
    let moved = fixture.path().join("source/moved-destination");
    let alias = fixture.path().join("destination-alias");
    std::fs::rename(&destination, &moved).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&moved, &alias).unwrap();
    // Root identity and every relative destination binding still match. They
    // cannot substitute for the creator's qualified current native prefix.
    rooted.verify_root_path_blocking().unwrap();
    rooted
        .verify_namespace_directory(Path::new(""), rooted.root_fd.as_raw_fd())
        .unwrap();
    let contents = moved.join(&private_name).join("contents");
    let before = stamp(&contents);
    resume.send(()).unwrap();
    let (staged, result) = worker.await.unwrap();
    assert!(
        matches!(result, Err(RootedFsError::SourceBindingChanged(_))),
        "{result:?}"
    );
    assert_eq!(stamp(&contents), before);
    assert_eq!(std::fs::read(moved.join("target")).unwrap(), b"OLD");
    std::fs::rename(&moved, &destination).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&destination, &alias).unwrap();
    staged.abort().unwrap();
}

#[cfg_attr(
    target_os = "linux",
    ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4 or Btrfs"
)]
#[tokio::test]
async fn private_allocation_cannot_survive_source_cancellation_or_operator_retarget() {
    for cancel in [true, false] {
        let (fixture, source, rooted) = fixture().await;
        let mut staged = rooted
            .begin_staged_file_blocking(&RelativePath::new("target").unwrap())
            .unwrap();
        let before = staged.file.metadata().unwrap().mode();
        let cancelled = source.rooted().source_run_cancellation(false).unwrap();
        let (wait, resume) = pause(&rooted, PublicationPausePoint::AfterSourceValidation);
        let worker = tokio::task::spawn_blocking(move || {
            let result = staged.apply_metadata_blocking(Some(0o400), None);
            (staged, result)
        });
        wait.await.unwrap();
        if cancel {
            cancelled.store(true, Ordering::Release);
        } else {
            std::fs::rename(
                fixture.path().join("source"),
                fixture.path().join("moved-source"),
            )
            .unwrap();
            std::fs::create_dir(fixture.path().join("source")).unwrap();
        }
        resume.send(()).unwrap();
        let (mut staged, result) = worker.await.unwrap();
        if cancel {
            assert!(
                matches!(result, Err(RootedFsError::SourceCatalogueTerminal)),
                "{result:?}"
            );
            cancelled.store(false, Ordering::Release);
        } else {
            assert!(
                matches!(result, Err(RootedFsError::Io(ref error)) if error.get_ref()
                .and_then(|e| e.downcast_ref::<RootedFsError>()).is_some_and(|e| matches!(e, RootedFsError::RootChanged(_)))),
                "{result:?}"
            );
        }
        assert_eq!(staged.file.metadata().unwrap().mode(), before);
        assert!(staged.apply_metadata_blocking(Some(0o400), None).is_err());
        // SOURCE health cannot revoke genuinely owned abort cleanup.
        staged.abort().unwrap();
        assert_eq!(
            std::fs::read_dir(fixture.path().join("destination"))
                .unwrap()
                .count(),
            1
        );
    }
}
