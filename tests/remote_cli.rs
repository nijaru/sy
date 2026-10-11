//! CLI-to-v3 integration with only the external SSH transport substituted.
//! Authentication/configuration behavior belongs in real OpenSSH tests.

#![cfg(all(unix, feature = "ssh"))]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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
        let mut source_arg = if pull {
            remote_arg.clone()
        } else {
            local.as_os_str().to_owned()
        };
        source_arg.push("/");
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
        source_arg.push("/");
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

fn operands(
    source: &std::path::Path,
    destination: &std::path::Path,
    pull: bool,
) -> (OsString, OsString) {
    let remote = |path: &std::path::Path| {
        let mut value = OsString::from("test-peer:");
        value.push(path);
        value
    };
    if pull {
        (remote(source), destination.as_os_str().to_owned())
    } else {
        (source.as_os_str().to_owned(), remote(destination))
    }
}

#[tokio::test]
async fn selected_remote_files_bind_original_source_and_actual_destination() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    for pull in [false, true] {
        let scope = fixture.path().join(if pull { "pull" } else { "push" });
        let source_dir = scope.join("source ; ' $ ");
        let destination_dir = scope.join("destination ; ' $ ");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir(&destination_dir).unwrap();
        let name = if cfg!(target_os = "linux") {
            OsString::from_vec(b"original-\xff".to_vec())
        } else {
            OsString::from("original-日 ; ' $")
        };
        let source = source_dir.join(&name);
        let destination = destination_dir.join("renamed ; ' $");
        std::fs::write(&source, b"first bytes").unwrap();
        // Either sibling would fail a tree scan. Selected work must never
        // enumerate either namespace or authorize sibling deletion.
        let _socket = std::os::unix::net::UnixListener::bind(source_dir.join("socket")).unwrap();
        let unreadable = source_dir.join("unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o0)).unwrap();
        std::fs::write(destination_dir.join("protected-sibling"), b"keep").unwrap();
        let (source_arg, destination_arg) = operands(&source, &destination, pull);
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(std::fs::read(&destination).unwrap(), b"first bytes");
        assert!(!destination_dir.join(&name).exists());
        std::fs::write(&source, b"updated original bytes").unwrap();
        copy(
            &source_arg,
            &destination_arg,
            &["--backup", "--checksum"],
            &search_path,
        )
        .await;
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"updated original bytes"
        );
        assert_eq!(
            std::fs::read(destination_dir.join("renamed ; ' $~")).unwrap(),
            b"first bytes"
        );
        if !pull {
            copy(
                &source_arg,
                &destination_arg,
                &["--delete", "--force-delete"],
                &search_path,
            )
            .await;
        }
        assert_eq!(
            std::fs::read(destination_dir.join("protected-sibling")).unwrap(),
            b"keep"
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"updated original bytes");
        // An operator-selected directory symlink is followed only initially;
        // a selected leaf still keeps its source basename under the held FD.
        let destination_alias = scope.join("destination-alias");
        std::os::unix::fs::symlink(&destination_dir, &destination_alias).unwrap();
        let (source_arg, destination_arg) = operands(&source, &destination_alias, pull);
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(
            std::fs::read(destination_dir.join(&name)).unwrap(),
            b"updated original bytes"
        );
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[tokio::test]
async fn remote_directory_basename_and_contents_spellings_bind_real_tree_roots() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    for pull in [false, true] {
        for contents in [false, true] {
            let scope = fixture.path().join(format!("{pull}-{contents}"));
            let source = scope.join("tree");
            let destination = scope.join("destination");
            std::fs::create_dir_all(source.join("nested")).unwrap();
            std::fs::write(source.join("nested/file"), b"directory bytes").unwrap();
            let (mut source_arg, destination_arg) = operands(&source, &destination, pull);
            if contents {
                source_arg.push("/");
            }
            copy(
                &source_arg,
                &destination_arg,
                &["--preserve-times"],
                &search_path,
            )
            .await;
            let actual = if contents {
                destination.clone()
            } else {
                destination.join("tree")
            };
            assert_eq!(
                std::fs::read(actual.join("nested/file")).unwrap(),
                b"directory bytes"
            );
            assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
            assert!(!actual.join("tree").exists());
            assert_eq!(
                filetime::FileTime::from_last_modification_time(
                    &std::fs::metadata(source.join("nested")).unwrap()
                ),
                filetime::FileTime::from_last_modification_time(
                    &std::fs::metadata(actual.join("nested")).unwrap()
                )
            );
        }
    }
}

