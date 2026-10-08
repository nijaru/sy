//! CLI-to-v3 integration with only the external SSH transport substituted.
//! Authentication/configuration behavior belongs in real OpenSSH tests.

#![cfg(all(unix, feature = "ssh"))]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

#[tokio::test]
async fn remote_roots_and_filenames_survive_cli_dispatch_in_both_directions() {
    let fixture = tempfile::TempDir::new().unwrap();
    let bin = fixture.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    // Execute the real agent over process pipes, without requiring credentials
    // or an SSH daemon. The shell never sees any endpoint filesystem path.
    std::fs::write(&ssh, b"#!/bin/sh\nexec \"$SY_TEST_AGENT\" __serve\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut search_path = vec![bin];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let search_path = std::env::join_paths(search_path).unwrap();
    // APFS rejects non-UTF-8 names; Linux exercises the raw-byte contract.
    let name = if cfg!(target_os = "linux") {
        OsString::from_vec(b"file-\xff".to_vec())
    } else {
        OsString::from("file-日 space")
    };
    let root_name = if cfg!(target_os = "linux") {
        OsString::from_vec(b"remote-\xfe".to_vec())
    } else {
        OsString::from("remote-日 space ' ; $")
    };

    for pull in [false, true] {
        let scope = fixture.path().join(if pull { "pull" } else { "push" });
        let local = scope.join("local");
        let remote = scope.join(&root_name);
        let (source, destination) = if pull {
            (&remote, &local)
        } else {
            (&local, &remote)
        };
        std::fs::create_dir_all(source).unwrap();
        std::fs::write(source.join(&name), b"native path content").unwrap();
        for directory in ["left", "right"] {
            std::fs::create_dir(source.join(directory)).unwrap();
            std::fs::write(source.join(directory).join(&name), directory.as_bytes()).unwrap();
        }
        let mut remote_arg = OsString::from("test-peer:");
        remote_arg.push(&remote);
        let source_arg = if pull {
            remote_arg.clone()
        } else {
            local.as_os_str().to_owned()
        };
        let destination_arg = if pull {
            local.as_os_str().to_owned()
        } else {
            remote_arg
        };

        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(
            std::fs::read(destination.join(&name)).unwrap(),
            b"native path content"
        );

        // Size changes force replacement without mtime sleeps. Two equal
        // basenames in different directories must not share a backup slot.
        std::fs::write(source.join(&name), b"changed native path content").unwrap();
        for directory in ["left", "right"] {
            std::fs::write(source.join(directory).join(&name), b"changed content").unwrap();
        }
        copy(
            &source_arg,
            &destination_arg,
            &["--backup", "--backup-dir=backups"],
            &search_path,
        )
        .await;
        let mut backup_name = name.clone();
        backup_name.push("~");
        assert_eq!(
            std::fs::read(destination.join("backups").join(&backup_name)).unwrap(),
            b"native path content"
        );
        for directory in ["left", "right"] {
            assert_eq!(
                std::fs::read(
                    destination
                        .join("backups")
                        .join(directory)
                        .join(&backup_name)
                )
                .unwrap(),
                directory.as_bytes()
            );
        }
    }
}

async fn copy(source: &OsStr, destination: &OsStr, flags: &[&str], search_path: &OsStr) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sy"));
    command
        .args([source, destination])
        .args(flags)
        .args(["--quiet", "--no-hooks"])
        .env("PATH", search_path)
        .env("SY_TEST_AGENT", env!("CARGO_BIN_EXE_sy"))
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("remote CLI transfer stalled")
        .unwrap();
    assert!(
        output.status.success(),
        "remote copy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
