use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::namespace::NamespaceQualification;
use crate::rooted_fs::local_source_effects::LocalSourceEffects;
use futures::TryStreamExt;

#[derive(Clone, Copy, Debug)]
enum EndChange {
    OperatorAncestor,
    MemberDirectory,
    TapeExtent,
    Cancel,
}

#[tokio::test]
async fn census_end_barriers_precede_private_creation_and_remain_terminal() {
    for change in [
        EndChange::OperatorAncestor,
        EndChange::MemberDirectory,
        EndChange::TapeExtent,
        EndChange::Cancel,
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let ancestor = fixture.path().join("ancestor");
        let source_path = ancestor.join("source");
        std::fs::create_dir_all(source_path.join("member")).unwrap();
        std::fs::write(source_path.join("member/file"), b"SOURCE").unwrap();
        let source = SourceRoot::open(source_path.clone()).await.unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let effects = LocalSourceEffects::seal(&source).await.unwrap();
        let source = source.rooted();
        let (tape, cancelled) = {
            let mut owner = source.source_observations.lock().unwrap();
            let c = owner.active().unwrap();
            (c.file.try_clone().unwrap(), Arc::clone(&c.cancelled))
        };
        let destination = fixture.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        let rooted = RootedFs::open(destination.clone()).await.unwrap();
        let (mut rooted, namespace) = rooted.with_namespace_qualification().await.unwrap();
        namespace.seal().await.unwrap();
        rooted.local_source_effects = Some(effects.clone());
        let before = stat_fd(rooted.root_fd.as_raw_fd()).unwrap();
        let destination_fd = rooted.root_fd.try_clone().unwrap();

        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, receive) = std::sync::mpsc::channel();
        source.pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceCensus,
                reached,
                resume: receive,
            },
        );
        let worker = tokio::task::spawn_blocking(move || {
            rooted.begin_staged_file_blocking(&RelativePath::new("publication").unwrap())
        });
        wait.await.unwrap();
        let remaining = match change {
            EndChange::OperatorAncestor => {
                let moved = fixture.path().join("moved");
                std::fs::rename(&ancestor, &moved).unwrap();
                moved.join("source/member/file")
            }
            EndChange::MemberDirectory => {
                let moved = fixture.path().join("moved");
                std::fs::rename(source_path.join("member"), &moved).unwrap();
                moved.join("file")
            }
            EndChange::TapeExtent => {
                tape.set_len(tape.metadata().unwrap().len() + 1).unwrap();
                source_path.join("member/file")
            }
            EndChange::Cancel => {
                cancelled.store(true, Ordering::Release);
                source_path.join("member/file")
            }
        };
        resume.send(()).unwrap();
        let error = match worker.await.unwrap() {
            Ok(_) => panic!("{change:?} admitted private creation"),
            Err(error) => error,
        };
        match change {
            EndChange::OperatorAncestor => assert!(
                matches!(&error, RootedFsError::Io(error) if matches!(
                    error.get_ref().and_then(|error| error.downcast_ref::<RootedFsError>()),
                    Some(RootedFsError::RootChanged(_))
                )),
                "{error}"
            ),
            EndChange::MemberDirectory => assert!(
                matches!(&error, RootedFsError::SourceBindingChanged(path) if path == Path::new("member")),
                "{error}"
            ),
            EndChange::TapeExtent => assert!(
                matches!(&error, RootedFsError::Io(error) if error.kind() == io::ErrorKind::InvalidData),
                "{error}"
            ),
            EndChange::Cancel => assert!(
                matches!(&error, RootedFsError::SourceCatalogueTerminal),
                "{error}"
            ),
        }
        // Removing the temporary fault cannot revive a partially checked batch.
        if matches!(change, EndChange::Cancel) {
            cancelled.store(false, Ordering::Release);
        }
        assert!(effects.validate_originals_blocking().is_err(), "{change:?}");
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        assert!(
            staging::same_observation(&before, &stat_fd(destination_fd.as_raw_fd()).unwrap()),
            "{change:?} changed destination metadata before admission"
        );
        assert_eq!(std::fs::read(remaining).unwrap(), b"SOURCE");
    }
}

#[tokio::test]
async fn census_authenticates_root_inverse_and_non_directory_disposition() {
    for root_inverse in [true, false] {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        std::fs::create_dir(&source_path).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(source_path.join("file"), b"SOURCE").unwrap();
        let source = SourceRoot::open(source_path).await.unwrap();
        source
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let inventory = source.rooted().seal_source_inventory_blocking().unwrap();
        let rooted = RootedFs::open(destination).await.unwrap();
        let candidate = object_key(rooted.root_fd.as_raw_fd(), None)
            .unwrap()
            .unwrap();
        {
            let mut owner = inventory.rooted.source_observations.lock().unwrap();
            let c = owner.active().unwrap();
            let key = if root_inverse {
                state_key(b"ordinal", [0; 32])
            } else {
                let (key, _) = c.original_at(1).unwrap();
                state_key(b"directory", key)
            };
            // Authenticated but inconsistent state is not membership evidence.
            c.index.set_bytes(key, [42; 48]).unwrap();
        }
        assert!(matches!(
            inventory.excludes_objects_blocking(&[candidate], SourceExclusion::Objects),
            Err(RootedFsError::Io(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(matches!(
            inventory.excludes_objects_blocking(&[candidate], SourceExclusion::Objects),
            Err(RootedFsError::SourceCatalogueTerminal)
        ));
    }
}
