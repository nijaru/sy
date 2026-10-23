use super::*;

#[cfg(not(target_os = "linux"))]
#[test]
fn unqualified_backend_refuses_before_any_native_lookup() {
    for path in [".", "dir/..", "../outside"] {
        // An invalid FD makes an attempted native lookup fail with EBADF.
        // Unsupported must instead be decided before acquiring any candidate.
        assert!(matches!(
            directory(-1, Path::new(path), Path::new("entry")),
            Err(RootedFsError::SourceFollowUnsupported(path)) if path == Path::new("entry")
        ));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_floor_is_source_root_not_current_directory() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("dir")).unwrap();
    let root = File::open(fixture.path()).unwrap();
    let child = File::open(fixture.path().join("dir")).unwrap();
    let actual = directory(root.as_raw_fd(), Path::new("dir/.."), Path::new("entry")).unwrap();
    assert!(
        NativeObservation::from_stat(&stat_fd(actual.as_raw_fd()).unwrap())
            .unwrap()
            .same_binding(
                NativeObservation::from_stat(&stat_fd(root.as_raw_fd()).unwrap()).unwrap()
            )
    );
    assert!(matches!(
        directory(child.as_raw_fd(), Path::new(".."), Path::new("entry")),
        Err(RootedFsError::SourceFollowUnsupported(_))
    ));
    assert!(matches!(
        directory(
            root.as_raw_fd(),
            Path::new("dir/../../"),
            Path::new("entry")
        ),
        Err(RootedFsError::SourceFollowUnsupported(_))
    ));
    let outside = tempfile::tempdir().unwrap();
    std::fs::rename(fixture.path().join("dir"), outside.path().join("parked")).unwrap();
    assert!(matches!(
        directory(root.as_raw_fd(), Path::new("dir/.."), Path::new("entry")),
        Err(RootedFsError::SourceBindingChanged(_))
    ));
}
