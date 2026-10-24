use super::*;

#[cfg(target_os = "macos")]
#[tokio::test]
async fn empty_directory_effects_exclude_original_source_not_just_descendants() {
    use crate::endpoint::source_root::SourceRoot;
    use futures::TryStreamExt;
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("source")).unwrap();
    std::fs::create_dir(fixture.path().join("old")).unwrap();
    let source = SourceRoot::open(fixture.path().join("source"))
        .await
        .unwrap();
    source
        .entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let effects = LocalSourceEffects::seal(&source).await.unwrap();
    let destination = operand::DestinationRoot::Present(
        RootedFs::open(fixture.path().to_path_buf()).await.unwrap(),
    );
    for kind in [
        SourceEffectKind::LeafPublication,
        SourceEffectKind::NamespaceRetirement,
    ] {
        effects
            .validate_effect(&destination, RelativePath::new("old").unwrap(), kind)
            .await
            .unwrap();
        assert!(matches!(
            effects
                .validate_effect(&destination, RelativePath::new("source").unwrap(), kind)
                .await,
            Err(RootedFsError::SourceEffectUnqualified(path)) if path == Path::new("source")
        ));
    }
    assert!(fixture.path().join("source").is_dir());
    assert!(fixture.path().join("old").is_dir());
}

#[cfg(target_os = "macos")]
#[test]
fn empty_directory_proof_rejects_nonempty_changed_and_rebound_old() {
    for action in ["nonempty", "arrival", "empty-again", "replacement"] {
        let fixture = tempfile::tempdir().unwrap();
        let parent = File::open(fixture.path()).unwrap();
        let old = fixture.path().join("old");
        std::fs::create_dir(&old).unwrap();
        if action == "nonempty" {
            std::fs::write(old.join("child"), b"OLD").unwrap();
        }
        let effect = SourceEffect {
            kind: SourceEffectKind::NamespaceRetirement,
            parent: parent.as_raw_fd(),
            name: OsStr::new("old"),
            object: None,
            path: Path::new("old"),
        };
        let before = stat_at_optional(effect.parent, effect.name)
            .unwrap()
            .unwrap();
        let proof = EmptyDirectory::capture(&effect, before);
        if action == "nonempty" {
            assert!(matches!(
                proof,
                Err(RootedFsError::SourceEffectUnqualified(_))
            ));
            assert_eq!(std::fs::read(old.join("child")).unwrap(), b"OLD");
            continue;
        }
        let proof = proof.unwrap();
        match action {
            "arrival" | "empty-again" => {
                std::fs::write(old.join("child"), b"ARRIVAL").unwrap();
                if action == "empty-again" {
                    std::fs::remove_file(old.join("child")).unwrap();
                }
            }
            "replacement" => {
                std::fs::rename(&old, fixture.path().join("moved")).unwrap();
                std::fs::create_dir(&old).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            proof.verify(&effect),
            Err(RootedFsError::DestinationChanged(path)) if path == Path::new("old")
        ));
    }
}
