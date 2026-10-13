//! Delta PULL through real `sy __serve`, the shared preflight/controller, and
//! native local publication. No SSH connection or global configuration.
#![cfg(unix)]
use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Stdio;
use std::time::Duration;
use sy::engine::controller::{preflight_sync, SyncController};
use sy::engine::planner::{ComparisonMode, ComparisonPolicy};
use sy::engine::scan::{EntryMetadataRequest, ScanRequest};
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::remote::pull::RemotePullExecutor;
use sy::remote::runtime::ClientRemoteSession;
use tokio::process::Command;

fn fixture() -> Vec<u8> {
    let mut state = 0x1234_5678_90ab_cdef_u64;
    (0..12 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[tokio::test]
async fn routed_delta_pull_reuses_old_blocks_and_preserves_native_metadata() {
    let old = fixture();
    let mut new = b"shift the entire basis by an unaligned prefix".to_vec();
    new.extend_from_slice(&old);
    new[2 * 1024 * 1024..2 * 1024 * 1024 + 8192].fill(b'x');
    let digest = blake3::hash(&new);
    for preserve in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let source_path = source.path().join("file");
        let destination_path = destination.path().join("file");
        std::fs::write(&source_path, &new).unwrap();
        std::fs::write(&destination_path, &old).unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&destination_path, std::fs::Permissions::from_mode(0o640))
            .unwrap();
        xattr::set(&source_path, "user.sy-delta", b"SOURCE").unwrap();
        xattr::set(&destination_path, "user.sy-delta", b"OLD").unwrap();
        filetime::set_file_mtime(
            &source_path,
            filetime::FileTime::from_unix_time(1_600_000_000, 123),
        )
        .unwrap();
        let source_before = std::fs::metadata(&source_path).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg("__serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = ClientRemoteSession::connect(
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
            sy::protocol::Operation::Pull,
            source.path(),
            Default::default(),
        )
        .await
        .unwrap();
        assert!(client.capabilities().delta_pull);
        let mut executor = RemotePullExecutor::new(
            destination.path().to_path_buf(),
            client.request_handle(),
            client.sender(),
            Scheduler::new(ResourceBudget {
                active_files: 1,
                ..Default::default()
            })
            .unwrap(),
        )
        .with_xattrs(preserve)
        .with_acls(preserve && cfg!(feature = "acl"))
        .with_compression(Some(sy::engine::compression::CompressionPolicy::Always));
        let request = ScanRequest {
            metadata: EntryMetadataRequest {
                identity: true,
                unix_mode: true,
                ..Default::default()
            },
            respect_gitignore: false,
            ..Default::default()
        };
        let (namespace, dest) = executor.destination_entries(request).await.unwrap();
        let remote = client.scan(request).await.unwrap();
        let plan = preflight_sync(
            remote,
            dest,
            ComparisonPolicy {
                mode: ComparisonMode::SizeOnly,
                preserve_permissions: preserve,
                preserve_times: preserve,
                ..Default::default()
            },
            None,
            sy::engine::namespace::NamespacePreflight::new(namespace.as_ref()),
            |_| false,
        )
        .await
        .unwrap();
        let summary = tokio::time::timeout(
            Duration::from_secs(30),
            SyncController::new(executor, NonZeroUsize::new(1).unwrap()).execute(plan),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            summary.delta_files, 1,
            "large changed regular files must use the routed delta policy"
        );
        assert_eq!(summary.files_transferred, 1);
        assert!(
            summary.reused_bytes > 11 * 1024 * 1024,
            "actual OLD reuse: {summary:?}"
        );
        assert!(
            summary.literal_bytes < 32 * 1024,
            "incremental literals: {summary:?}"
        );
        assert_eq!(
            summary.reused_bytes + summary.literal_bytes,
            new.len() as u64
        );
        let actual = std::fs::read(&destination_path).unwrap();
        assert_eq!(blake3::hash(&actual), digest);
        assert_eq!(actual, new);
        assert_eq!(std::fs::read(&source_path).unwrap(), new);
        let source_after = std::fs::metadata(&source_path).unwrap();
        assert_eq!(
            (
                source_after.ino(),
                source_after.mode(),
                source_after.ctime(),
                source_after.ctime_nsec()
            ),
            (
                source_before.ino(),
                source_before.mode(),
                source_before.ctime(),
                source_before.ctime_nsec()
            )
        );
        let metadata = std::fs::metadata(&destination_path).unwrap();
        assert_eq!(
            metadata.mode() & 0o777,
            if preserve { 0o600 } else { 0o640 }
        );
        assert_eq!(
            xattr::get(&destination_path, "user.sy-delta")
                .unwrap()
                .unwrap(),
            if preserve {
                b"SOURCE".as_slice()
            } else {
                b"OLD".as_slice()
            }
        );
        if preserve {
            assert_eq!(
                (metadata.mtime(), metadata.mtime_nsec()),
                (1_600_000_000, 123)
            );
        }
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        drop(client);
        assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    }
}

