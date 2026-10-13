use super::*;
use crate::endpoint::ExpectedDestination;

fn rel(path: &str) -> RelativePath {
    RelativePath::new(path).unwrap()
}
async fn qualify(root: RootedFs, parents: &[&str]) -> (RootedFs, DestinationNamespace) {
    let (root, proof) = root.with_namespace_qualification().await.unwrap();
    proof.qualify(NamespaceParent::Root).await.unwrap();
    for parent in parents {
        proof
            .qualify(NamespaceParent::Directory(rel(parent)))
            .await
            .unwrap();
    }
    proof.seal().await.unwrap();
    (root, proof)
}

#[tokio::test]
async fn concurrent_owned_directory_preparation_reuses_only_confirmed_progress() {
    let fixture = tempfile::tempdir().unwrap();
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, _proof) = qualify(root, &["shared/nested"]).await;
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            root.create_directories_blocking(&rel("shared/nested"))
        }));
    }
    for worker in workers {
        worker.await.unwrap().unwrap();
    }
    assert!(fixture.path().join("shared/nested").is_dir());
    // Existing namespace tests separately require that a foreign concurrent
    // winner is retained and never adopted as this run's own continuation.
}

#[tokio::test]
async fn original_parent_replacement_with_same_leaf_refuses_native_publication() {
    for late in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("dir")).unwrap();
        std::fs::create_dir(fixture.path().join("foreign")).unwrap();
        std::fs::write(fixture.path().join("dir/file"), b"old").unwrap();
        // Establish both aliases BEFORE observation, so the substitution does
        // not itself change the leaf token and mask the missing parent check.
        std::fs::hard_link(
            fixture.path().join("dir/file"),
            fixture.path().join("foreign/file"),
        )
        .unwrap();
        let original = crate::endpoint::local_identity::identity_for_metadata(
            &std::fs::metadata(fixture.path().join("dir/file")).unwrap(),
        );
        let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
        let (root, _proof) = qualify(root, &["dir"]).await;
        let staged = if late {
            let mut stage = root.begin_staged_file_blocking(&rel("dir/file")).unwrap();
            stage.file_mut().write_all(b"new").unwrap();
            stage.prepare_publication_blocking().unwrap();
            Some(stage)
        } else {
            None
        };
        std::fs::rename(fixture.path().join("dir"), fixture.path().join("original")).unwrap();
        std::fs::rename(fixture.path().join("foreign"), fixture.path().join("dir")).unwrap();
        assert_eq!(
            crate::endpoint::local_identity::identity_for_metadata(
                &std::fs::metadata(fixture.path().join("dir/file")).unwrap()
            ),
            original
        );
        if let Some(stage) = staged {
            // A new run may observe the changed parent. The prepared transaction
            // still owns the OLD run and must not adopt this fresh qualification.
            let (next, _next_proof) = qualify(root, &["dir"]).await;
            let fresh = next.begin_staged_file_blocking(&rel("dir/file")).unwrap();
            fresh.abort().unwrap();
            assert!(stage.commit().is_err());
        } else {
            assert!(root.begin_staged_file_blocking(&rel("dir/file")).is_err());
        }
        assert_eq!(
            std::fs::read(fixture.path().join("dir/file")).unwrap(),
            b"old"
        );
        assert_eq!(
            std::fs::read(fixture.path().join("original/file")).unwrap(),
            b"old"
        );
    }
}

#[tokio::test]
async fn attached_unsealed_catalogue_refuses_real_effects() {
    let fixture = tempfile::tempdir().unwrap();
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, _proof) = root.with_namespace_qualification().await.unwrap();
    assert!(matches!(
        root.create_directory_blocking(&rel("new")),
        Err(RootedFsError::NamespaceUnsealed)
    ));
    assert!(root.begin_staged_file_blocking(&rel("file")).is_err());
    assert!(!fixture.path().join("new").exists());
}

