use super::*;

#[test]
#[ignore = "requires Linux 6.12+ native statx mount IDs and mounted procfs"]
fn rejects_fake_proc_root_and_genuine_wrong_task_subtrees() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(fixture.path().join("self/fd")).unwrap();
    let held = File::open(fixture.path()).unwrap();
    std::os::unix::fs::symlink(
        "/constant",
        fixture.path().join(format!("self/fd/{}", held.as_raw_fd())),
    )
    .unwrap();
    let fake =
        source_observations::operator::directory_at(libc::AT_FDCWD, fixture.path().as_os_str())
            .unwrap();
    assert!(ProcAddress::from_root(fake, held.as_raw_fd())
        .unwrap()
        .is_none());
    let wrong_task =
        source_observations::operator::directory_at(libc::AT_FDCWD, OsStr::new("/proc/1")).unwrap();
    assert!(ProcAddress::from_root(wrong_task, held.as_raw_fd())
        .unwrap()
        .is_none());
}

#[test]
#[ignore = "requires Linux 6.12+ native statx mount IDs and mounted procfs"]
fn pinned_current_thread_link_detects_persistent_deep_ancestor_relocation() {
    let fixture = tempfile::tempdir().unwrap();
    let creator = fixture.path().join("ancestor/deep/creator");
    std::fs::create_dir_all(&creator).unwrap();
    std::fs::create_dir(fixture.path().join("source")).unwrap();
    let held = File::open(&creator).unwrap();
    let address = ProcAddress::new(held.as_raw_fd()).unwrap().unwrap();
    let before = address.read().unwrap().unwrap();
    let original = stat_fd(held.as_raw_fd()).unwrap();
    std::fs::rename(
        fixture.path().join("ancestor"),
        fixture.path().join("source/arrival"),
    )
    .unwrap();
    assert_ne!(address.read().unwrap().unwrap(), before);
    assert!(staging::same_inode(
        &original,
        &stat_fd(held.as_raw_fd()).unwrap()
    ));
}
