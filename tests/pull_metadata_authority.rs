//! The source can alias a local pull destination even when the peer is read-only.
#![cfg(all(target_os = "macos", feature = "ssh"))]

use std::os::macos::fs::MetadataExt;
use std::os::unix::fs::{MetadataExt as UnixMetadataExt, PermissionsExt};
use std::time::Duration;
use sy::engine::domain::{EntryKind, RelativePath};
use sy::rooted_fs::RootedFs;

#[tokio::test]
async fn flags_only_pull_refuses_destination_alias_of_excluded_source() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    let bin = fixture.path().join("bin");
    for path in [&source, &destination, &bin] {
        std::fs::create_dir(path).unwrap();
    }
    for name in ["a", "b"] {
        std::fs::write(source.join(name), b"same bytes").unwrap();
        std::fs::set_permissions(source.join(name), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        filetime::set_file_mtime(
            source.join(name),
            filetime::FileTime::from_unix_time(1_600_000_000, 0),
        )
        .unwrap();
    }
    let rooted = RootedFs::open(source.clone()).await.unwrap();
    rooted
        .write_bsd_flags_blocking(
            &RelativePath::new("a").unwrap(),
            EntryKind::File,
            libc::UF_NODUMP,
        )
        .unwrap();
    std::fs::hard_link(source.join("b"), destination.join("a")).unwrap();
    let source_before = std::fs::metadata(source.join("b")).unwrap();
    let inode = source_before.ino();
    assert_eq!(source_before.st_flags(), 0);
    std::fs::write(
        bin.join("ssh"),
        b"#!/bin/sh\nexec \"$SY_TEST_AGENT\" __serve\n",
    )
    .unwrap();
    std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_sy"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("SY_TEST_AGENT", env!("CARGO_BIN_EXE_sy"))
            .args(["--size-only", "--preserve-flags", "--exclude", "b"])
            .arg(format!("test-peer:{}/", source.display()))
            .arg(&destination)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    for path in [source.join("b"), destination.join("a")] {
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.ino(), inode);
        assert_eq!(metadata.st_flags(), 0);
        assert_eq!(metadata.mode(), source_before.mode());
        assert_eq!(metadata.mtime(), source_before.mtime());
        assert_eq!(metadata.ctime(), source_before.ctime());
        assert_eq!(metadata.ctime_nsec(), source_before.ctime_nsec());
    }
    assert_eq!(std::fs::read(source.join("b")).unwrap(), b"same bytes");
    assert_eq!(std::fs::read_dir(destination).unwrap().count(), 1);
    assert!(
        !output.status.success(),
        "shared metadata must be explicitly refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("shared regular file"), "{stderr}");
}
