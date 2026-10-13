use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn rel(path: &str) -> RelativePath {
    RelativePath::new(path).unwrap()
}

fn observes_folded_names(root: &Path) -> bool {
    std::fs::write(root.join("fold-probe"), b"probe").unwrap();
    let folded = root.join("FOLD-PROBE").exists();
    std::fs::remove_file(root.join("fold-probe")).unwrap();
    if !folded {
        eprintln!("observed prerequisite unavailable: native case aliases do not resolve");
    }
    folded
}

#[tokio::test]
async fn native_backup_spelling_uses_original_parents_and_literal_bytes() {
    for (existing, requested) in [
        ("FOO~", "foo~"),
        ("BACKUPS", "backups/file~"),
        ("café~", "cafe\u{301}~"),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        if existing == "BACKUPS" {
            std::fs::create_dir(fixture.path().join(existing)).unwrap();
        } else {
            std::fs::write(fixture.path().join(existing), b"kept").unwrap();
        }
        let lookup = requested.split('/').next().unwrap();
        if !fixture.path().join(lookup).exists() {
            eprintln!(
                "observed prerequisite unavailable: {lookup:?} does not resolve {existing:?}"
            );
            continue;
        }
        let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
        let (_, proof) = rooted.with_namespace_qualification().await.unwrap();
        proof.qualify(NamespaceParent::Root).await.unwrap();
        if existing == "BACKUPS" {
            proof
                .qualify(NamespaceParent::Directory(rel("backups")))
                .await
                .unwrap();
        }
        let error = proof
            .validate_backup_address(rel(requested))
            .await
            .unwrap_err();
        let NamespacePreflightError::Io(error) = error else {
            panic!("{error}")
        };
        assert!(matches!(
            error.get_ref().unwrap().downcast_ref::<RootedFsError>(),
            Some(RootedFsError::BackupAddressAlias(_))
        ));
    }
}