#[tokio::test]
async fn matching_preservation_through_ssh_does_not_touch_the_source() {
    use std::os::unix::fs::MetadataExt;

    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    let source = fixture.path().join("source");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    let file = source.join("nested/file");
    std::fs::write(&file, b"original source bytes").unwrap();
    xattr::set(&file, "user.sy-idempotent", b"original attribute").unwrap();
    #[cfg(feature = "acl")]
    let original_acls = {
        [&source, &source.join("nested"), &file].map(|path| {
            let mut entries = exacl::getfacl(path, None).unwrap();
            entries.push(exacl::AclEntry::allow_user(
                "1",
                exacl::Perm::READ,
                exacl::Flag::empty(),
            ));
            #[cfg(target_os = "linux")]
            entries.push(exacl::AclEntry::allow_mask(
                exacl::Perm::READ | exacl::Perm::EXECUTE,
                exacl::Flag::empty(),
            ));
            exacl::setfacl(&[path], &entries, None).unwrap();
            exacl::getfacl(path, None).unwrap()
        })
    };
    #[cfg(target_os = "macos")]
    for path in [&source, &source.join("nested"), &file] {
        use std::os::fd::AsRawFd;
        let held = std::fs::File::open(path).unwrap();
        // SAFETY: held is live; this fixture only adds the nonblocking NODUMP flag.
        assert_eq!(
            unsafe { libc::fchflags(held.as_raw_fd(), libc::UF_NODUMP) },
            0
        );
    }
    let observe = || {
        [&source, &source.join("nested"), &file].map(|path| {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        })
    };
    let original = observe();
    for pull in [false, true] {
        let (mut source_arg, destination_arg) = operands(&source, &source, pull);
        source_arg.push("/");
        let mut preservation_flags = vec!["-pX", "-ptX"];
        if cfg!(feature = "acl") {
            preservation_flags.push("-pA");
        }
        if cfg!(all(feature = "acl", target_os = "macos")) {
            preservation_flags.push("-pAF");
        }
        for flags in preservation_flags {
            copy(&source_arg, &destination_arg, &[flags], &search_path).await;
            assert_eq!(
                observe(),
                original,
                "equal preservation mutated source metadata"
            );
            #[cfg(feature = "acl")]
            assert_eq!(
                [&source, &source.join("nested"), &file]
                    .map(|path| exacl::getfacl(path, None).unwrap()),
                original_acls
            );
            #[cfg(target_os = "macos")]
            for path in [&source, &source.join("nested"), &file] {
                use std::os::macos::fs::MetadataExt;
                assert_eq!(std::fs::metadata(path).unwrap().st_flags(), libc::UF_NODUMP);
            }
            assert_eq!(std::fs::read(&file).unwrap(), b"original source bytes");
            assert_eq!(
                xattr::get(&file, "user.sy-idempotent").unwrap().unwrap(),
                b"original attribute"
            );
        }
    }
}

#[tokio::test]
async fn destination_directory_syntax_cannot_authorize_leaf_replacement() {
    use std::os::unix::fs::MetadataExt;

    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    let source = fixture.path().join("source");
    let destination = fixture.path().join("renamed");
    let link = fixture.path().join("leaf-link");
    std::fs::write(&source, b"original source").unwrap();
    std::fs::write(&destination, b"previous destination bytes").unwrap();
    std::os::unix::fs::symlink("renamed", &link).unwrap();
    let observation = |path: &std::path::Path| {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        (
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    let original = [
        observation(&source),
        observation(&destination),
        observation(&link),
    ];
    let log = std::env::split_paths(&search_path)
        .next()
        .unwrap()
        .join("ssh-launches");
    for pull in [false, true] {
        for target in [&destination, &link] {
            for suffix in ["/", "/."] {
                let (source_arg, mut destination_arg) = operands(&source, target, pull);
                destination_arg.push(suffix);
                let output = tokio::time::timeout(
                    Duration::from_secs(15),
                    tokio::process::Command::new(env!("CARGO_BIN_EXE_sy"))
                        .args([source_arg, destination_arg])
                        .args(["-a", "--quiet", "--no-hooks"])
                        .env("PATH", &search_path)
                        .env("SY_TEST_AGENT", env!("CARGO_BIN_EXE_sy"))
                        .env("SY_TEST_SSH_LOG", &log)
                        .kill_on_drop(true)
                        .output(),
                )
                .await
                .expect("directory-syntax refusal stalled")
                .unwrap();
                assert!(
                    !output.status.success(),
                    "directory syntax became leaf replacement"
                );
                assert_eq!(std::fs::read(&source).unwrap(), b"original source");
                assert_eq!(
                    std::fs::read(&destination).unwrap(),
                    b"previous destination bytes"
                );
                assert_eq!(
                    std::fs::read_link(&link).unwrap(),
                    std::path::Path::new("renamed")
                );
                assert_eq!(
                    [
                        observation(&source),
                        observation(&destination),
                        observation(&link)
                    ],
                    original
                );
                assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 4);
            }
        }
    }
}

#[tokio::test]
async fn directory_root_links_honor_explicit_directory_and_local_follow_intent() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    let actual = fixture.path().join("real");
    std::fs::create_dir(&actual).unwrap();
    std::fs::write(actual.join("file"), b"original directory bytes").unwrap();
    let alias = fixture.path().join("alias");
    std::os::unix::fs::symlink("real", &alias).unwrap();
    let observed = std::fs::symlink_metadata(&alias).unwrap();
    for (index, (pull, contents, follow)) in [
        (false, true, false),
        (true, true, false),
        (false, false, true),
        (false, true, true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut source = alias.as_os_str().to_owned();
        if contents {
            source.push("/");
        }
        let destination = fixture.path().join(format!("destination-{index}"));
        let (source_arg, destination_arg) =
            operands(std::path::Path::new(&source), &destination, pull);
        copy(
            &source_arg,
            &destination_arg,
            if follow { &["-L"] } else { &[] },
            &search_path,
        )
        .await;
        let root = if contents {
            destination
        } else {
            destination.join("alias")
        };
        assert_eq!(
            std::fs::read(root.join("file")).unwrap(),
            b"original directory bytes"
        );
    }
    // Without directory syntax or --copy-links, the same link stays opaque.
    for pull in [false, true] {
        let destination = fixture
            .path()
            .join(if pull { "opaque-pull" } else { "opaque-push" });
        let (source_arg, destination_arg) = operands(&alias, &destination, pull);
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(
            std::fs::read_link(destination).unwrap(),
            std::path::Path::new("real")
        );
    }
    let after = std::fs::symlink_metadata(&alias).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        (after.ino(), after.ctime(), after.ctime_nsec()),
        (observed.ino(), observed.ctime(), observed.ctime_nsec())
    );
}

