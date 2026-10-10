//! Real process-pipe completion with the external SSH command substituted.
//! Protocol responses and publication succeed before the wrapper's exit status.
#![cfg(all(unix, feature = "ssh"))]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

struct FakeSsh {
    root: tempfile::TempDir,
    path: OsString,
}

impl FakeSsh {
    fn new(script: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(
            &ssh,
            format!("#!/bin/sh\nprintf '%s' \"$$\" > \"$SY_TEST_PID\"\n{script}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        Self {
            root,
            path: std::env::join_paths(paths).unwrap(),
        }
    }

    async fn copy(
        &self,
        source: &Path,
        destination: &Path,
        pull: bool,
        flags: &[&str],
    ) -> std::process::Output {
        let mut remote = OsString::from("test-peer:");
        remote.push(if pull { source } else { destination });
        let (source, destination) = if pull {
            (remote, destination.as_os_str().to_owned())
        } else {
            (source.as_os_str().to_owned(), remote)
        };
        let mut source = source;
        source.push("/");
        let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
        command
            .args([source, destination])
            .args(flags)
            .args(["--quiet", "--no-hooks"])
            .env("PATH", &self.path)
            .env("SY_TEST_AGENT", env!("CARGO_BIN_EXE_sy"))
            .env("SY_TEST_PID", self.root.path().join("pid"))
            .env("SY_TEST_COMPLETED", self.root.path().join("completed"))
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .expect("SSH completion or termination was not bounded")
            .unwrap()
    }

    async fn assert_reaped(&self) {
        let pid = std::fs::read_to_string(self.root.path().join("pid")).unwrap();
        let status = Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(
            !status.success(),
            "owned SSH process {pid} still exists after CLI return"
        );
    }
}

#[tokio::test]
async fn completed_push_and_pull_require_clean_ssh_exit() {
    for pull in [false, true] {
        for code in [37, 0] {
            let fake = FakeSsh::new(&format!(
                "\"$SY_TEST_AGENT\" __serve || exit 91\nprintf completed > \"$SY_TEST_COMPLETED\"\nexit {code}"
            ));
            let source = fake.root.path().join("source");
            let destination = fake.root.path().join("destination");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("file"), b"verified publication").unwrap();
            let output = fake.copy(&source, &destination, pull, &[]).await;
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), code == 0, "{stderr}");
            if code != 0 {
                assert!(stderr.contains("OpenSSH exited unsuccessfully"), "{stderr}");
                assert!(stderr.contains("37"), "{stderr}");
            }
            // The agent only exits after client EOF. This proves completion,
            // not an early peer failure that happened to produce nonzero status.
            assert_eq!(
                std::fs::read(fake.root.path().join("completed")).unwrap(),
                b"completed"
            );
            assert_eq!(
                std::fs::read(destination.join("file")).unwrap(),
                b"verified publication"
            );
            assert_eq!(
                std::fs::read(source.join("file")).unwrap(),
                b"verified publication"
            );
            fake.assert_reaped().await;
        }
    }
}

#[tokio::test]
async fn ordinary_error_reaps_ssh_without_replacing_operation_error() {
    let fake = FakeSsh::new("exec \"$SY_TEST_AGENT\" __serve");
    let source = fake.root.path().join("source");
    let destination = fake.root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"original source").unwrap();
    let output = fake
        .copy(
            &source,
            &destination,
            false,
            &["--backup", "--backup-dir=/outside-root"],
        )
        .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--backup-dir must be relative"), "{stderr}");
    assert!(!destination.join("file").exists());
    assert_eq!(
        std::fs::read(source.join("file")).unwrap(),
        b"original source"
    );
    fake.assert_reaped().await;
}

#[tokio::test]
async fn completed_responses_do_not_allow_forever_hanging_ssh() {
    let fake = FakeSsh::new("\"$SY_TEST_AGENT\" __serve || exit 91\nprintf completed > \"$SY_TEST_COMPLETED\"\nexec /bin/sleep 60");
    let source = fake.root.path().join("source");
    let destination = fake.root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"published before timeout").unwrap();
    let output = fake.copy(&source, &destination, false, &[]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("completion exceeded its shutdown deadline"),
        "{stderr}"
    );
    assert!(fake.root.path().join("completed").exists());
    assert_eq!(
        std::fs::read(destination.join("file")).unwrap(),
        b"published before timeout"
    );
    assert_eq!(
        std::fs::read(source.join("file")).unwrap(),
        b"published before timeout"
    );
    fake.assert_reaped().await;
}
