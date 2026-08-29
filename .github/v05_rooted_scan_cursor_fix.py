from pathlib import Path

path = Path("src/rooted_fs/scan.rs")
text = path.read_text()

old = '''#[cfg(target_os = "linux")]
fn stat_times(stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    let mtime_nsec =
        u32::try_from(stat.st_mtime_nsec).map_err(|_| RootedScanError::UnsupportedPlatform)?;
    let ctime_nsec =
        u32::try_from(stat.st_ctime_nsec).map_err(|_| RootedScanError::UnsupportedPlatform)?;
    Ok((stat.st_mtime, mtime_nsec, stat.st_ctime, ctime_nsec))
}

#[cfg(target_os = "macos")]
fn stat_times(stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    let mtime_nsec = u32::try_from(stat.st_mtimespec.tv_nsec)
        .map_err(|_| RootedScanError::UnsupportedPlatform)?;
    let ctime_nsec = u32::try_from(stat.st_ctimespec.tv_nsec)
        .map_err(|_| RootedScanError::UnsupportedPlatform)?;
    Ok((
        stat.st_mtimespec.tv_sec,
        mtime_nsec,
        stat.st_ctimespec.tv_sec,
        ctime_nsec,
    ))
}
'''
new = '''#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stat_times(stat: &libc::stat) -> Result<(i64, u32, i64, u32), RootedScanError> {
    let mtime_nsec =
        u32::try_from(stat.st_mtime_nsec).map_err(|_| RootedScanError::UnsupportedPlatform)?;
    let ctime_nsec =
        u32::try_from(stat.st_ctime_nsec).map_err(|_| RootedScanError::UnsupportedPlatform)?;
    Ok((stat.st_mtime, mtime_nsec, stat.st_ctime, ctime_nsec))
}
'''
if old not in text:
    raise SystemExit("stat_times marker missing")
text = text.replace(old, new, 1)

old = '''fn directory_names(fd: RawFd) -> Result<Vec<OsString>, RootedScanError> {
    let duplicate = unsafe {
        // SAFETY: `fd` is a live directory descriptor and dup creates an
        // independent descriptor for fdopendir to own.
        libc::dup(fd)
    };
    if duplicate < 0 {
        return Err(RootedScanError::ReadDirectory(io::Error::last_os_error()));
    }
    let dir = unsafe {
        // SAFETY: `duplicate` is a fresh directory descriptor. fdopendir takes
        // ownership on success.
        libc::fdopendir(duplicate)
    };
    if dir.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            // SAFETY: fdopendir failed, so ownership remained with us.
            libc::close(duplicate);
        }
        return Err(RootedScanError::ReadDirectory(error));
    }
'''
new = '''fn directory_names(fd: RawFd) -> Result<Vec<OsString>, RootedScanError> {
    let scan_fd = unsafe {
        // SAFETY: `fd` is a live directory descriptor and `.` is a fixed native
        // component. Reopening it creates a distinct open file description, so
        // concurrent scans do not share the directory offset as they would with
        // dup(2).
        libc::openat(
            fd,
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if scan_fd < 0 {
        return Err(RootedScanError::ReadDirectory(io::Error::last_os_error()));
    }
    let dir = unsafe {
        // SAFETY: `scan_fd` is a fresh directory descriptor. fdopendir takes
        // ownership on success.
        libc::fdopendir(scan_fd)
    };
    if dir.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            // SAFETY: fdopendir failed, so ownership remained with us.
            libc::close(scan_fd);
        }
        return Err(RootedScanError::ReadDirectory(error));
    }
'''
if old not in text:
    raise SystemExit("directory_names marker missing")
text = text.replace(old, new, 1)

marker = '''    #[tokio::test]
    async fn zero_depth_scan_emits_no_entries() {
'''
test = '''    #[tokio::test]
    async fn concurrent_scans_have_independent_directory_cursors() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("a"), b"a").unwrap();
        std::fs::write(root.path().join("dir/b"), b"b").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();

        let (left, right) = tokio::join!(
            collect(&rooted, ScanRequest::default()),
            collect(&rooted, ScanRequest::default()),
        );
        let expected = [Path::new("a"), Path::new("dir"), Path::new("dir/b")];
        for entries in [&left, &right] {
            assert_eq!(entries.len(), expected.len());
            assert!(entries
                .iter()
                .zip(expected)
                .all(|(entry, path)| entry.path.as_path() == path));
        }
    }

'''
if "concurrent_scans_have_independent_directory_cursors" not in text:
    if marker not in text:
        raise SystemExit("test insertion marker missing")
    text = text.replace(marker, test + marker, 1)

path.write_text(text)
