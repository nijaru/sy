use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::namespace::NamespaceQualification;
use futures::TryStreamExt;

async fn source(path: &Path) -> (SourceRoot, LocalSourceEffects) {
    let source = SourceRoot::open(path.to_path_buf()).await.unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    (source, effects)
}

async fn destination(path: &Path) -> operand::DestinationRoot {
    let root = operand::BoundDestination::open(
        path.to_path_buf(),
        operand::SourceShape::Directory { basename: None },
    )
    .await
    .unwrap()
    .root;
    let (root, namespace) = root.with_namespace_qualification().await.unwrap();
    namespace.seal().await.unwrap();
    root
}

#[tokio::test]
async fn late_backup_overlap_cannot_admit_an_earlier_pending_root() {
    let fixture = tempfile::tempdir().unwrap();
    let src = fixture.path().join("source");
    let back = fixture.path().join("backup");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&back).unwrap();
    std::fs::write(src.join("original"), b"SOURCE").unwrap();
    std::fs::hard_link(src.join("original"), back.join("old")).unwrap();
    let (_, effects) = source(&src).await;
    let root = destination(&fixture.path().join("missing/root"))
        .await
        .with_local_source_effects(effects.clone());
    let backup = backup::BackupRoot::capture(back.clone()).await;
    let (namespace, entries) = backup.prepare().await.unwrap();
    entries.try_collect::<Vec<_>>().await.unwrap();
    namespace.seal().await.unwrap();
    let batch = EffectPreflight::new(&effects, &root, Some(&backup))
        .await
        .unwrap();
    batch
        .declare(
            EffectRoot::Main,
            RelativePath::new("new").unwrap(),
            SourceEffectKind::LeafPublication,
        )
        .await
        .unwrap();
    batch
        .declare(
            EffectRoot::Backup,
            RelativePath::new("old").unwrap(),
            SourceEffectKind::LeafPublication,
        )
        .await
        .unwrap();
    assert!(matches!(batch.finish().await,
        Err(RootedFsError::SourceEffectUnqualified(path)) if path == Path::new("old")));
    assert!(matches!(
        root.acquire_local().await,
        Err(RootedFsError::SourceEffectUnqualified(_))
    ));
    assert!(!fixture.path().join("missing").exists());
    assert_eq!(std::fs::read(src.join("original")).unwrap(), b"SOURCE");
    assert_eq!(std::fs::read(back.join("old")).unwrap(), b"SOURCE");
}

#[tokio::test]
async fn shared_census_checks_late_directory_and_operator_link_footprints() {
    for operator_link in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let ancestor = fixture.path().join("ancestor");
        let src = ancestor.join("source");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("original"), b"SOURCE").unwrap();
        let alias = fixture.path().join("operator");
        std::os::unix::fs::symlink(&src, &alias).unwrap();
        let (_, effects) = source(if operator_link { &alias } else { &src }).await;
        let root = destination(fixture.path()).await;
        let batch = EffectPreflight::new(&effects, &root, None).await.unwrap();
        batch
            .declare(
                EffectRoot::Main,
                RelativePath::new("safe").unwrap(),
                SourceEffectKind::LeafPublication,
            )
            .await
            .unwrap();
        let (path, kind) = if operator_link {
            ("operator", SourceEffectKind::SymlinkMetadata)
        } else {
            ("ancestor", SourceEffectKind::DirectoryPolicy)
        };
        batch
            .declare(EffectRoot::Main, RelativePath::new(path).unwrap(), kind)
            .await
            .unwrap();
        assert!(matches!(batch.finish().await,
            Err(RootedFsError::SourceEffectUnqualified(changed)) if changed == Path::new(path)));
        assert!(!fixture.path().join("safe").exists());
        assert_eq!(std::fs::read(src.join("original")).unwrap(), b"SOURCE");
        effects.validate_originals_blocking().unwrap();
    }
}