#[tokio::test]
async fn four_concurrent_delta_pulls_progress_with_server_byte_admission() {
    // Each OLD produces 3,072 signatures, exceeding the router's 128-frame
    // window. Four client workers compete for only two server delta permits.
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let old = fixture();
    let mut new = old.clone();
    new.extend_from_slice(b"changed EOF");
    for index in 0..4 {
        std::fs::write(source.path().join(index.to_string()), &new).unwrap();
        std::fs::write(destination.path().join(index.to_string()), &old).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg("__serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = ClientRemoteSession::connect(
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
        sy::protocol::Operation::Pull,
        source.path(),
        Default::default(),
    )
    .await
    .unwrap();
    let mut executor = RemotePullExecutor::new(
        destination.path().to_path_buf(),
        client.request_handle(),
        client.sender(),
        Scheduler::new(ResourceBudget::default()).unwrap(),
    );
    let request = ScanRequest {
        metadata: EntryMetadataRequest {
            identity: true,
            unix_mode: true,
            ..Default::default()
        },
        respect_gitignore: false,
        ..Default::default()
    };
    let (namespace, dest) = executor.destination_entries(request).await.unwrap();
    let remote = client.scan(request).await.unwrap();
    let plan = preflight_sync(
        remote,
        dest,
        ComparisonPolicy {
            mode: ComparisonMode::SizeOnly,
            ..Default::default()
        },
        None,
        sy::engine::namespace::NamespacePreflight::new(namespace.as_ref()),
        |_| false,
    )
    .await
    .unwrap();
    let summary = tokio::time::timeout(
        Duration::from_secs(15),
        SyncController::new(executor, NonZeroUsize::new(4).unwrap()).execute(plan),
    )
    .await
    .expect("waiting signature uploads must not starve admitted completion ACKs")
    .unwrap();
    assert_eq!(summary.delta_files, 4);
    assert_eq!(summary.files_transferred, 4);
    assert!(summary.reused_bytes >= 4 * old.len() as u64);
    for index in 0..4 {
        assert_eq!(
            std::fs::read(destination.path().join(index.to_string())).unwrap(),
            new
        );
        assert_eq!(
            std::fs::read(source.path().join(index.to_string())).unwrap(),
            new
        );
    }
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 4);
    drop(client);
    assert!(tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
}

#[cfg(feature = "ssh")]
#[tokio::test]
async fn cli_delta_pull_keeps_sparse_logical_bytes_and_eof_integrity() {
    use std::io::{Seek, SeekFrom, Write};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    let bin = root.path().join("bin");
    for path in [&source, &destination, &bin] {
        std::fs::create_dir(path).unwrap();
    }
    // This substitutes only the per-process external command. It opens no SSH
    // connection and reads/writes no SSH or global configuration.
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nexec \"$SY_TEST_AGENT\" __serve\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (path, extra) in [(&source, true), (&destination, false)] {
        let mut file = std::fs::File::create(path.join("file")).unwrap();
        file.set_len(12 * 1024 * 1024).unwrap();
        file.seek(SeekFrom::Start(6 * 1024 * 1024)).unwrap();
        file.write_all(b"nonzero middle extent").unwrap();
        if extra {
            file.seek(SeekFrom::End(0)).unwrap();
            file.write_all(b"changed EOF").unwrap();
        }
    }
    let original = std::fs::read(source.join("file")).unwrap();
    let mut remote = std::ffi::OsString::from("pipe-only-peer:");
    remote.push(&source);
    remote.push("/");
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(remote)
            .arg(&destination)
            .args(["--json", "--no-hooks"])
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("SY_TEST_AGENT", env!("CARGO_BIN_EXE_sy"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let summary = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == "summary")
        .unwrap();
    assert_eq!(summary["files_updated"], 1);
    assert!(
        summary["bytes_transferred"].as_u64().unwrap() < 64 * 1024,
        "must execute the delta caller: {summary}"
    );
    let actual = std::fs::read(destination.join("file")).unwrap();
    assert_eq!(blake3::hash(&actual), blake3::hash(&original));
    assert_eq!(actual, original);
    assert_eq!(std::fs::read(source.join("file")).unwrap(), original);
    assert_eq!(std::fs::read_dir(destination).unwrap().count(), 1);
}