#[tokio::test]
async fn required_unstarted_destination_refuses_effects_without_begin() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("old"), b"keep").unwrap();
    let mut root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let expected = root.path_identity_blocking(&rel("old")).unwrap().unwrap().1;
    root.require_namespace_qualification();
    assert!(matches!(
        root.create_directory_blocking(&rel("new")),
        Err(RootedFsError::NamespaceUnsealed)
    ));
    assert!(matches!(
        root.begin_staged_file_blocking(&rel("old")),
        Err(RootedFsError::NamespaceUnsealed)
    ));
    assert!(matches!(
        root.apply_metadata_blocking(&rel("old"), EntryKind::File, expected, Some(0o700), None),
        Err(RootedFsError::NamespaceUnsealed)
    ));
    assert_eq!(std::fs::read(fixture.path().join("old")).unwrap(), b"keep");
    assert!(!fixture.path().join("new").exists());

    let mut pending = operand::BoundDestination::open(
        fixture.path().join("missing/nested"),
        operand::SourceShape::Directory { basename: None },
    )
    .await
    .unwrap()
    .root;
    pending.require_namespace_qualification();
    assert!(matches!(
        pending.acquire().await,
        Err(RootedFsError::NamespaceUnsealed)
    ));
    assert!(!fixture.path().join("missing").exists());
}

#[tokio::test]
async fn unconfigured_acquired_root_qualifies_held_authority_not_foreign_spelling() {
    let fixture = tempfile::tempdir().unwrap();
    let bound = operand::BoundDestination::open(
        fixture.path().join("new/root"),
        operand::SourceShape::Directory { basename: None },
    )
    .await
    .unwrap();
    let root = bound.root.acquire().await.unwrap();
    let original_fd = root.root_fd.as_raw_fd();
    std::fs::rename(fixture.path().join("new/root"), fixture.path().join("held")).unwrap();
    std::fs::create_dir(fixture.path().join("new/root")).unwrap();
    let (root, proof) = qualify(root, &[]).await;
    assert_eq!(root.root_fd.as_raw_fd(), original_fd);
    assert!(Arc::ptr_eq(
        &proof.owner.lock().unwrap().anchor,
        &root.root_fd
    ));
    assert!(matches!(
        root.verify_root_path_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
    let mut stage = root.begin_staged_file_blocking(&rel("file")).unwrap();
    stage.file_mut().write_all(b"original").unwrap();
    stage.prepare_publication_blocking().unwrap();
    stage.commit().unwrap();
    assert_eq!(
        std::fs::read(fixture.path().join("held/file")).unwrap(),
        b"original"
    );
    assert!(!fixture.path().join("new/root/file").exists());
}

#[tokio::test]
async fn predicted_absence_does_not_adopt_preexisting_foreign_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, _proof) = qualify(root, &["new", "new/child"]).await;
    std::fs::create_dir(fixture.path().join("new")).unwrap();
    std::fs::write(fixture.path().join("new/foreign"), b"keep").unwrap();
    assert!(root.create_directories_blocking(&rel("new/child")).is_err());
    assert_eq!(
        std::fs::read(fixture.path().join("new/foreign")).unwrap(),
        b"keep"
    );
    assert!(!fixture.path().join("new/child").exists());
}

#[tokio::test]
async fn concurrent_mkdir_winner_is_retained_not_adopted() {
    let fixture = tempfile::tempdir().unwrap();
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, _proof) = qualify(root, &["new", "new/child"]).await;
    let (reached, wait) = tokio::sync::oneshot::channel();
    let (resume, pause) = std::sync::mpsc::channel();
    root.pause_mutation_at(
        0,
        PublicationPause {
            point: PublicationPausePoint::BeforeAdmission,
            reached,
            resume: pause,
        },
    );
    let worker = root.clone();
    let task =
        tokio::task::spawn_blocking(move || worker.create_directories_blocking(&rel("new/child")));
    wait.await.unwrap();
    std::fs::create_dir(fixture.path().join("new")).unwrap();
    std::fs::write(fixture.path().join("new/foreign"), b"keep").unwrap();
    resume.send(()).unwrap();
    assert!(task.await.unwrap().is_err());
    assert_eq!(
        std::fs::read(fixture.path().join("new/foreign")).unwrap(),
        b"keep"
    );
    assert!(!fixture.path().join("new/child").exists());
}

