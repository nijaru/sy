//! Native regressions for publication seals and private cleanup ownership.
use super::*;
use std::io::{Seek, SeekFrom, Write};

fn relative(path: &str) -> RelativePath {
    RelativePath::new(path).unwrap()
}

#[test]
fn failed_staging_descriptor_duplication_does_not_authorize_cleanup() {
    const CHILD: &str = "SY_STAGING_DUPLICATION_FAILURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rooted_fs::staging_tests::failed_staging_descriptor_duplication_does_not_authorize_cleanup",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Isolate descriptor exhaustion from the parallel test harness.
    let root = tempfile::tempdir().unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let mut namespace = rooted
        .begin_namespace_blocking(Path::new("target"), ExpectedDestination::Absent)
        .unwrap();
    let contents = root
        .path()
        .join(&namespace.staging_dir_name)
        .join("contents");
    std::os::unix::fs::symlink("owned", &contents).unwrap();
    let held = open_symlink_at(namespace.staging_dir_fd.as_raw_fd(), &namespace.temp_name).unwrap();
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limits is writable storage; the child changes only its own soft
    // descriptor limit and exits without affecting the parent test process.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
        0
    );
    limits.rlim_cur = limits.rlim_cur.min(64);
    // SAFETY: the initialized limit retains the existing hard limit.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) }, 0);
    let mut occupied = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => occupied.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    assert!(
        matches!(namespace.register_staging(&held), Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::EMFILE))
    );
    drop(occupied);
    drop(held);
    // Even if the cached inode numbers still match, no retained descriptor
    // pins their identity. Refuse cleanup rather than assume no inode reuse.
    assert!(matches!(
        namespace.abort(),
        Err(RootedFsError::StagingAbortFailed { .. })
    ));
    assert_eq!(std::fs::read_link(&contents).unwrap(), Path::new("owned"));
}

#[test]
fn staged_verification_and_unverified_preparation_cannot_adopt_later_fd_edits() {
    for verify in [true, false] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("target"), b"old").unwrap();
        let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("target"))
            .unwrap();
        staged.file_mut().write_all(b"verified").unwrap();
        staged.apply_metadata_blocking(Some(0o600), None).unwrap();
        if verify {
            assert_eq!(
                staged.staged_hash_blocking().unwrap(),
                blake3::hash(b"verified")
            );
        } else {
            staged.prepare_publication_blocking().unwrap();
        }
        let mut duplicate = staged.try_clone_file().unwrap();
        duplicate.seek(SeekFrom::Start(0)).unwrap();
        duplicate.write_all(b"tampered").unwrap();
        assert!(matches!(
            staged.commit(),
            Err(RootedFsError::StagingEntryChanged(_))
        ));
        assert_eq!(std::fs::read(root.path().join("target")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn late_staging_name_substitution_never_overwrites_or_cleans_foreign_data() {
    for cancel in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("target"), b"old").unwrap();
        let mut rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let admission = Arc::new(PublicationAdmission::default());
        rooted.bind_session_mutations(Arc::clone(&admission), false);
        let mut staged = rooted
            .begin_staged_file_blocking(&relative("target"))
            .unwrap();
        staged.file_mut().write_all(b"original").unwrap();
        let private = root.path().join(&staged.namespace.staging_dir_name);
        let contents = private.join("contents");
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        staged.pause_publication(PublicationPause {
            point: PublicationPausePoint::BeforeAdmission,
            reached: reached_tx,
            resume: resume_rx,
        });
        let worker = tokio::task::spawn_blocking(move || staged.commit());
        let reached = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
        let substitute = (|| -> std::io::Result<()> {
            std::fs::rename(&contents, root.path().join("original"))?;
            std::fs::write(&contents, b"foreign")
        })();
        if cancel {
            admission.close();
        }
        let _ = resume_tx.send(());
        let result = worker.await.unwrap();
        reached.unwrap().unwrap();
        substitute.unwrap();
        assert_eq!(std::fs::read(root.path().join("target")).unwrap(), b"old");
        assert!(matches!(
            result,
            Err(RootedFsError::StagingOperationAbortFailed { .. })
        ));
        assert_eq!(
            std::fs::read(root.path().join("original")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read(&contents).unwrap(), b"foreign");
        assert!(private.is_dir());
    }
}

#[tokio::test]
async fn admitted_staging_fd_edit_does_not_produce_a_completion_proof() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target"), b"old").unwrap();
    let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
    let mut staged = rooted
        .begin_staged_file_blocking(&relative("target"))
        .unwrap();
    staged.file_mut().write_all(b"original").unwrap();
    staged.staged_hash_blocking().unwrap();
    let mut duplicate = staged.try_clone_file().unwrap();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    staged.pause_publication(PublicationPause {
        point: PublicationPausePoint::AfterAdmission,
        reached: reached_tx,
        resume: resume_rx,
    });
    let worker = tokio::task::spawn_blocking(move || {
        staged
            .commit()
            .and_then(|pending| pending.finalize_blocking(None))
    });
    let reached = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
    let edit = duplicate
        .seek(SeekFrom::Start(0))
        .and_then(|_| duplicate.write_all(b"tampered"));
    let _ = resume_tx.send(());
    let result = worker.await.unwrap();
    reached.unwrap().unwrap();
    edit.unwrap();
    assert!(matches!(result, Err(RootedFsError::StagingEntryChanged(_))));
    assert_eq!(std::fs::read(root.path().join("target")).unwrap(), b"old");
}

