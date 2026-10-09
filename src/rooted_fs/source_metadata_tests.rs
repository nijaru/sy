use super::*;
use crate::endpoint::local_identity::metadata_identity;

fn path(value: &str) -> RelativePath {
    RelativePath::new(value).unwrap()
}

#[test]
fn source_metadata_requires_the_observed_file_or_directory() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("source")).unwrap();
    std::fs::write(root.path().join("source/file"), b"source").unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let file = path("source/file");
    let directory = path("source");
    let directory_identity = rooted
        .path_identity_blocking(&directory)
        .unwrap()
        .unwrap()
        .1;
    xattr::set(root.path().join("source/file"), "user.sy-proof", b"A").unwrap();
    // Attribute writes change the regular-file observation; scan afterward.
    let file_identity = rooted.path_identity_blocking(&file).unwrap().unwrap().1;
    assert!(rooted
        .read_observed_xattrs_blocking(&file, EntryKind::File, file_identity)
        .unwrap()
        .iter()
        .any(|(name, value)| name == "user.sy-proof" && value == b"A"));
    assert!(rooted
        .read_observed_xattrs_blocking(&directory, EntryKind::Directory, directory_identity)
        .is_ok());

    std::fs::rename(root.path().join("source"), root.path().join("original")).unwrap();
    std::fs::create_dir(root.path().join("source")).unwrap();
    std::fs::write(root.path().join("source/file"), b"foreign").unwrap();
    for (relative, kind, expected) in [
        (&file, EntryKind::File, file_identity),
        (&directory, EntryKind::Directory, directory_identity),
    ] {
        assert!(matches!(
            rooted.read_observed_xattrs_blocking(relative, kind, expected),
            Err(RootedFsError::SourceMetadataChanged(_))
        ));
        #[cfg(feature = "acl")]
        assert!(matches!(
            rooted.read_observed_acl_blocking(relative, kind, expected),
            Err(RootedFsError::SourceMetadataChanged(_))
        ));
        #[cfg(target_os = "macos")]
        assert!(matches!(
            rooted.read_observed_bsd_flags_blocking(relative, kind, expected),
            Err(RootedFsError::SourceMetadataChanged(_))
        ));
    }
}

#[test]
fn source_metadata_revalidates_ancestor_binding_after_held_read() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("source")).unwrap();
    std::fs::write(root.path().join("source/file"), b"source").unwrap();
    xattr::set(root.path().join("source/file"), "user.sy-proof", b"A").unwrap();
    let rooted = RootedFs::open_blocking(root.path().to_path_buf()).unwrap();
    let relative = path("source/file");
    let expected = rooted.path_identity_blocking(&relative).unwrap().unwrap().1;
    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        rooted.read_observed_metadata_blocking(&relative, EntryKind::File, expected, |held| {
            opened_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            // The value still belongs to A, but the source path now names B.
            read_xattrs_from_file(held)
        })
    });
    opened_rx.recv().unwrap();
    let swap = (|| -> std::io::Result<()> {
        std::fs::rename(root.path().join("source"), root.path().join("original"))?;
        std::fs::create_dir(root.path().join("source"))?;
        std::fs::write(root.path().join("source/file"), b"foreign")?;
        Ok(())
    })();
    // Always release and drain the worker before assertions.
    continue_tx.send(()).unwrap();
    let result = worker.join().unwrap();
    swap.unwrap();
    assert_eq!(
        metadata_identity(
            &std::fs::metadata(root.path().join("original/file")).unwrap(),
            EntryKind::File
        ),
        Some(expected)
    );
    assert!(matches!(
        result,
        Err(RootedFsError::SourceMetadataChanged(_))
    ));
}