#[tokio::test]
async fn own_new_directories_and_pending_nested_root_share_original_owner() {
    let fixture = tempfile::tempdir().unwrap();
    let bound = operand::BoundDestination::open(
        fixture.path().join("new/root"),
        operand::SourceShape::Directory { basename: None },
    )
    .await
    .unwrap();
    let (bound, proof) = bound.root.with_namespace_qualification().await.unwrap();
    proof
        .qualify(NamespaceParent::Directory(rel("child")))
        .await
        .unwrap();
    proof.seal().await.unwrap();
    let root = bound.acquire().await.unwrap();
    assert!(Arc::ptr_eq(
        &proof.owner,
        root.destination_namespace.as_ref().unwrap()
    ));
    root.create_directories_blocking(&rel("child")).unwrap();
    let mut file = root
        .begin_staged_file_with_expectation_blocking(
            &rel("child/file"),
            ExpectedDestination::Absent,
        )
        .unwrap();
    file.file_mut().write_all(b"owned").unwrap();
    file.prepare_publication_blocking().unwrap();
    file.commit().unwrap();
    assert_eq!(
        std::fs::read(fixture.path().join("new/root/child/file")).unwrap(),
        b"owned"
    );
    let original_fd = root.root_fd.as_raw_fd();
    let original = root.clone();
    let (root, next_proof) = qualify(root, &["child"]).await;
    assert_eq!(root.root_fd.as_raw_fd(), original_fd);
    assert!(Arc::ptr_eq(
        &next_proof.owner.lock().unwrap().anchor,
        &root.root_fd
    ));
    assert_eq!(root.namespace_prefix, Path::new(""));
    assert_eq!(original.namespace_prefix, Path::new("new/root"));
    assert!(!Arc::ptr_eq(&proof.owner, &next_proof.owner));
    root.begin_staged_file_blocking(&rel("child/file"))
        .unwrap()
        .abort()
        .unwrap();
    std::fs::rename(fixture.path().join("new/root"), fixture.path().join("held")).unwrap();
    std::fs::create_dir(fixture.path().join("new/root")).unwrap();
    let (root, relocated_proof) = qualify(root, &["child"]).await;
    assert_eq!(root.root_fd.as_raw_fd(), original_fd);
    assert!(Arc::ptr_eq(
        &relocated_proof.owner.lock().unwrap().anchor,
        &root.root_fd
    ));
    assert!(matches!(
        root.verify_root_path_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
    for bound in [&original, &root] {
        bound
            .begin_staged_file_blocking(&rel("child/file"))
            .unwrap()
            .abort()
            .unwrap();
    }
    assert!(!fixture.path().join("new/root/child").exists());
}

#[tokio::test]
async fn pending_acquired_root_relocation_preserves_native_parent_and_namespace_proof() {
    let fixture = tempfile::tempdir().unwrap();
    let selected = fixture.path().join("new/root");
    let bound = operand::BoundDestination::open(
        selected.clone(),
        operand::SourceShape::Directory { basename: None },
    )
    .await
    .unwrap();
    let (pending, proof) = bound.root.with_namespace_qualification().await.unwrap();
    proof
        .qualify(NamespaceParent::Directory(rel("dir/new")))
        .await
        .unwrap();
    proof.seal().await.unwrap();
    let root = pending.acquire().await.unwrap();
    root.create_directory_blocking(&rel("dir")).unwrap();
    let path = rel("dir/file");
    let (parent, _) = root.open_parent_blocking(path.as_path()).unwrap();
    let before = stat_fd(parent.as_raw_fd()).unwrap();
    let original_record = proof
        .owner
        .lock()
        .unwrap()
        .get(key(Path::new("new/root/dir")))
        .unwrap()
        .unwrap();
    assert_eq!(
        original_record.observed,
        Some((before.st_dev as u64, before.st_ino as u64))
    );
    let mut stage = root
        .begin_staged_file_with_expectation_blocking(&path, ExpectedDestination::Absent)
        .unwrap();
    stage.file_mut().write_all(b"held-root").unwrap();
    stage.prepare_publication_blocking().unwrap();

    let moved = fixture.path().join("held");
    let foreign = fixture.path().join("foreign");
    std::fs::rename(&selected, &moved).unwrap();
    std::fs::create_dir(&foreign).unwrap();
    std::os::unix::fs::symlink(&foreign, &selected).unwrap();
    assert!(matches!(
        root.verify_root_path_blocking(),
        Err(RootedFsError::RootChanged(_))
    ));
    root.verify_parent_binding_blocking(path.as_path(), &parent)
        .unwrap();
    root.verify_namespace_directory(Path::new("dir"), parent.as_raw_fd())
        .unwrap();
    let (reopened, _) = root.open_parent_blocking(path.as_path()).unwrap();
    let after = stat_fd(reopened.as_raw_fd()).unwrap();
    assert_eq!((before.st_dev, before.st_ino), (after.st_dev, after.st_ino));
    stage.commit().unwrap();
    root.create_directory_blocking(&rel("dir/new")).unwrap();
    assert!(Arc::ptr_eq(
        &proof.owner,
        root.destination_namespace.as_ref().unwrap()
    ));
    assert_eq!(std::fs::read(moved.join("dir/file")).unwrap(), b"held-root");
    assert!(moved.join("dir/new").is_dir());
    assert_eq!(std::fs::read_dir(&foreign).unwrap().count(), 0);

    // Held-root authority does not bypass original directory bindings below it,
    // even when the leaf under the foreign parent is the exact same inode.
    std::fs::create_dir(moved.join("foreign-dir")).unwrap();
    std::fs::hard_link(moved.join("dir/file"), moved.join("foreign-dir/file")).unwrap();
    std::fs::rename(moved.join("dir"), moved.join("original-dir")).unwrap();
    std::fs::rename(moved.join("foreign-dir"), moved.join("dir")).unwrap();
    use std::os::unix::fs::MetadataExt;
    let original_leaf = std::fs::metadata(moved.join("original-dir/file")).unwrap();
    let foreign_leaf = std::fs::metadata(moved.join("dir/file")).unwrap();
    assert_eq!(
        (original_leaf.dev(), original_leaf.ino()),
        (foreign_leaf.dev(), foreign_leaf.ino())
    );
    let old = root.clone();
    let (next, _next_proof) = qualify(root, &["dir"]).await;
    next.begin_staged_file_blocking(&path)
        .unwrap()
        .abort()
        .unwrap();
    assert!(matches!(
        old.begin_staged_file_blocking(&path),
        Err(RootedFsError::DestinationChanged(_))
    ));
    assert_eq!(std::fs::read(moved.join("dir/file")).unwrap(), b"held-root");
}

#[tokio::test]
async fn scratch_corruption_poison_is_terminal_before_native_effects() {
    use std::os::unix::fs::FileExt;
    for index in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
        let (root, proof) = qualify(root, &["new"]).await;
        {
            let c = proof.owner.lock().unwrap();
            if index {
                c.index.file.write_at(&[255], 0).unwrap();
            } else {
                c.tape.write_at(&[255], 32).unwrap();
            }
        }
        assert!(root.create_directories_blocking(&rel("new")).is_err());
        assert!(root.begin_staged_file_blocking(&rel("file")).is_err());
        assert!(!fixture.path().join("new").exists());
    }
}

#[tokio::test]
async fn many_parents_use_constant_catalogue_descriptors() {
    const CHILD: &str = "SY_NAMESPACE_LOW_FD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rooted_fs::namespace::tests::many_parents_use_constant_catalogue_descriptors",
                "--test-threads=1",
                "--nocapture",
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
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: initialized by getrlimit; changed only in this isolated test child.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
        0
    );
    // SAFETY: successful getrlimit initialized the structure.
    let mut limit = unsafe { limit.assume_init() };
    limit.rlim_cur = limit.rlim_cur.min(48);
    // SAFETY: live native structure; hard limit is unchanged.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let fixture = tempfile::tempdir().unwrap();
    for n in 0..256 {
        std::fs::create_dir(fixture.path().join(format!("dir{n}"))).unwrap();
    }
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, proof) = root.with_namespace_qualification().await.unwrap();
    for n in 0..256 {
        proof
            .qualify(NamespaceParent::Directory(rel(&format!("dir{n}"))))
            .await
            .unwrap();
    }
    proof.seal().await.unwrap();
    for n in 0..256 {
        let path = rel(&format!("dir{n}/file"));
        let stage = root
            .begin_staged_file_with_expectation_blocking(&path, ExpectedDestination::Absent)
            .unwrap();
        stage.abort().unwrap();
    }
    // One shared owner contains only the held anchor and two scratch files;
    // active walks close each predecessor rather than retaining per-parent FDs.
    assert_eq!(proof.owner.lock().unwrap().count, 257);
}