#[tokio::test]
async fn destination_replay_preserves_source_priority_without_poisoning_healthy_source() {
    for change_source in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let src = fixture.path().join("source");
        let dst = fixture.path().join("destination");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.join("original"), b"SOURCE").unwrap();
        std::fs::write(dst.join("old"), b"OLD").unwrap();
        std::fs::write(dst.join("foreign"), b"foreign").unwrap();
        let (source, effects) = source(&src).await;
        let root = destination(&dst).await;
        let batch = EffectPreflight::new(&effects, &root, None).await.unwrap();
        batch
            .declare(
                EffectRoot::Main,
                RelativePath::new("old").unwrap(),
                SourceEffectKind::LeafPublication,
            )
            .await
            .unwrap();
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        source.rooted().pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceCensus,
                reached,
                resume: resumed,
            },
        );
        let worker = tokio::spawn(batch.finish());
        wait.await.unwrap();
        std::fs::rename(dst.join("foreign"), dst.join("old")).unwrap();
        if change_source {
            std::fs::write(src.join("original"), b"changed SOURCE").unwrap();
        }
        resume.send(()).unwrap();
        let error = worker.await.unwrap().unwrap_err();
        if change_source {
            assert!(
                matches!(error, RootedFsError::SourceBindingChanged(_)),
                "{error}"
            );
        } else {
            assert!(
                matches!(error, RootedFsError::DestinationChanged(_)),
                "{error}"
            );
            effects.validate_originals_blocking().unwrap();
            let retry = EffectPreflight::new(&effects, &root, None).await.unwrap();
            retry
                .declare(
                    EffectRoot::Main,
                    RelativePath::new("old").unwrap(),
                    SourceEffectKind::LeafPublication,
                )
                .await
                .unwrap();
            retry.finish().await.unwrap();
        }
        assert_eq!(std::fs::read(dst.join("old")).unwrap(), b"foreign");
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn cancellation_after_collection_is_terminal_and_admits_no_root() {
    let fixture = tempfile::tempdir().unwrap();
    let src = fixture.path().join("source");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("original"), b"SOURCE").unwrap();
    let (source, effects) = source(&src).await;
    let root = destination(&fixture.path().join("missing/root"))
        .await
        .with_local_source_effects(effects.clone());
    let batch = EffectPreflight::new(&effects, &root, None).await.unwrap();
    batch
        .declare(
            EffectRoot::Main,
            RelativePath::new("new").unwrap(),
            SourceEffectKind::LeafPublication,
        )
        .await
        .unwrap();
    let cancelled = source.rooted().source_run_cancellation(false).unwrap();
    cancelled.store(true, Ordering::Release);
    assert!(matches!(
        batch.finish().await,
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
    cancelled.store(false, Ordering::Release);
    assert!(matches!(
        effects.validate_originals_blocking(),
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
    assert!(root.acquire_local().await.is_err());
    assert!(!fixture.path().join("missing").exists());
}

#[tokio::test]
async fn wide_preflight_has_bounded_descriptors() {
    const CHILD: &str = "SY_TEST_EFFECT_PREFLIGHT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let fixture = tempfile::tempdir().unwrap();
        let src = fixture.path().join("source");
        let dst = fixture.path().join("destination");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(src.join("original"), b"SOURCE").unwrap();
        let (_, effects) = source(&src).await;
        let root = destination(&dst).await;
        let batch = EffectPreflight::new(&effects, &root, None).await.unwrap();
        for index in 0..1200 {
            batch
                .declare(
                    EffectRoot::Main,
                    RelativePath::new(format!("new{index:04}")).unwrap(),
                    SourceEffectKind::LeafPublication,
                )
                .await
                .unwrap();
        }
        batch.finish().await.unwrap();
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 0);
        assert_eq!(std::fs::read(src.join("original")).unwrap(), b"SOURCE");
        return;
    }
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "rooted_fs::local_source_effects::preflight::tests::wide_preflight_has_bounded_descriptors"])
        .env(CHILD, "1");
    // SAFETY: only async-signal-safe syscalls and stack data after fork.
    unsafe {
        child.pre_exec(|| {
            let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut limit = limit.assume_init();
            limit.rlim_cur = limit.rlim_cur.min(32);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
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