#[test]
fn abort_and_drop_retain_substituted_entries_for_file_and_symlink_owners() {
    for symlink in [false, true] {
        for explicit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
            let staged = if symlink {
                rooted
                    .begin_staged_symlink_blocking(
                        &relative("target"),
                        Path::new("original"),
                        ExpectedDestination::Absent,
                        None,
                    )
                    .unwrap()
            } else {
                rooted
                    .begin_staged_file_blocking(&relative("target"))
                    .unwrap()
                    .namespace
            };
            let private = root.path().join(&staged.staging_dir_name);
            let contents = private.join("contents");
            std::fs::rename(&contents, root.path().join("original")).unwrap();
            std::fs::write(&contents, b"foreign").unwrap();
            if explicit {
                assert!(matches!(
                    staged.abort(),
                    Err(RootedFsError::StagingAbortFailed { .. })
                ));
            } else {
                drop(staged);
            }
            assert_eq!(std::fs::read(&contents).unwrap(), b"foreign");
            assert!(private.is_dir());
        }
    }
}

#[tokio::test]
async fn hardlink_publication_checks_private_name_and_retains_foreign_substitution() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source"), b"original").unwrap();
    std::fs::write(root.path().join("target"), b"old").unwrap();
    let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
    let expected = rooted
        .path_identity_blocking(&relative("source"))
        .unwrap()
        .unwrap()
        .1;
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    rooted.pause_mutation_at(
        1,
        PublicationPause {
            point: PublicationPausePoint::BeforeAdmission,
            reached: reached_tx,
            resume: resume_rx,
        },
    );
    let worker = tokio::task::spawn_blocking(move || {
        rooted.publish_hardlink_blocking(
            &relative("source"),
            &relative("target"),
            expected,
            ExpectedDestination::SnapshotAtOpen,
        )
    });
    let reached = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
    let substitute = (|| -> std::io::Result<PathBuf> {
        let private = std::fs::read_dir(root.path())?
            .find_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".sy-stage-")
                    .then(|| entry.path())
            })
            .ok_or_else(|| std::io::Error::other("missing private staging"))?;
        std::fs::rename(private.join("contents"), root.path().join("owned-link"))?;
        std::fs::write(private.join("contents"), b"foreign")?;
        Ok(private)
    })();
    let _ = resume_tx.send(());
    let result = worker.await.unwrap();
    reached.unwrap().unwrap();
    let private = substitute.unwrap();
    assert_eq!(std::fs::read(root.path().join("target")).unwrap(), b"old");
    assert!(matches!(
        result,
        Err(RootedFsError::StagingOperationAbortFailed { .. })
    ));
    assert_eq!(
        std::fs::read(root.path().join("source")).unwrap(),
        b"original"
    );
    assert_eq!(std::fs::read(private.join("contents")).unwrap(), b"foreign");
}