#[tokio::test]
async fn directory_target_finalization_requires_own_qualification() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("dir")).unwrap();
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, _proof) = qualify(root, &[]).await;
    let expected = root.path_identity_blocking(&rel("dir")).unwrap().unwrap().1;
    assert!(root
        .finalize_directory_blocking(
            &rel("dir"),
            expected,
            Some(0o700),
            None,
            &DirectoryPreservation::default()
        )
        .is_err());
}

#[tokio::test]
async fn new_run_succeeds_with_retained_proofs_and_roots_without_rebinding_them() {
    let fixture = tempfile::tempdir().unwrap();
    let mut initial = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    initial.require_namespace_qualification();
    let cached = initial.clone();
    let original_fd = initial.root_fd.as_raw_fd();
    let (root, proof) = qualify(initial, &[]).await;
    let retained = root.clone();
    let (next, next_proof) = qualify(root, &["new"]).await;
    assert_eq!(next.root_fd.as_raw_fd(), original_fd);
    assert!(!Arc::ptr_eq(&proof.owner, &next_proof.owner));
    assert!(Arc::ptr_eq(
        &proof.owner,
        retained.destination_namespace.as_ref().unwrap()
    ));
    assert!(matches!(
        cached.create_directory_blocking(&rel("new")),
        Err(RootedFsError::NamespaceUnsealed)
    ));
    assert!(matches!(
        retained.create_directory_blocking(&rel("new")),
        Err(RootedFsError::NamespaceUnqualified(_))
    ));
    next.create_directory_blocking(&rel("new")).unwrap();
}

