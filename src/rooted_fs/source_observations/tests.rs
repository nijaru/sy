use super::*;
use futures::TryStreamExt;

async fn scanned(root: &Path) -> (RootedFs, Vec<crate::engine::domain::Entry>) {
    let rooted = RootedFs::open(root.to_path_buf()).await.unwrap();
    let entries = rooted
        .source_entry_stream(Default::default())
        .try_collect()
        .await
        .unwrap();
    (rooted, entries)
}

#[tokio::test]
async fn original_scan_parent_is_required_before_first_data_open_and_for_every_hardlink_member() {
    for representative in [true, false] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("selected")).unwrap();
        std::fs::create_dir(root.path().join("alternate")).unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        for name in ["selected/member", "alternate/member"] {
            std::fs::hard_link(root.path().join("file"), root.path().join(name)).unwrap();
        }
        let (rooted, entries) = scanned(root.path()).await;
        let member = entries
            .iter()
            .find(|entry| entry.path.as_path() == Path::new("selected/member"))
            .unwrap();
        let opener = entries
            .iter()
            .find(|entry| entry.path.as_path() == Path::new("file"))
            .unwrap();
        let source = (!representative).then(|| {
            crate::remote::source_file::SourceFile::capture_blocking(
                rooted.clone(),
                opener.path.clone(),
                opener.identity.unwrap(),
                opener.size,
            )
            .unwrap()
        });
        std::fs::rename(root.path().join("selected"), root.path().join("held")).unwrap();
        std::fs::rename(root.path().join("alternate"), root.path().join("selected")).unwrap();
        assert_eq!(
            rooted.path_identity_blocking(&member.path).unwrap(),
            Some((EntryKind::File, member.identity.unwrap())),
            "identical inode is not binding evidence"
        );
        if let Some(source) = source {
            assert!(source
                .validate_member_blocking(&member.path, member.identity.unwrap())
                .is_err());
        } else {
            assert!(crate::remote::source_file::SourceFile::capture_blocking(
                rooted.clone(),
                member.path.clone(),
                member.identity.unwrap(),
                member.size
            )
            .is_err());
        }
        assert!(rooted.open_regular_blocking(&member.path).is_err());
        assert_eq!(
            std::fs::read(root.path().join("held/member")).unwrap(),
            b"original"
        );
    }
}

#[tokio::test]
async fn omitted_names_remain_original_and_pruned_directories_do_not_enumerate_descendants() {
    let root = tempfile::tempdir().unwrap();
    for name in [".git", "ignored", "depth"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    std::fs::write(root.path().join(".ignore"), b"ignored/\n").unwrap();
    // A considered special entry is an error; these children must never be
    // considered because their containing directory was excluded/pruned.
    for name in [".git/socket", "ignored/socket", "depth/socket"] {
        let _socket = std::os::unix::net::UnixListener::bind(root.path().join(name)).unwrap();
    }
    let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
    let entries: Vec<_> = rooted
        .source_entry_stream(crate::engine::scan::ScanRequest {
            respect_gitignore: true,
            include_git_dir: false,
            max_depth: Some(1),
            ..Default::default()
        })
        .try_collect()
        .await
        .unwrap();
    assert!(!entries.iter().any(|entry| [".git", "ignored"]
        .iter()
        .any(|name| entry.path.as_path() == Path::new(name))));
    for name in [".git", "ignored", "depth"] {
        rooted
            .validate_source_observation_blocking(&RelativePath::new(name).unwrap())
            .unwrap();
    }
    let mut owner = rooted.source_observations.lock().unwrap();
    let catalogue = owner.active().unwrap();
    catalogue.begin().unwrap();
    for name in [".git/socket", "ignored/socket", "depth/socket"] {
        assert!(catalogue.get(path_key(Path::new(name))).unwrap().is_none());
    }
    catalogue.healthy = true;
    drop(owner);
    std::fs::rename(root.path().join("ignored"), root.path().join("old")).unwrap();
    std::fs::create_dir(root.path().join("ignored")).unwrap();
    assert!(matches!(
        rooted.validate_source_observation_blocking(&RelativePath::new("ignored").unwrap()),
        Err(RootedFsError::SourceBindingChanged(_))
    ));
}

#[tokio::test]
async fn directly_excluded_special_names_have_bindings_without_transfer_capability() {
    for local in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".ignore"), b"socket\n").unwrap();
        std::fs::write(root.path().join("file"), b"ordinary").unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
        let source = crate::endpoint::source_root::SourceRoot::open(root.path().to_path_buf())
            .await
            .unwrap();
        let rooted = source.rooted();
        let stream = if local {
            source.entries(Default::default())
        } else {
            rooted.source_entry_stream(Default::default())
        };
        let entries: Vec<_> = stream.try_collect().await.unwrap();
        assert!(entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new("file")));
        assert!(!entries
            .iter()
            .any(|entry| entry.path.as_path() == Path::new("socket")));
        rooted
            .validate_source_observation_blocking(&RelativePath::new("socket").unwrap())
            .unwrap();
        // .ignore applies independently of the gitignore option. A separate
        // unexcluded fixture proves recording has not enabled special transfer.
        let included = tempfile::tempdir().unwrap();
        let _included_socket =
            std::os::unix::net::UnixListener::bind(included.path().join("socket")).unwrap();
        let included_source =
            crate::endpoint::source_root::SourceRoot::open(included.path().to_path_buf())
                .await
                .unwrap();
        let stream = if local {
            included_source.entries(Default::default())
        } else {
            included_source
                .rooted()
                .source_entry_stream(Default::default())
        };
        assert!(stream.try_collect::<Vec<_>>().await.is_err());
    }
}

