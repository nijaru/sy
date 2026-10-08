use super::*;

#[cfg(windows)]
#[test]
fn windows_link_identity_respects_follow_policy_and_detects_cycles() {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    let root = tempfile::TempDir::new().unwrap();
    let directory = root.path().join("directory");
    let alias = root.path().join("alias");
    let dangling = root.path().join("dangling");
    std::fs::create_dir(&directory).unwrap();
    // This test requires Windows Developer Mode or symlink privilege.
    symlink_dir(&directory, &alias).unwrap();
    symlink_file(root.path().join("absent"), &dangling).unwrap();
    let link_metadata = std::fs::symlink_metadata(&alias).unwrap();
    let target_metadata = std::fs::metadata(&alias).unwrap();
    assert!(
        path_identity(&alias, &link_metadata, false).unwrap()
            != path_identity(&directory, &target_metadata, false).unwrap()
    );
    assert!(
        path_identity(&alias, &target_metadata, true).unwrap()
            == path_identity(&directory, &target_metadata, true).unwrap()
    );
    let dangling_metadata = std::fs::symlink_metadata(&dangling).unwrap();
    assert!(path_identity(&dangling, &dangling_metadata, false).is_ok());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
    walk_tree(
        root.path(),
        ScanRequest::default(),
        &sender,
        SortBudget::default(),
    )
    .unwrap();
    drop(sender);
    let mut entries = Vec::new();
    while let Some(entry) = receiver.blocking_recv() {
        entries.push(entry.unwrap());
    }
    assert!(entries
        .iter()
        .any(|entry| entry.path.as_path() == Path::new("dangling")
            && entry.kind == crate::engine::domain::EntryKind::Symlink));
    symlink_dir(root.path(), root.path().join("a-loop")).unwrap();
    let (sender, _receiver) = tokio::sync::mpsc::channel(16);
    assert!(matches!(
        walk_tree(
            root.path(),
            ScanRequest {
                follow_symlinks: true,
                ..ScanRequest::default()
            },
            &sender,
            SortBudget::default()
        ),
        Err(LocalScanError::SymlinkLoop(_))
    ));
}

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
