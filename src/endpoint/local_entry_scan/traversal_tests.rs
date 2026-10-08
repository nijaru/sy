use super::*;

#[cfg(unix)]
#[test]
fn reopening_validates_every_ancestor_even_when_leaf_inode_is_unchanged() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("parent/child")).unwrap();
    let mut stack = Stack::new().unwrap();
    let mut directory = Directory::root(root.path()).unwrap();
    for (depth, name) in [None, Some("parent"), Some("child")]
        .into_iter()
        .enumerate()
    {
        if let Some(name) = name {
            directory = directory.child(OsStr::new(name), false).unwrap();
        }
        stack
            .write(
                depth,
                Frame {
                    run: 0,
                    offset: 0,
                    identity: directory.identity().unwrap(),
                },
            )
            .unwrap();
    }
    drop(directory);
    std::fs::rename(root.path().join("parent"), root.path().join("old")).unwrap();
    std::fs::create_dir(root.path().join("parent")).unwrap();
    std::fs::rename(
        root.path().join("old/child"),
        root.path().join("parent/child"),
    )
    .unwrap();
    assert!(matches!(
        reopen(root.path(), Path::new("parent/child"), false, &mut stack),
        Err(LocalScanError::DirectoryChanged(_))
    ));
    std::fs::remove_dir_all(root.path().join("parent")).unwrap();
    std::os::unix::fs::symlink(root.path().join("old"), root.path().join("parent")).unwrap();
    assert!(reopen(root.path(), Path::new("parent/child"), false, &mut stack).is_err());
}