#[tokio::test]
async fn catalogue_corruption_and_truncation_poison_real_read_callers() {
    for truncate in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        let (rooted, entries) = scanned(root.path()).await;
        {
            let mut owner = rooted.source_observations.lock().unwrap();
            let catalogue = owner.active().unwrap();
            if truncate {
                catalogue.file.set_len(catalogue.end - 1).unwrap();
            } else {
                let locator = catalogue
                    .index
                    .get_bytes(path_key(Path::new("file")))
                    .unwrap()
                    .unwrap();
                let offset = u64::from_le_bytes(locator[..8].try_into().unwrap());
                catalogue.file.seek(SeekFrom::Start(offset)).unwrap();
                catalogue.file.write_all(&[99]).unwrap();
            }
        }
        assert!(rooted.open_regular_blocking(&entries[0].path).is_err());
        assert!(matches!(
            rooted.open_regular_blocking(&entries[0].path),
            Err(RootedFsError::SourceCatalogueTerminal)
        ));
        assert!(
            rooted
                .source_entry_stream(Default::default())
                .try_collect::<Vec<_>>()
                .await
                .is_err(),
            "rescan must not repair scratch authority"
        );
    }
}

#[tokio::test]
async fn cancelled_source_scan_cannot_reuse_a_partial_attempt() {
    for local in [false, true] {
        let root = tempfile::tempdir().unwrap();
        // More than either producer's bounded channel, so closing after one
        // item cannot be mistaken for naturally exhausted work.
        for number in 0..600 {
            std::fs::write(root.path().join(format!("file-{number:04}")), b"original").unwrap();
        }
        let source = crate::endpoint::source_root::SourceRoot::open(root.path().to_path_buf())
            .await
            .unwrap();
        let rooted = source.rooted();
        let mut stream = if local {
            source.entries(Default::default())
        } else {
            rooted.source_entry_stream(Default::default())
        };
        let entry = stream.try_next().await.unwrap().unwrap();
        stream.close().await.unwrap();
        assert!(matches!(
            rooted.open_regular_blocking(&entry.path),
            Err(RootedFsError::SourceCatalogueTerminal)
        ));
        let stream = if local {
            source.entries(Default::default())
        } else {
            rooted.source_entry_stream(Default::default())
        };
        assert!(stream.try_collect::<Vec<_>>().await.is_err());
    }
}

#[tokio::test]
async fn rescan_cannot_adopt_a_replaced_original() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"original").unwrap();
    let (rooted, _) = scanned(root.path()).await;
    std::fs::rename(root.path().join("file"), root.path().join("held")).unwrap();
    std::fs::write(root.path().join("file"), b"original").unwrap();
    assert!(rooted
        .source_entry_stream(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .is_err());
}