#[tokio::test]
async fn remote_selected_links_are_opaque_and_type_transitions_are_transactional() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    for pull in [false, true] {
        let scope = fixture.path().join(if pull { "pull" } else { "push" });
        std::fs::create_dir(&scope).unwrap();
        let source = scope.join("source");
        let destination = scope.join("renamed");
        let (source_arg, destination_arg) = operands(&source, &destination, pull);
        std::fs::create_dir(scope.join("real-directory")).unwrap();
        std::fs::write(scope.join("real-directory/child"), b"not selected").unwrap();
        for target in ["absent", "source", "other", "real-directory"] {
            if std::fs::symlink_metadata(&source).is_ok() {
                std::fs::remove_file(&source).unwrap();
            }
            std::os::unix::fs::symlink(target, &source).unwrap();
            if target == "other" {
                std::os::unix::fs::symlink("source", scope.join("other")).unwrap();
            }
            // Isolate leaf replacement from destination-directory selection:
            // breaking the preceding source cycle would make the old destination
            // link designate real-directory, correctly selecting that directory.
            if std::fs::symlink_metadata(&destination).is_ok() {
                std::fs::remove_file(&destination).unwrap();
            }
            std::fs::write(&destination, b"old leaf").unwrap();
            copy(&source_arg, &destination_arg, &[], &search_path).await;
            assert_eq!(
                std::fs::read_link(&destination).unwrap(),
                std::path::Path::new(target)
            );
            assert_eq!(
                std::fs::read_link(&source).unwrap(),
                std::path::Path::new(target)
            );
        }
        assert_eq!(
            std::fs::read(scope.join("real-directory/child")).unwrap(),
            b"not selected"
        );
        // A directory-designating destination link selects that directory;
        // file-over-link replacement instead needs a nondirectory leaf target.
        std::fs::remove_file(&destination).unwrap();
        std::os::unix::fs::symlink("absent-for-file", &destination).unwrap();
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"file replaces opaque link").unwrap();
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"file replaces opaque link"
        );
        std::fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink("absent-again", &source).unwrap();
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(
            std::fs::read_link(&destination).unwrap(),
            std::path::Path::new("absent-again")
        );
        // Cyclic destination is a leaf, not a directory or its followed target.
        std::fs::remove_file(&destination).unwrap();
        std::os::unix::fs::symlink("renamed", &destination).unwrap();
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(
            std::fs::read_link(&destination).unwrap(),
            std::path::Path::new("absent-again")
        );
    }
}