#[tokio::test]
async fn late_backup_copy_refuses_same_inode_case_renames_in_both_roots() {
    for external in [false, true] {
        for ancestor in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            if !observes_folded_names(fixture.path()) {
                return;
            }
            let main_path = fixture.path().join("main");
            let backup_path = fixture.path().join("backup");
            std::fs::create_dir(&main_path).unwrap();
            std::fs::create_dir(&backup_path).unwrap();
            std::fs::write(main_path.join("source"), b"old destination").unwrap();
            let publication_root = if external { &backup_path } else { &main_path };
            std::fs::create_dir(publication_root.join("nested")).unwrap();
            let slot = publication_root.join("nested/foo~");
            std::fs::write(&slot, b"kept backup").unwrap();
            let before = std::fs::metadata(&slot).unwrap();
            let main = RootedFs::open(main_path.clone()).await.unwrap();
            let (main, proof) = main.with_namespace_qualification().await.unwrap();
            proof.qualify(NamespaceParent::Root).await.unwrap();
            let external_root = if external {
                let root = super::super::backup::BackupRoot::capture(backup_path.clone()).await;
                let (backup_proof, mut entries) = root.prepare().await.unwrap();
                entries.close().await.unwrap();
                backup_proof
                    .qualify(NamespaceParent::Directory(rel("nested")))
                    .await
                    .unwrap();
                backup_proof
                    .validate_backup_address(rel("nested/foo~"))
                    .await
                    .unwrap();
                backup_proof.seal().await.unwrap();
                Some(root)
            } else {
                proof
                    .qualify(NamespaceParent::Directory(rel("nested")))
                    .await
                    .unwrap();
                proof
                    .validate_backup_address(rel("nested/foo~"))
                    .await
                    .unwrap();
                None
            };
            proof.seal().await.unwrap();
            let expected = main
                .path_identity_blocking(&rel("source"))
                .unwrap()
                .unwrap()
                .1;
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            main.pause_mutation_at(
                0,
                PublicationPause {
                    point: PublicationPausePoint::BeforeAdmission,
                    reached: reached_tx,
                    resume: resume_rx,
                },
            );
            let target = slot.clone();
            let worker = tokio::task::spawn_blocking(move || {
                if let Some(backup) = &external_root {
                    main.backup_file_with_root_blocking(
                        &rel("source"),
                        &target,
                        expected,
                        Some(backup),
                    )
                } else {
                    main.copy_file_blocking(&rel("source"), &rel("nested/foo~"), expected)
                }
            });
            let reached = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
            let kept = if ancestor {
                publication_root.join("NESTED/foo~")
            } else {
                publication_root.join("nested/FOO~")
            };
            let rename = if ancestor {
                std::fs::rename(
                    publication_root.join("nested"),
                    publication_root.join("NESTED"),
                )
            } else {
                std::fs::rename(&slot, &kept)
            };
            let _ = resume_tx.send(());
            let result = worker.await.unwrap();
            reached.unwrap().unwrap();
            rename.unwrap();
            assert!(
                matches!(result, Err(RootedFsError::BackupAddressAlias(_))),
                "{result:?}"
            );
            let after = std::fs::metadata(&kept).unwrap();
            assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
            assert_eq!(std::fs::read(&kept).unwrap(), b"kept backup");
            assert_eq!(
                std::fs::read(main_path.join("source")).unwrap(),
                b"old destination"
            );
            assert_eq!(
                std::fs::read_dir(kept.parent().unwrap()).unwrap().count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn pending_backup_root_case_rename_refuses_next_mkdir() {
    let fixture = tempfile::tempdir().unwrap();
    if !observes_folded_names(fixture.path()) {
        return;
    }
    let main_path = fixture.path().join("main");
    std::fs::create_dir(&main_path).unwrap();
    std::fs::write(main_path.join("source"), b"old").unwrap();
    let main = RootedFs::open(main_path.clone()).await.unwrap();
    let expected = main
        .path_identity_blocking(&rel("source"))
        .unwrap()
        .unwrap()
        .1;
    let path = fixture.path().join("newroot/deep");
    let backup = super::super::backup::BackupRoot::capture(path.clone()).await;
    let (proof, mut entries) = backup.prepare().await.unwrap();
    entries.close().await.unwrap();
    proof.qualify(NamespaceParent::Root).await.unwrap();
    proof.validate_backup_address(rel("foo~")).await.unwrap();
    proof.seal().await.unwrap();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    main.pause_mutation_at(
        1,
        PublicationPause {
            point: PublicationPausePoint::BeforeAdmission,
            reached: reached_tx,
            resume: resume_rx,
        },
    );
    let worker = tokio::task::spawn_blocking(move || {
        main.backup_file_with_root_blocking(
            &rel("source"),
            &path.join("foo~"),
            expected,
            Some(&backup),
        )
    });
    let reached = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
    let rename = std::fs::rename(
        fixture.path().join("newroot"),
        fixture.path().join("NEWROOT"),
    );
    let _ = resume_tx.send(());
    let result = worker.await.unwrap();
    reached.unwrap().unwrap();
    rename.unwrap();
    assert!(
        matches!(result, Err(RootedFsError::BackupAddressAlias(_))),
        "{result:?}"
    );
    assert!(!fixture.path().join("NEWROOT/deep").exists());
    assert_eq!(std::fs::read(main_path.join("source")).unwrap(), b"old");
}

#[tokio::test]
async fn backup_literal_proof_propagates_mandatory_directory_read_failure() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("literal~"), b"kept").unwrap();
    let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (_, proof) = rooted.with_namespace_qualification().await.unwrap();
    proof.qualify(NamespaceParent::Root).await.unwrap();
    std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o111)).unwrap();
    let result = proof.validate_backup_address(rel("literal~")).await;
    std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    // Elevated runners can bypass DAC; report that observed prerequisite.
    // SAFETY: geteuid has no arguments or caller obligations.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("observed prerequisite unavailable: elevated runner bypasses directory DAC");
        return;
    }
    assert!(
        matches!(result, Err(NamespacePreflightError::Io(_))),
        "{result:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn backup_literal_proof_preserves_non_utf8_names() {
    let fixture = tempfile::tempdir().unwrap();
    let name = OsStr::from_bytes(b"file-\xff~");
    std::fs::write(fixture.path().join(name), b"kept").unwrap();
    let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
    let (_, proof) = rooted.with_namespace_qualification().await.unwrap();
    proof.qualify(NamespaceParent::Root).await.unwrap();
    proof
        .validate_backup_address(RelativePath::new(name).unwrap())
        .await
        .unwrap();
}
