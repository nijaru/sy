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
    let search_path = ssh_path(fixture.path());
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

#[tokio::test]
async fn remote_selection_checksum_and_absent_preview_follow_shared_cli_policy() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    let source = fixture.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("keep.bin"), b"original").unwrap();
    std::fs::write(source.join("small.bin"), b"x").unwrap();
    std::fs::write(source.join("large.bin"), [b'x'; 17]).unwrap();
    std::fs::write(source.join("excluded.tmp"), b"excluded").unwrap();
    std::fs::create_dir(source.join("ignored")).unwrap();
    std::fs::write(source.join("ignored/hidden.bin"), b"hidden").unwrap();
    std::os::unix::fs::symlink("keep.bin", source.join("link.bin")).unwrap();
    let flags = [
        "--include=*.bin",
        "--exclude=ignored/",
        "--exclude=*.tmp",
        "--min-size=4",
        "--max-size=16",
        "--links=skip",
    ];

    for pull in [true, false] {
        std::fs::write(source.join("keep.bin"), b"original").unwrap();
        let destination = fixture
            .path()
            .join(if pull { "pull/nested" } else { "push/nested" });
        let mut source_arg = source.as_os_str().to_os_string();
        let mut destination_arg = destination.as_os_str().to_os_string();
        let remote = if pull {
            &mut source_arg
        } else {
            &mut destination_arg
        };
        let mut address = OsString::from("test-peer:");
        address.push(&*remote);
        *remote = address;
        let mut preview_flags = flags.to_vec();
        preview_flags.extend(["--dry-run", "--json"]);
        let preview = copy(&source_arg, &destination_arg, &preview_flags, &search_path).await;
        let summary = std::str::from_utf8(&preview.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|event| event["type"] == "summary")
            .expect("dry-run JSON summary missing");
        assert_eq!(summary["files_created"], 1);
        assert!(!destination.parent().unwrap().exists());
        assert_eq!(std::fs::read(source.join("keep.bin")).unwrap(), b"original");

        copy(&source_arg, &destination_arg, &flags, &search_path).await;
        assert_eq!(
            std::fs::read(destination.join("keep.bin")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
        let observed_time = std::fs::metadata(source.join("keep.bin"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::File::open(destination.join("keep.bin"))
            .unwrap()
            .set_modified(observed_time)
            .unwrap();
        let preview_existing =
            copy(&source_arg, &destination_arg, &preview_flags, &search_path).await;
        assert!(std::str::from_utf8(&preview_existing.stdout)
            .unwrap()
            .lines()
            .any(|line| {
                let event: serde_json::Value = serde_json::from_str(line).unwrap();
                event["type"] == "summary" && event["files_skipped"] == 1
            }));
        assert_eq!(
            std::fs::read(destination.join("keep.bin")).unwrap(),
            b"original"
        );
        // Same size AND timestamps: --checksum must compare content, not silently
        // use quick equality or refuse an otherwise supported synchronization.
        std::fs::write(source.join("keep.bin"), b"modified").unwrap();
        let modified = std::fs::metadata(source.join("keep.bin"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::File::open(destination.join("keep.bin"))
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let mut checksum_flags = flags.to_vec();
        checksum_flags.push("--checksum");
        copy(&source_arg, &destination_arg, &checksum_flags, &search_path).await;
        assert_eq!(
            std::fs::read(destination.join("keep.bin")).unwrap(),
            b"modified"
        );
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(source.join("ignored/hidden.bin")).unwrap(),
            b"hidden"
        );
    }
}

#[tokio::test]
async fn transferred_file_times_follow_explicit_policy_in_every_direction() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    let old = filetime::FileTime::from_unix_time(1_600_000_000, 123_456_000);
    for direction in ["local", "local-stream", "push", "pull"] {
        for preserve in [false, true] {
            let scope = fixture.path().join(format!("{direction}-{preserve}"));
            let source = scope.join("source");
            let destination = scope.join("destination");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::create_dir(&destination).unwrap();
            for name in ["create", "update", "replace"] {
                let file = source.join(name);
                std::fs::write(&file, b"selected source bytes").unwrap();
                filetime::set_file_mtime(file, old).unwrap();
            }
            std::fs::write(destination.join("update"), b"old").unwrap();
            std::os::unix::fs::symlink("absent", destination.join("replace")).unwrap();
            let remote = |path: &std::path::Path| {
                let mut operand = OsString::from("test-peer:");
                operand.push(path);
                operand
            };
            let mut source_operand = if direction == "pull" {
                remote(&source)
            } else {
                source.as_os_str().to_owned()
            };
            // Local operands use rsync's explicit contents spelling.
            source_operand.push("/");
            let destination_operand = if direction == "push" {
                remote(&destination)
            } else {
                destination.as_os_str().to_owned()
            };
            let before = std::time::SystemTime::now() - Duration::from_secs(2);
            let mut flags = Vec::new();
            if preserve {
                flags.push("-t");
            }
            if direction == "local-stream" {
                flags.push("--bwlimit=1M");
            }
            copy(&source_operand, &destination_operand, &flags, &search_path).await;
            let after = std::time::SystemTime::now() + Duration::from_secs(2);
            for name in ["create", "update", "replace"] {
                let file = destination.join(name);
                assert_eq!(std::fs::read(&file).unwrap(), b"selected source bytes");
                let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
                if preserve {
                    assert_eq!(
                        filetime::FileTime::from_system_time(modified),
                        old,
                        "{direction}: {name}"
                    );
                } else {
                    assert!(
                        modified >= before && modified <= after,
                        "{direction}: {name} inherited a source/old timestamp: {modified:?}"
                    );
                }
                assert_eq!(
                    filetime::FileTime::from_last_modification_time(
                        &std::fs::metadata(source.join(name)).unwrap()
                    ),
                    old
                );
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "acl"))]
#[tokio::test]
async fn acl_only_refresh_uses_native_mode_in_every_direction() {
    use std::os::unix::fs::MetadataExt;

    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    for direction in ["local", "push", "pull"] {
        let scope = fixture.path().join(direction);
        let source = scope.join("source");
        let destination = scope.join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let mut entries = exacl::from_mode(0o640);
        entries.push(exacl::AclEntry::allow_user(
            "12345",
            exacl::Perm::READ | exacl::Perm::WRITE,
            exacl::Flag::empty(),
        ));
        entries.push(exacl::AclEntry::allow_mask(
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        for (name, destination_mode) in [("different-mode", 0o600), ("equal-mode", 0o640)] {
            let source_file = source.join(name);
            let destination_file = destination.join(name);
            // Quick equality deliberately does not imply equal bytes. ACL-only
            // refresh must preserve OLD destination bytes and inode identity.
            std::fs::write(&source_file, b"source").unwrap();
            std::fs::write(&destination_file, b"target").unwrap();
            exacl::setfacl(&[&source_file], &entries, None).unwrap();
            std::fs::set_permissions(
                &destination_file,
                std::fs::Permissions::from_mode(destination_mode),
            )
            .unwrap();
            for file in [&source_file, &destination_file] {
                filetime::set_file_mtime(
                    file,
                    filetime::FileTime::from_unix_time(1_600_000_000, 123),
                )
                .unwrap();
            }
        }
        let before = ["different-mode", "equal-mode"]
            .map(|name| std::fs::metadata(destination.join(name)).unwrap());
        let remote = |path: &std::path::Path| {
            let mut operand = OsString::from("test-peer:");
            operand.push(path);
            operand
        };
        let mut source_operand = if direction == "pull" {
            remote(&source)
        } else {
            source.as_os_str().to_owned()
        };
        source_operand.push("/");
        let destination_operand = if direction == "push" {
            remote(&destination)
        } else {
            destination.as_os_str().to_owned()
        };
        copy(&source_operand, &destination_operand, &["-A"], &search_path).await;
        for (name, before) in ["different-mode", "equal-mode"].into_iter().zip(before) {
            let file = destination.join(name);
            let after = std::fs::metadata(&file).unwrap();
            assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
            assert_eq!(after.mode() & 0o7777, 0o640);
            assert_eq!(std::fs::read(&file).unwrap(), b"target");
            assert_eq!(
                exacl::getfacl(&file, None).unwrap(),
                exacl::getfacl(source.join(name), None).unwrap()
            );
            assert_eq!(std::fs::read(source.join(name)).unwrap(), b"source");
        }
    }
}

fn ssh_path(root: &std::path::Path) -> OsString {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    // Only transport is substituted; paths remain real protocol data.
    std::fs::write(&ssh, b"#!/bin/sh\nexec \"$SY_TEST_AGENT\" __serve\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut search_path = vec![bin];
    search_path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(search_path).unwrap()
}

async fn copy(
    source: &OsStr,
    destination: &OsStr,
    flags: &[&str],
    search_path: &OsStr,
) -> std::process::Output {
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
    output
}