#[tokio::test]
async fn remote_selected_missing_parents_are_acquired_only_for_chosen_create() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    for pull in [false, true] {
        let scope = fixture.path().join(if pull { "pull" } else { "push" });
        std::fs::create_dir(&scope).unwrap();
        let source = scope.join("source");
        std::fs::write(&source, b"selected bytes").unwrap();
        let selections: &[&[&str]] = &[
            &["--dry-run"],
            &["--exclude=*"],
            &["--min-size=100"],
            &["--max-size=1"],
            &["--existing"],
        ];
        for (index, flags) in selections.iter().enumerate() {
            let parent = scope.join(format!("missing-{index}"));
            let destination = parent.join("nested/renamed");
            let (source_arg, destination_arg) = operands(&source, &destination, pull);
            copy(&source_arg, &destination_arg, flags, &search_path).await;
            assert!(!parent.exists(), "{flags:?} created parents");
        }
        // Metadata-only classification must work on an unreadable regular file
        // when selection excludes reading it (ordinary macOS UID included).
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o0)).unwrap();
        let parent = scope.join("unreadable-preview");
        let (source_arg, destination_arg) = operands(&source, &parent.join("nested/renamed"), pull);
        copy(&source_arg, &destination_arg, &["--dry-run"], &search_path).await;
        assert!(!parent.exists());
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let destination = scope.join("created/nested/renamed");
        let (source_arg, destination_arg) = operands(&source, &destination, pull);
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(std::fs::read(&destination).unwrap(), b"selected bytes");
        std::fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink("absent", &source).unwrap();
        let parent = scope.join("skipped-link");
        let destination = parent.join("nested/renamed");
        let (source_arg, destination_arg) = operands(&source, &destination, pull);
        copy(
            &source_arg,
            &destination_arg,
            &["--links=skip"],
            &search_path,
        )
        .await;
        assert!(!parent.exists());
        copy(&source_arg, &destination_arg, &[], &search_path).await;
        assert_eq!(
            std::fs::read_link(destination).unwrap(),
            std::path::Path::new("absent")
        );
    }
}

#[tokio::test]
async fn unreadable_selected_destination_is_replaced_without_old_payload_reads() {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no pointer arguments or side effects.
    assert_ne!(unsafe { libc::geteuid() }, 0);
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    for pull in [false, true] {
        let scope = fixture.path().join(if pull { "pull" } else { "push" });
        std::fs::create_dir(&scope).unwrap();
        let source = scope.join("original");
        let destination = scope.join("renamed");
        // The push old file qualifies by size for optional signatures. Mode 000
        // must select whole bytes rather than fail that optimization request.
        let old_size = if pull {
            3
        } else {
            sy::remote::push::DEFAULT_REMOTE_DELTA_MIN_SIZE as usize
        };
        let payload = vec![b'n'; old_size + 1];
        std::fs::write(&source, &payload).unwrap();
        std::fs::write(&destination, vec![b'o'; old_size]).unwrap();
        std::fs::write(scope.join("sibling"), b"untouched").unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o0)).unwrap();
        assert_eq!(
            std::fs::File::open(&destination).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let source_observation = || {
            let stat = std::fs::metadata(&source).unwrap();
            (
                stat.dev(),
                stat.ino(),
                stat.nlink(),
                stat.len(),
                stat.mode(),
                stat.mtime(),
                stat.mtime_nsec(),
                stat.ctime(),
                stat.ctime_nsec(),
            )
        };
        let before = source_observation();
        let (source_arg, destination_arg) = operands(&source, &destination, pull);
        copy(
            &source_arg,
            &destination_arg,
            &["-p", "--verify=after"],
            &search_path,
        )
        .await;
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
        assert_eq!(std::fs::read(&source).unwrap(), payload);
        assert_eq!(source_observation(), before);
        assert_eq!(std::fs::read(scope.join("sibling")).unwrap(), b"untouched");
    }
}

fn ssh_path(root: &std::path::Path) -> OsString {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    // Only transport is substituted; paths remain real protocol data.
    std::fs::write(&ssh, b"#!/bin/sh\n[ \"$#\" -eq 3 ] && [ \"$1\" = test-peer ] && [ \"$2\" = sy ] && [ \"$3\" = __serve ] || exit 91\nprintf 'launch\\n' >> \"$SY_TEST_SSH_LOG\"\nexec \"$SY_TEST_AGENT\" __serve\n").unwrap();
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
    let log = std::env::split_paths(search_path)
        .next()
        .unwrap()
        .join("ssh-launches");
    let before = std::fs::read(&log).unwrap_or_default().len();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sy"));
    command
        .args([source, destination])
        .args(flags)
        .args(["--quiet", "--no-hooks"])
        .env("PATH", search_path)
        .env("SY_TEST_AGENT", env!("CARGO_BIN_EXE_sy"))
        .env("SY_TEST_SSH_LOG", &log)
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
    if source.as_bytes().starts_with(b"test-peer:")
        || destination.as_bytes().starts_with(b"test-peer:")
    {
        assert_eq!(
            std::fs::read(&log).unwrap().len() - before,
            b"launch\n".len(),
            "operand classification must not reconnect"
        );
    }
    output
}