#[tokio::test]
async fn legitimate_directory_metadata_does_not_invalidate_namespace_proof() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("dir")).unwrap();
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, _proof) = qualify(root, &["dir"]).await;
    std::fs::set_permissions(
        fixture.path().join("dir"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let stage = root
        .begin_staged_file_with_expectation_blocking(&rel("dir/file"), ExpectedDestination::Absent)
        .unwrap();
    stage.abort().unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_casefold_profile_change_refuses_admission() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("dir")).unwrap();
    let root = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (root, _proof) = qualify(root, &["dir"]).await;
    let directory = open_dir_at(root.root_fd.as_raw_fd(), OsStr::new("dir")).unwrap();
    let mut flags: libc::c_uint = 0;
    // SAFETY: live directory and unsigned-int ioctl output buffer.
    let queried = unsafe { libc::ioctl(directory.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) };
    if queried != 0 {
        eprintln!(
            "native casefold mutation unavailable: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    flags ^= 0x4000_0000;
    // SAFETY: live directory and unsigned-int input buffer, modifying this test's empty directory only.
    if unsafe { libc::ioctl(directory.as_raw_fd(), libc::FS_IOC_SETFLAGS, &flags) } != 0 {
        eprintln!(
            "native casefold mutation unavailable: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let old = root.clone();
    let (next, _next_proof) = qualify(root, &["dir"]).await;
    next.begin_staged_file_with_expectation_blocking(&rel("dir/file"), ExpectedDestination::Absent)
        .unwrap()
        .abort()
        .unwrap();
    assert!(old
        .begin_staged_file_with_expectation_blocking(&rel("dir/file"), ExpectedDestination::Absent)
        .is_err());
    assert!(!fixture.path().join("dir/file").exists());
}