fn private_directory(rooted: &RootedFs) -> RootedNamespaceTransaction {
    let mut namespace = rooted
        .begin_namespace_blocking(Path::new("target"), ExpectedDestination::SnapshotAtOpen)
        .unwrap();
    let name = component_cstring(&namespace.temp_name).unwrap();
    // SAFETY: the held directory and live single-component name create only a
    // private fixture directory, never a visible destination or recursive tree.
    assert_eq!(
        unsafe { libc::mkdirat(namespace.staging_dir_fd.as_raw_fd(), name.as_ptr(), 0o700) },
        0
    );
    let file: File = open_dir_at(namespace.staging_dir_fd.as_raw_fd(), &namespace.temp_name)
        .unwrap()
        .into();
    namespace.register_staging(&file).unwrap();
    namespace
}

#[test]
fn exchanged_old_cleanup_requires_ownership_for_directories_and_single_link_files() {
    // The public directory-subtree transaction is not implemented yet. Exercise
    // the common namespace owner directly for its nondirectory-to-directory half.
    for old_directory in [true, false] {
        let root = tempfile::tempdir().unwrap();
        if old_directory {
            std::fs::create_dir(root.path().join("target")).unwrap();
        } else {
            std::fs::write(root.path().join("target"), b"old bytes").unwrap();
        }
        let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
        let mut namespace = if old_directory {
            let mut staged = rooted
                .begin_staged_file_blocking(&relative("target"))
                .unwrap();
            staged.file_mut().write_all(b"new").unwrap();
            staged.namespace
        } else {
            private_directory(&rooted)
        };
        let private = root.path().join(&namespace.staging_dir_name);
        namespace.seal_staging().unwrap();
        let mut lineage = rooted.retirement.lock().unwrap();
        namespace
            .publish_blocking(None, None, &mut lineage, || Ok(()))
            .unwrap();
        std::fs::rename(private.join("contents"), root.path().join("old")).unwrap();
        std::fs::write(private.join("contents"), b"foreign").unwrap();
        assert!(matches!(
            namespace.finish_commit_blocking(&mut lineage),
            Err(RootedFsError::CommittedCleanupPending { .. })
        ));
        drop(namespace);
        assert_eq!(std::fs::read(private.join("contents")).unwrap(), b"foreign");
        if old_directory {
            assert_eq!(std::fs::read(root.path().join("target")).unwrap(), b"new");
            assert!(root.path().join("old").is_dir());
        } else {
            assert!(root.path().join("target").is_dir());
            assert_eq!(
                std::fs::read(root.path().join("old")).unwrap(),
                b"old bytes"
            );
        }
    }
}

#[test]
fn abort_cleanup_tolerates_missing_contents_but_never_recurses_owned_directory() {
    let root = tempfile::tempdir().unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let namespace = private_directory(&rooted);
    let private = root.path().join(&namespace.staging_dir_name);
    std::fs::write(private.join("contents/child"), b"owned child").unwrap();
    assert!(matches!(
        namespace.abort(),
        Err(RootedFsError::StagingAbortFailed { .. })
    ));
    assert_eq!(
        std::fs::read(private.join("contents/child")).unwrap(),
        b"owned child"
    );
    let staged = rooted
        .begin_staged_file_blocking(&relative("missing"))
        .unwrap();
    let private = root.path().join(&staged.namespace.staging_dir_name);
    std::fs::remove_file(private.join("contents")).unwrap();
    staged.abort().unwrap();
    assert!(!private.exists());
}

#[test]
fn pending_publication_retains_sealed_fields_after_native_effect() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target"), b"old").unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let mut staged = rooted
        .begin_staged_file_blocking(&relative("target"))
        .unwrap();
    staged.file_mut().write_all(b"original").unwrap();
    staged.staged_hash_blocking().unwrap();
    let RootedStagedFile {
        file,
        mut namespace,
    } = staged;
    let mut lineage = rooted.retirement.lock().unwrap();
    namespace
        .publish_blocking(None, None, &mut lineage, || Ok(()))
        .unwrap();
    // A same-inode edit after publication cannot be adopted by from_committed.
    // ctime may advance only around our own namespace effects, never here.
    xattr::set(root.path().join("target"), "user.sy-foreign", b"changed").unwrap();
    namespace.finish_commit_blocking(&mut lineage).unwrap();
    let observation = namespace.sealed.unwrap();
    let error = RootedPublishedFile::from_committed(
        file,
        rooted.clone(),
        relative("target"),
        None,
        observation,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RootedFsError::CommittedFinalizationFailed { .. }
    ));
    assert_eq!(
        xattr::get(root.path().join("target"), "user.sy-foreign").unwrap(),
        Some(b"changed".to_vec())
    );
}
