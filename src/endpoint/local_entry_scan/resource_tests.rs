use super::traversal::*;
use super::*;
use std::io;

type TestScan = (
    tokio::sync::mpsc::Receiver<Result<Entry, BoxError>>,
    tokio::task::JoinHandle<Result<(), LocalScanError>>,
);

#[cfg(unix)]
#[tokio::test]
async fn unsafe_scratch_is_refused_before_scanner_writes() {
    use futures::TryStreamExt;
    if let Some(root) = std::env::var_os("SY_TEST_UNSAFE_SCRATCH_ROOT") {
        let root = PathBuf::from(root);
        let names = || {
            let mut names: Vec<_> = std::fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let before_names = names();
        let before_mtime = std::fs::metadata(&root).unwrap().modified().unwrap();
        let scratch = tempfile::env::temp_dir();
        let scratch_mtime = std::fs::metadata(&scratch).unwrap().modified().unwrap();
        let local = local_entry_stream(root.clone(), ScanRequest::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap_err();
        assert!(local
            .to_string()
            .contains("temporary directory is inside scanned root"));
        let rooted = crate::rooted_fs::RootedFs::open(root.clone())
            .await
            .unwrap();
        let remote = rooted
            .entry_stream(ScanRequest::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap_err();
        assert!(remote
            .to_string()
            .contains("temporary directory is inside scanned root"));
        assert_eq!(names(), before_names);
        assert_eq!(
            std::fs::metadata(root).unwrap().modified().unwrap(),
            before_mtime
        );
        assert_eq!(
            std::fs::metadata(&scratch).unwrap().modified().unwrap(),
            scratch_mtime
        );
        assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 0);
        return;
    }
    let root = tempfile::TempDir::new().unwrap();
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::write(root.path().join("file"), b"source").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "endpoint::local_entry_scan::resource_tests::unsafe_scratch_is_refused_before_scanner_writes"])
        .env("SY_TEST_UNSAFE_SCRATCH_ROOT", root.path())
        .env("TMPDIR", &scratch).output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tiny_worker(root: PathBuf) -> TestScan {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let worker = tokio::task::spawn_blocking(move || {
        walk_tree(
            &root,
            ScanRequest::default(),
            &sender,
            crate::engine::scan_sort::SortBudget {
                bytes: 90,
                records: 3,
                fan_in: 2,
            },
        )
    });
    (receiver, worker)
}

#[cfg(unix)]
#[tokio::test]
async fn deep_wide_tiny_budget_low_fd_and_cancellation_cleanup() {
    if let Some(root) = std::env::var_os("SY_TEST_LOCAL_SCAN_FD_ROOT") {
        let root = PathBuf::from(root);
        let scratch_parent = tempfile::env::temp_dir();
        let root_modified = std::fs::metadata(&root).unwrap().modified().unwrap();
        let (mut receiver, worker) = tiny_worker(root.clone());
        let mut count = 0;
        let mut previous = None;
        while let Some(entry) = receiver.recv().await {
            let entry = entry.unwrap();
            if let Some(previous) = &previous {
                assert!(previous < &entry.path);
            }
            previous = Some(entry.path);
            count += 1;
        }
        worker.await.unwrap().unwrap();
        assert_eq!(count, 180 * 2 + 511);
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            2,
            "source names changed"
        );

        let (mut receiver, worker) = tiny_worker(root.clone());
        assert!(receiver.recv().await.unwrap().is_ok());
        drop(receiver);
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap();
        match result {
            Ok(()) => {}
            Err(LocalScanError::Walk(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            other => panic!("cancellation failed: {other:?}"),
        }
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            2,
            "cancelled scratch leaked"
        );

        let (mut receiver, worker) = tiny_worker(root.clone());
        assert!(receiver.recv().await.unwrap().is_ok());
        let scratch = std::fs::read_dir(&scratch_parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("sy-scan-")
            })
            .unwrap();
        let moved_scratch = scratch_parent.join("injected-scratch-failure");
        // Rename is atomic even while a run is being written; recursive
        // removal here would race the writer and make fault injection flaky.
        std::fs::rename(scratch, &moved_scratch).unwrap();
        while receiver.recv().await.is_some() {}
        assert!(
            worker.await.unwrap().is_err(),
            "scratch failure became successful EOF"
        );
        std::fs::remove_dir_all(moved_scratch).unwrap();
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            2,
            "source names changed after scratch failure"
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().modified().unwrap(),
            root_modified
        );
        assert_eq!(
            std::fs::read_dir(scratch_parent).unwrap().count(),
            0,
            "scratch leaked"
        );
        return;
    }
    let root = tempfile::TempDir::new().unwrap();
    let mut directory = root.path().to_path_buf();
    for _ in 0..180 {
        std::fs::create_dir(directory.join("a")).unwrap();
        std::fs::write(directory.join("z"), b"sibling").unwrap();
        directory.push("a");
    }
    for index in (0..511).rev() {
        std::fs::write(directory.join(format!("n{index:04}")), b"x").unwrap();
    }
    // The isolated child has a real FD ceiling below its traversal depth;
    // scratch is outside the source so bookkeeping cannot mutate it.
    let scratch = tempfile::TempDir::new().unwrap();
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "endpoint::local_entry_scan::resource_tests::deep_wide_tiny_budget_low_fd_and_cancellation_cleanup"])
        .env("SY_TEST_LOCAL_SCAN_FD_ROOT", root.path()).env("TMPDIR", scratch.path());
    // SAFETY: pre_exec uses only async-signal-safe syscalls and stack buffers;
    // it does not allocate, take locks, or touch the async runtime.
    unsafe {
        child.pre_exec(|| {
            let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut limit = limit.assume_init();
            limit.rlim_cur = limit.rlim_cur.min(32);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = child.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unsupported_special_files_and_oversized_ignore_are_loud_errors() {
    use futures::TryStreamExt;
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::TempDir::new().unwrap();
    let fifo = std::ffi::CString::new(root.path().join("fifo").as_os_str().as_bytes()).unwrap();
    // SAFETY: fifo is a valid NUL-terminated path; mkfifo does not open it.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        local_entry_stream(root.path().to_path_buf(), ScanRequest::default())
            .try_collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    std::fs::remove_file(root.path().join("fifo")).unwrap();
    std::fs::write(root.path().join("file"), b"x").unwrap();
    std::fs::write(root.path().join(".ignore"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    let result = local_entry_stream(root.path().to_path_buf(), ScanRequest::default())
        .try_collect::<Vec<_>>()
        .await;
    assert!(result.unwrap_err().to_string().contains("exceeds"));
}
