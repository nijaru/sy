use super::*;

#[test]
#[ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4 or Btrfs"]
fn physical_parents_protect_ancestors_not_independent_directories() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("source")).unwrap();
    std::fs::create_dir(fixture.path().join("destination")).unwrap();
    let root = File::open(fixture.path().join("source")).unwrap();
    let parent = File::open(fixture.path()).unwrap();
    let destination = File::open(fixture.path().join("destination")).unwrap();
    let expected = object_key(root.as_raw_fd(), None).unwrap().unwrap();
    let ancestor = object_key(parent.as_raw_fd(), None).unwrap().unwrap();
    let independent = object_key(destination.as_raw_fd(), None).unwrap().unwrap();
    assert!(excludes(root.as_raw_fd(), expected, independent).unwrap());
    assert!(!excludes(root.as_raw_fd(), expected, expected).unwrap());
    assert!(!excludes(root.as_raw_fd(), expected, ancestor).unwrap());
    let mut foreign = independent;
    foreign.0[4] ^= 1;
    assert!(!excludes(root.as_raw_fd(), expected, foreign).unwrap());
    std::fs::remove_dir(fixture.path().join("source")).unwrap();
    assert!(!excludes(root.as_raw_fd(), expected, independent).unwrap());
}
