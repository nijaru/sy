#![cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};

fn sync(source: &Path, destination: &Path, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(format!("{}/", source.display()))
        .arg(destination)
        .arg("--no-hooks")
        .args(flags)
        .output()
        .unwrap()
}
#[cfg(target_os = "macos")]
#[test]
fn pruned_source_admits_copy_update_and_deletion_backups_without_reading_contents() {
    for nonempty in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        let opaque = source.join(".git");
        std::fs::create_dir(&opaque).unwrap();
        let hidden = opaque.join("deep/unreadable");
        if nonempty {
            std::fs::create_dir(opaque.join("deep")).unwrap();
            std::fs::write(&hidden, b"protected original").unwrap();
            std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
        } else {
            std::fs::set_permissions(&opaque, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let hidden_before = nonempty.then(|| std::fs::metadata(&hidden).unwrap());
        let before = std::fs::symlink_metadata(&opaque).unwrap();
        std::fs::write(source.join("new"), b"new file").unwrap();
        std::fs::write(source.join("updated"), b"new replacement bytes").unwrap();
        std::fs::write(destination.join("updated"), b"old bytes").unwrap();
        std::fs::write(destination.join("deleted"), b"deleted OLD").unwrap();
        let backup = fixture.path().join("backup");
        let output = sync(
            &source,
            &destination,
            &[
                "--delete",
                "--force-delete",
                "--backup",
                "--backup-dir",
                backup.to_str().unwrap(),
                "--exclude-vcs",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(destination.join("new")).unwrap(), b"new file");
        assert_eq!(
            std::fs::read(destination.join("updated")).unwrap(),
            b"new replacement bytes"
        );
        assert!(!destination.join("deleted").exists());
        assert!(!destination.join(".git").exists());
        assert_eq!(
            std::fs::read(backup.join("updated~")).unwrap(),
            b"old bytes"
        );
        assert_eq!(
            std::fs::read(backup.join("deleted~")).unwrap(),
            b"deleted OLD"
        );
        let after = std::fs::symlink_metadata(&opaque).unwrap();
        assert_eq!(
            (
                after.ino(),
                after.nlink(),
                after.mode(),
                after.mtime(),
                after.mtime_nsec(),
                after.ctime(),
                after.ctime_nsec()
            ),
            (
                before.ino(),
                before.nlink(),
                before.mode(),
                before.mtime(),
                before.mtime_nsec(),
                before.ctime(),
                before.ctime_nsec()
            )
        );
        assert_eq!(std::fs::read(source.join("new")).unwrap(), b"new file");
        assert_eq!(
            std::fs::read(source.join("updated")).unwrap(),
            b"new replacement bytes"
        );
        std::fs::set_permissions(opaque, std::fs::Permissions::from_mode(0o700)).unwrap();
        if let Some(before) = hidden_before {
            let after = std::fs::metadata(&hidden).unwrap();
            assert_eq!(
                (
                    after.ino(),
                    after.mode(),
                    after.nlink(),
                    after.mtime(),
                    after.mtime_nsec(),
                    after.ctime(),
                    after.ctime_nsec()
                ),
                (
                    before.ino(),
                    before.mode(),
                    before.nlink(),
                    before.mtime(),
                    before.mtime_nsec(),
                    before.ctime(),
                    before.ctime_nsec()
                )
            );
            std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(std::fs::read(hidden).unwrap(), b"protected original");
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn off_path_operator_links_are_protected_from_replacement_deletion_and_timestamps() {
    for effect in ["replace", "delete", "timestamp"] {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("physical");
        let destination = fixture.path().join("operator");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(source.join("a-first"), b"SOURCE").unwrap();
        let original = destination.join("link");
        std::os::unix::fs::symlink(&source, &original).unwrap();
        let flags = match effect {
            "replace" => {
                std::fs::write(source.join("link"), b"replacement").unwrap();
                vec!["-j1"]
            }
            "delete" => vec!["-j1", "--delete", "--force-delete"],
            "timestamp" => {
                std::os::unix::fs::symlink(&source, source.join("link")).unwrap();
                let time = filetime::FileTime::from_unix_time(100, 123);
                filetime::set_symlink_file_times(source.join("link"), time, time).unwrap();
                vec!["-j1", "-t"]
            }
            _ => unreachable!(),
        };
        let state = || {
            let stat = std::fs::symlink_metadata(&original).unwrap();
            (
                stat.ino(),
                stat.nlink(),
                stat.mtime(),
                stat.mtime_nsec(),
                stat.ctime(),
                stat.ctime_nsec(),
            )
        };
        let before = state();
        let output = sync(&original, &destination, &flags);
        assert!(
            !output.status.success(),
            "{effect} modified original operator LINK"
        );
        assert!(
            !destination.join("a-first").exists(),
            "{effect} escaped complete preflight"
        );
        assert_eq!(state(), before);
        assert_eq!(std::fs::read_link(&original).unwrap(), source);
        assert_eq!(std::fs::read(source.join("a-first")).unwrap(), b"SOURCE");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn guarded_hardlink_groups_fit_low_fd_budget_without_per_member_pins() {
    use std::os::unix::process::CommandExt;

    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    let mut originals = Vec::new();
    for group in 0..3 {
        let original = source.join(format!("g{group}-0"));
        std::fs::write(&original, [group]).unwrap();
        for member in 1..8 {
            std::fs::hard_link(&original, source.join(format!("g{group}-{member}"))).unwrap();
        }
        originals.push(identity(&original));
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
    command
        .arg(format!("{}/", source.display()))
        .arg(&destination)
        .args(["-H", "--no-hooks", "--json"]);
    // SAFETY: the child performs only setrlimit before exec; no allocation,
    // locks, or access to parent state occurs in the post-fork hook.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 32,
                rlim_max: 32,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (group, before) in originals.into_iter().enumerate() {
        assert_eq!(identity(&source.join(format!("g{group}-0"))), before);
        let expected = identity(&destination.join(format!("g{group}-0")));
        for member in 0..8 {
            let path = destination.join(format!("g{group}-{member}"));
            assert_eq!(identity(&path), expected);
            assert_eq!(std::fs::read(path).unwrap(), [group as u8]);
        }
    }
    assert_eq!(std::fs::read_dir(destination).unwrap().count(), 24);
}

fn refused(output: Output) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("original SOURCE"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn identity(path: &Path) -> (u64, u64, u64, i64, i64) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    (
        metadata.dev(),
        metadata.ino(),
        metadata.nlink(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}
fn directories(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let source = root.join("source");
    let destination = root.join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&destination).unwrap();
    (source, destination)
}

#[test]
fn source_root_inside_destination_is_not_delete_authority() {
    let fixture = tempfile::tempdir().unwrap();
    let destination = fixture.path();
    let source = destination.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("input"), b"SOURCE input").unwrap();
    let before = identity(&source.join("input"));
    refused(sync(&source, destination, &["--delete", "--force-delete"]));
    assert_eq!(
        std::fs::read(source.join("input")).unwrap(),
        b"SOURCE input"
    );
    assert_eq!(identity(&source.join("input")), before);
    assert!(
        !destination.join("input").exists(),
        "unrelated copy preceded admission"
    );
}

#[test]
fn backup_cannot_overwrite_an_excluded_original_source_name() {
    for symlink_replacement in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        if symlink_replacement {
            std::os::unix::fs::symlink("literal", source.join("a")).unwrap();
        } else {
            std::fs::write(source.join("a"), b"replacement source bytes").unwrap();
        }
        std::fs::write(source.join("a~"), b"protected SOURCE").unwrap();
        std::fs::write(destination.join("a"), b"OLD").unwrap();
        let before = identity(&source.join("a~"));
        refused(
            Command::new(env!("CARGO_BIN_EXE_sy"))
                .arg(format!("{}/", source.display()))
                .arg(&destination)
                .args(["--no-hooks", "--exclude", "a~", "--backup", "--backup-dir"])
                .arg(&source)
                .output()
                .unwrap(),
        );
        assert_eq!(
            std::fs::read(source.join("a~")).unwrap(),
            b"protected SOURCE"
        );
        assert_eq!(identity(&source.join("a~")), before);
        assert_eq!(std::fs::read(destination.join("a")).unwrap(), b"OLD");
    }
}

#[test]
fn excluded_source_directory_does_not_admit_destination_writes() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path();
    let destination = source.join("dest");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(source.join("input"), b"SOURCE").unwrap();
    let before = identity(&destination);
    refused(sync(source, &destination, &["--exclude", "dest/"]));
    assert_eq!(identity(&destination), before);
    assert!(!destination.join("input").exists());
}

#[test]
fn retiring_old_cannot_change_an_omitted_source_hardlink() {
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    std::fs::write(source.join("target"), b"replacement source bytes").unwrap();
    std::fs::write(source.join("protected"), b"protected SOURCE bytes").unwrap();
    std::fs::hard_link(source.join("protected"), destination.join("target")).unwrap();
    let before = identity(&source.join("protected"));
    refused(sync(&source, &destination, &["--exclude", "protected"]));
    assert_eq!(identity(&source.join("protected")), before);
    assert_eq!(
        std::fs::read(destination.join("target")).unwrap(),
        b"protected SOURCE bytes"
    );
}

#[test]
fn delete_only_plan_cannot_retire_an_omitted_source_alias() {
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    for root in [&source, &destination] {
        std::fs::write(root.join("keep"), b"same").unwrap();
    }
    std::fs::write(source.join("protected"), b"SOURCE").unwrap();
    std::fs::hard_link(source.join("protected"), destination.join("stale")).unwrap();
    let before = identity(&source.join("protected"));
    refused(sync(
        &source,
        &destination,
        &[
            "--size-only",
            "--exclude",
            "protected",
            "--delete",
            "--force-delete",
        ],
    ));
    assert_eq!(identity(&source.join("protected")), before);
    assert_eq!(std::fs::read(destination.join("stale")).unwrap(), b"SOURCE");
}

#[cfg(target_os = "macos")]
#[test]
fn directory_time_only_plan_preserves_source_and_finalizes_after_children() {
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    for root in [&source, &destination] {
        std::fs::create_dir(root.join("dir")).unwrap();
    }
    std::fs::write(source.join("dir/file"), b"SOURCE bytes").unwrap();
    std::fs::write(destination.join("dir/file"), b"OLD").unwrap();
    let stamp = filetime::FileTime::from_unix_time(1234, 567_890_123);
    filetime::set_file_mtime(source.join("dir"), stamp).unwrap();
    let before = [
        identity(&source),
        identity(&source.join("dir")),
        identity(&source.join("dir/file")),
    ];
    let result = sync(&source, &destination, &["-t"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read(destination.join("dir/file")).unwrap(),
        b"SOURCE bytes"
    );
    let directory = std::fs::metadata(destination.join("dir")).unwrap();
    assert_eq!(
        (directory.mtime(), directory.mtime_nsec()),
        (1234, 567_890_123)
    );
    assert_eq!(
        [
            identity(&source),
            identity(&source.join("dir")),
            identity(&source.join("dir/file"))
        ],
        before
    );
    assert_eq!(
        std::fs::read_dir(destination.join("dir")).unwrap().count(),
        1
    );
}

#[test]
fn directory_setter_only_plan_is_admitted_before_finalization() {
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    for root in [&source, &destination] {
        std::fs::create_dir(root.join("dir")).unwrap();
    }
    std::fs::set_permissions(source.join("dir"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(
        destination.join("dir"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let before = identity(&destination.join("dir"));
    let source_before = identity(&source.join("dir"));
    let output = sync(&source, &destination, &["-p"]);
    #[cfg(target_os = "macos")]
    {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let after = identity(&destination.join("dir"));
        assert_eq!((after.0, after.1, after.2), (before.0, before.1, before.2));
        assert_eq!(
            std::fs::metadata(destination.join("dir")).unwrap().mode() & 0o777,
            0o700
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        refused(output);
        assert_eq!(identity(&destination.join("dir")), before);
    }
    assert_eq!(identity(&source.join("dir")), source_before);
}

#[test]
fn directory_policy_cannot_change_an_off_path_original_operator_ancestor() {
    let fixture = tempfile::tempdir().unwrap();
    let physical = fixture.path().join("physical");
    let source = physical.join("root");
    let operator = fixture.path().join("operator");
    std::fs::create_dir_all(source.join("operator")).unwrap();
    std::fs::create_dir(&operator).unwrap();
    std::fs::set_permissions(
        source.join("operator"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&operator, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(source.join("a-file"), b"SOURCE").unwrap();
    std::os::unix::fs::symlink(&physical, operator.join("alias")).unwrap();
    let before = identity(&operator);
    let source_before = identity(&source.join("a-file"));
    refused(sync(&operator.join("alias/root"), fixture.path(), &["-p"]));
    assert_eq!(identity(&operator), before);
    assert_eq!(std::fs::metadata(&operator).unwrap().mode() & 0o777, 0o755);
    assert_eq!(identity(&source.join("a-file")), source_before);
    assert!(
        !fixture.path().join("a-file").exists(),
        "whole-plan admission must precede copies"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn selected_apfs_leaf_copies_and_independent_old_backup_use_native_admission() {
    for same_parent in [false, true] {
        for changed_old in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join("foo");
            std::fs::write(&source, b"selected SOURCE bytes").unwrap();
            let target = if same_parent {
                fixture.path().join("bar")
            } else {
                let directory = fixture.path().join("destination");
                std::fs::create_dir(&directory).unwrap();
                directory.join("bar")
            };
            if changed_old {
                std::fs::write(&target, b"OLD").unwrap();
                std::fs::write(target.with_file_name("bar~"), b"previous backup").unwrap();
            }
            let before = identity(&source);
            let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
            command.arg(&source).arg(&target).arg("--no-hooks");
            if changed_old {
                command.arg("--backup");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"selected SOURCE bytes");
            assert_eq!(identity(&source), before);
            if changed_old {
                assert_eq!(
                    std::fs::read(target.with_file_name("bar~")).unwrap(),
                    b"OLD"
                );
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn selected_followed_file_copies_and_backs_up_without_changing_link_or_target() {
    for changed_old in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let target = outside.join("data");
        let selected = source.join("selected");
        let output_path = destination.join("output");
        std::fs::write(&target, b"operator-selected target bytes").unwrap();
        std::os::unix::fs::symlink(&target, &selected).unwrap();
        if changed_old {
            std::fs::write(&output_path, b"OLD").unwrap();
        }
        let before = [identity(&selected), identity(&target)];
        let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
        command
            .arg(&selected)
            .arg(&output_path)
            .args(["--copy-links", "--no-hooks"]);
        if changed_old {
            command.arg("--backup");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(&output_path).unwrap(),
            b"operator-selected target bytes"
        );
        assert_eq!([identity(&selected), identity(&target)], before);
        assert_eq!(std::fs::read_link(&selected).unwrap(), target);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"operator-selected target bytes"
        );
        if changed_old {
            assert_eq!(std::fs::read(destination.join("output~")).unwrap(), b"OLD");
        }
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            1 + usize::from(changed_old)
        );
    }
}

#[test]
fn followed_source_removal_refuses_before_publication_backup_or_parent_creation() {
    for scenario in ["absent", "backup", "pending", "checksum", "preview"] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        let target = fixture.path().join("target");
        let selected = source.join("selected");
        let output = if scenario == "pending" {
            destination.join("missing/output")
        } else {
            destination.join("output")
        };
        std::fs::write(&target, b"selected DATA").unwrap();
        std::os::unix::fs::symlink(&target, &selected).unwrap();
        if matches!(scenario, "backup" | "checksum") {
            std::fs::write(
                &output,
                if scenario == "checksum" {
                    b"selected DATA".as_slice()
                } else {
                    b"OLD".as_slice()
                },
            )
            .unwrap();
        }
        let originals = [identity(&selected), identity(&target)];
        let old = output
            .exists()
            .then(|| (identity(&output), std::fs::read(&output).unwrap()));
        let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
        command.arg(&selected).arg(&output).args([
            "--copy-links",
            "--remove-source-files",
            "--no-hooks",
        ]);
        match scenario {
            "backup" => {
                command.arg("--backup");
            }
            "checksum" => {
                command.arg("--checksum");
            }
            "preview" => {
                command.arg("--dry-run");
            }
            _ => {}
        }
        let result = command.output().unwrap();
        assert!(!result.status.success(), "{scenario}: unexpected success");
        assert_eq!([identity(&selected), identity(&target)], originals);
        assert_eq!(std::fs::read_link(&selected).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"selected DATA");
        if let Some((identity_before, bytes)) = old {
            assert_eq!(identity(&output), identity_before, "{scenario}");
            assert_eq!(std::fs::read(&output).unwrap(), bytes);
            assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
        } else {
            assert_eq!(
                std::fs::read_dir(&destination).unwrap().count(),
                0,
                "{scenario}"
            );
        }
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("--remove-source-files does not support followed SOURCE names"),
            "{scenario}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn source_removal_follow_policy_keeps_regular_and_skipped_bindings_distinct() {
    for followed in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        let selected = source.join("selected");
        let target = fixture.path().join("target");
        let output = destination.join("output");
        if followed {
            std::fs::write(&target, b"SOURCE DATA").unwrap();
            std::os::unix::fs::symlink(&target, &selected).unwrap();
            std::fs::write(&output, b"kept OLD").unwrap();
        } else {
            std::fs::write(&selected, b"SOURCE DATA").unwrap();
        }
        let original = identity(&selected);
        let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
        command.arg(&selected).arg(&output).args([
            "--copy-links",
            "--remove-source-files",
            "--no-hooks",
        ]);
        if followed {
            command.arg("--ignore-existing");
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if followed {
            assert_eq!(identity(&selected), original);
            assert_eq!(std::fs::read_link(&selected).unwrap(), target);
            assert_eq!(std::fs::read(&target).unwrap(), b"SOURCE DATA");
            assert_eq!(std::fs::read(&output).unwrap(), b"kept OLD");
        } else {
            assert!(!selected.exists());
            assert_eq!(std::fs::read(&output).unwrap(), b"SOURCE DATA");
        }
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn forward_file_follow_copies_and_backs_up_without_adopting_physical_aliases() {
    // Darwin BENEATH is not rename-safe; API presence is not parent support.
    for (target, bridge, unsupported_parent) in [
        ("dir/bridge", "deeper/data", false),
        ("./dir/./bridge", "./deeper/./data", false),
        (".//dir///.//bridge", ".//deeper///.//data", false),
        ("dir/../dir/bridge", "../dir/deeper/data", true),
        ("{ROOT}/dir/bridge", "{ROOT}/dir/deeper/data", false),
        ("dir/bridge", "{ROOT}/.//dir///deeper/data", false),
        ("{ROOT}/.//dir///bridge", "deeper/data", false),
        ("{FD_ROOT}/dir/bridge", "{FD_ROOT}/dir/deeper/data", false),
    ] {
        for target_alias in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let (source, destination) = directories(fixture.path());
            for root in [&source, &destination] {
                std::fs::create_dir_all(root.join("dir/deeper")).unwrap();
            }
            std::fs::write(source.join("dir/deeper/data"), b"SOURCE DATA").unwrap();
            let relative_operand = target.contains("{FD_ROOT}");
            let native_address = source.canonicalize().unwrap();
            let target = target
                .replace("{ROOT}", source.to_str().unwrap())
                .replace("{FD_ROOT}", native_address.to_str().unwrap());
            let bridge = bridge
                .replace("{ROOT}", source.to_str().unwrap())
                .replace("{FD_ROOT}", native_address.to_str().unwrap());
            std::os::unix::fs::symlink(&target, source.join("entry")).unwrap();
            std::os::unix::fs::symlink(&bridge, source.join("dir/bridge")).unwrap();
            let names = ["entry", "dir/bridge", "dir/deeper/data"];
            for name in names {
                if target_alias && name == "entry" {
                    std::fs::hard_link(source.join("dir/deeper/data"), destination.join(name))
                        .unwrap();
                } else {
                    std::fs::write(destination.join(name), format!("OLD {name}")).unwrap();
                }
            }
            let source_originals = names.map(|name| identity(&source.join(name)));
            let old = names.map(|name| identity(&destination.join(name)));
            let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
            command
                .current_dir(fixture.path())
                .arg(if relative_operand {
                    "source/".to_string()
                } else {
                    format!("{}/", source.display())
                });
            let output = command
                .arg(&destination)
                .args(["--copy-links", "--no-hooks", "--ignore-times", "--backup"])
                .output()
                .unwrap();
            if target_alias || unsupported_parent {
                if unsupported_parent {
                    assert!(
                        !output.status.success(),
                        "unqualified native parent traversal must refuse"
                    );
                } else {
                    refused(output);
                }
                assert_eq!(names.map(|name| identity(&destination.join(name))), old);
                for name in names {
                    assert!(!destination.join(format!("{name}~")).exists());
                }
            } else {
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                for name in names {
                    assert_eq!(
                        std::fs::read(destination.join(name)).unwrap(),
                        b"SOURCE DATA"
                    );
                    assert!(!std::fs::symlink_metadata(destination.join(name))
                        .unwrap()
                        .file_type()
                        .is_symlink());
                    assert_eq!(
                        std::fs::read(destination.join(format!("{name}~"))).unwrap(),
                        format!("OLD {name}").as_bytes()
                    );
                }
            }
            assert_eq!(
                names.map(|name| identity(&source.join(name))),
                source_originals
            );
            assert_eq!(
                std::fs::read_link(source.join("entry"))
                    .unwrap()
                    .as_os_str(),
                Path::new(&target).as_os_str()
            );
            assert_eq!(
                std::fs::read_link(source.join("dir/bridge"))
                    .unwrap()
                    .as_os_str(),
                Path::new(&bridge).as_os_str()
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn sibling_file_follow_copies_and_backs_up_with_physical_source_protection() {
    for (target_alias, chain) in [(false, false), (true, false), (false, true), (true, true)] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        let target = source.join("target");
        let link = source.join("link");
        std::fs::write(&target, b"SOURCE DATA").unwrap();
        xattr::set(&target, "user.sy-sibling", b"original").unwrap();
        let mut source_names = vec!["link", "target"];
        let first_target = if chain {
            std::os::unix::fs::symlink("target", source.join("middle")).unwrap();
            source_names.push("middle");
            "middle"
        } else {
            "target"
        };
        std::os::unix::fs::symlink(first_target, &link).unwrap();
        std::fs::write(destination.join("target"), b"OLD target").unwrap();
        if target_alias {
            std::fs::hard_link(&target, destination.join("link")).unwrap();
        } else {
            std::fs::write(destination.join("link"), b"OLD link").unwrap();
        }
        let originals: Vec<_> = source_names
            .iter()
            .map(|name| identity(&source.join(name)))
            .collect();
        let old = [
            identity(&destination.join("link")),
            identity(&destination.join("target")),
        ];
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(format!("{}/", source.display()))
            .arg(&destination)
            .args([
                "--copy-links",
                "--no-hooks",
                "--ignore-times",
                "--backup",
                "-X",
            ])
            .output()
            .unwrap();
        if target_alias {
            refused(output);
            assert_eq!(
                [
                    identity(&destination.join("link")),
                    identity(&destination.join("target"))
                ],
                old
            );
            assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 2);
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            for name in &source_names {
                assert_eq!(
                    std::fs::read(destination.join(name)).unwrap(),
                    b"SOURCE DATA"
                );
                assert_eq!(
                    xattr::get(destination.join(name), "user.sy-sibling").unwrap(),
                    Some(b"original".to_vec())
                );
                assert!(!std::fs::symlink_metadata(destination.join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink());
            }
            assert_eq!(
                std::fs::read(destination.join("link~")).unwrap(),
                b"OLD link"
            );
            assert_eq!(
                std::fs::read(destination.join("target~")).unwrap(),
                b"OLD target"
            );
            assert_eq!(
                std::fs::read_dir(&destination).unwrap().count(),
                source_names.len() + 2
            );
        }
        assert_eq!(
            source_names
                .iter()
                .map(|name| identity(&source.join(name)))
                .collect::<Vec<_>>(),
            originals
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new(first_target));
        assert_eq!(std::fs::read(&target).unwrap(), b"SOURCE DATA");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn selected_followed_file_preserves_requested_target_metadata() {
    use std::os::macos::fs::MetadataExt as MacMetadataExt;
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    let target = fixture.path().join("target");
    let selected = source.join("selected");
    let output_path = destination.join("output");
    std::fs::write(&target, b"selected DATA").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    filetime::set_file_mtime(
        &target,
        filetime::FileTime::from_unix_time(123, 456_000_000),
    )
    .unwrap();
    xattr::set(&target, "user.sy-follow-preservation", b"target metadata").unwrap();
    #[cfg(feature = "acl")]
    let acl = {
        let mut acl = exacl::getfacl(&target, None).unwrap();
        // SAFETY: getuid has no arguments or preconditions. Use the numeric
        // fixture owner, not an account name requiring NSS remapping.
        let uid = unsafe { libc::getuid() };
        acl.push(exacl::AclEntry::allow_user(
            &uid.to_string(),
            exacl::Perm::READ,
            exacl::Flag::empty(),
        ));
        exacl::setfacl(&[&target], &acl, None).unwrap();
        exacl::to_string(&exacl::getfacl(&target, None).unwrap()).unwrap()
    };
    let path = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: path is a live NUL-terminated fixture path; UF_HIDDEN neither
    // prevents fixture cleanup nor changes access/inheritance policy.
    assert_eq!(unsafe { libc::chflags(path.as_ptr(), libc::UF_HIDDEN) }, 0);
    std::os::unix::fs::symlink(&target, &selected).unwrap();
    let before = [identity(&selected), identity(&target)];
    let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
    command.arg(&selected).arg(&output_path).args([
        "--copy-links",
        "--no-hooks",
        "-p",
        "-t",
        "-X",
        "-F",
    ]);
    #[cfg(feature = "acl")]
    command.arg("-A");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&output_path).unwrap(), b"selected DATA");
    let metadata = std::fs::metadata(&output_path).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(
        (metadata.mtime(), metadata.mtime_nsec()),
        (123, 456_000_000)
    );
    assert_eq!(metadata.st_flags() & libc::UF_HIDDEN, libc::UF_HIDDEN);
    assert_eq!(
        xattr::get(&output_path, "user.sy-follow-preservation").unwrap(),
        Some(b"target metadata".to_vec())
    );
    #[cfg(feature = "acl")]
    assert_eq!(
        exacl::to_string(&exacl::getfacl(&output_path, None).unwrap()).unwrap(),
        acl
    );
    assert_eq!([identity(&selected), identity(&target)], before);
    assert_eq!(std::fs::read_link(&selected).unwrap(), target);
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
}

#[cfg(target_os = "macos")]
#[test]
fn followed_selected_target_alias_in_publication_or_backup_refuses_before_effects() {
    for backup_slot in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let target = outside.join("data");
        let selected = source.join("selected");
        let output_path = destination.join("output");
        std::fs::write(&target, b"protected target").unwrap();
        std::os::unix::fs::symlink(&target, &selected).unwrap();
        let protected = if backup_slot {
            std::fs::write(&output_path, b"OLD").unwrap();
            destination.join("output~")
        } else {
            output_path.clone()
        };
        std::fs::hard_link(&target, &protected).unwrap();
        let before = [
            identity(&selected),
            identity(&target),
            identity(&output_path),
        ];
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(&selected)
            .arg(&output_path)
            .args(["--copy-links", "--no-hooks", "--backup", "--ignore-times"])
            .output()
            .unwrap();
        refused(output);
        assert_eq!(
            [
                identity(&selected),
                identity(&target),
                identity(&output_path)
            ],
            before
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"protected target");
        assert_eq!(std::fs::read(&protected).unwrap(), b"protected target");
        assert_eq!(std::fs::read_link(&selected).unwrap(), target);
        if backup_slot {
            assert_eq!(std::fs::read(&output_path).unwrap(), b"OLD");
        }
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            1 + usize::from(backup_slot)
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn ordinary_selected_copy_into_existing_directory_is_admitted() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("foo");
    let destination = fixture.path().join("destination");
    std::fs::write(&source, b"SOURCE").unwrap();
    std::fs::create_dir(&destination).unwrap();
    let before = identity(&source);
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(&source)
        .arg(&destination)
        .arg("--no-hooks")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(destination.join("foo")).unwrap(), b"SOURCE");
    assert_eq!(identity(&source), before);
}

#[cfg(target_os = "macos")]
#[test]
fn selected_backup_cannot_replace_source_or_waive_old_reads() {
    for protected_slot in [true, false] {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture
            .path()
            .join(if protected_slot { "bar~" } else { "foo" });
        let target = fixture.path().join("bar");
        std::fs::write(&source, b"SOURCE bytes").unwrap();
        std::fs::write(&target, b"OLD").unwrap();
        if !protected_slot {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o0)).unwrap();
            assert_eq!(
                std::fs::File::open(&target).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        let before = (identity(&source), identity(&target));
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(&source)
            .arg(&target)
            .args(["--no-hooks", "--backup"])
            .output()
            .unwrap();
        if protected_slot {
            refused(output);
        } else {
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .to_lowercase()
                    .contains("permission denied"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!fixture.path().join("bar~").exists());
        }
        assert_eq!((identity(&source), identity(&target)), before);
        assert_eq!(std::fs::read(&source).unwrap(), b"SOURCE bytes");
    }
}

#[test]
fn selected_source_alias_remains_refused() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("foo");
    let target = fixture.path().join("bar");
    std::fs::write(&source, b"SOURCE").unwrap();
    std::fs::hard_link(&source, &target).unwrap();
    let before = identity(&source);
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(&source)
        .arg(&target)
        .args(["--no-hooks", "--ignore-times"])
        .output()
        .unwrap();
    refused(output);
    assert_eq!(identity(&source), before);
}

#[cfg(target_os = "macos")]
#[test]
fn selected_mode000_source_is_not_a_byte_read_waiver() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("foo");
    let target = fixture.path().join("bar");
    std::fs::write(&source, b"SOURCE").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o0)).unwrap();
    assert_eq!(
        std::fs::File::open(&source).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(&source)
        .arg(&target)
        .arg("--no-hooks")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!target.exists());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_lowercase()
            .contains("permission denied"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn matching_mode000_hardlink_group_remains_readless() {
    // SAFETY: geteuid has no arguments or side effects.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "requires an unprivileged runner"
    );
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    for root in [&source, &destination] {
        std::fs::write(root.join("a"), b"same").unwrap();
        std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
        std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o0)).unwrap();
        assert_eq!(
            std::fs::File::open(root.join("a")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
    let before = (
        identity(&source.join("a")),
        identity(&destination.join("a")),
    );
    let output = sync(&source, &destination, &["-H", "--size-only"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        (
            identity(&source.join("a")),
            identity(&destination.join("a"))
        ),
        before
    );
}

#[cfg(target_os = "macos")]
#[test]
fn enumerated_tree_can_retire_an_independent_regular_leaf() {
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    std::fs::write(source.join("input"), b"SOURCE").unwrap();
    std::fs::write(destination.join("stale"), b"STALE").unwrap();
    let before = identity(&source.join("input"));
    let output = sync(&source, &destination, &["--delete", "--force-delete"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(destination.join("input")).unwrap(), b"SOURCE");
    assert!(!destination.join("stale").exists());
    assert_eq!(identity(&source.join("input")), before);
}

#[cfg(target_os = "macos")]
#[test]
fn fully_enumerated_tree_copies_through_existing_native_parents() {
    for changed_old in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        for root in [&source, &destination] {
            std::fs::create_dir_all(root.join("a/b")).unwrap();
        }
        std::fs::create_dir(source.join("new_empty")).unwrap();
        std::fs::write(source.join("top"), b"top SOURCE").unwrap();
        std::fs::write(source.join("a/b/file"), b"nested SOURCE").unwrap();
        // Omitted leaves still belong to the exact original protection census.
        std::fs::write(source.join("omitted"), b"omitted SOURCE").unwrap();
        let before = [
            identity(&source),
            identity(&source.join("top")),
            identity(&source.join("a/b/file")),
            identity(&source.join("omitted")),
        ];
        if changed_old {
            std::fs::write(destination.join("top"), b"OLD").unwrap();
            std::fs::write(destination.join("a/b/file"), b"OLD").unwrap();
        }
        let output = sync(&source, &destination, &["--exclude", "omitted"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(destination.join("top")).unwrap(),
            b"top SOURCE"
        );
        assert_eq!(
            std::fs::read(destination.join("a/b/file")).unwrap(),
            b"nested SOURCE"
        );
        assert!(destination.join("new_empty").is_dir());
        assert!(!destination.join("omitted").exists());
        assert_eq!(
            [
                identity(&source),
                identity(&source.join("top")),
                identity(&source.join("a/b/file")),
                identity(&source.join("omitted"))
            ],
            before
        );
    }
}

#[test]
fn quickequal_hardlink_topology_repair_is_a_publication_effect() {
    let fixture = tempfile::tempdir().unwrap();
    let (source, destination) = directories(fixture.path());
    std::fs::write(source.join("a"), b"NEW!").unwrap();
    std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
    for name in ["a", "b"] {
        std::fs::write(destination.join(name), b"OLD!").unwrap();
    }
    let before = (
        identity(&destination.join("a")),
        identity(&destination.join("b")),
    );
    let source_before = identity(&source.join("a"));
    let output = sync(&source, &destination, &["-H", "--size-only"]);
    if cfg!(target_os = "macos") {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            identity(&destination.join("a")),
            identity(&destination.join("b"))
        );
        assert_eq!(std::fs::metadata(destination.join("a")).unwrap().nlink(), 2);
        assert_eq!(std::fs::read(destination.join("a")).unwrap(), b"NEW!");
        assert_eq!(std::fs::read(destination.join("b")).unwrap(), b"NEW!");
        assert_eq!(identity(&source.join("a")), source_before);
    } else {
        refused(output);
        assert_eq!(
            (
                identity(&destination.join("a")),
                identity(&destination.join("b"))
            ),
            before
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn hardlink_publication_backs_up_every_potential_member_using_original_roots() {
    // Topology repair uses SOURCE; coalesced M/E isolation retains OLD. Both
    // publish every member and owe backups even for quick-equal plan entries.
    for retained_external in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        std::fs::write(source.join("a"), b"NEW!").unwrap();
        std::fs::set_permissions(source.join("a"), std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
        std::fs::write(destination.join("a"), b"OLD!").unwrap();
        std::fs::set_permissions(
            destination.join("a"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        if retained_external {
            std::fs::hard_link(destination.join("a"), destination.join("b")).unwrap();
        } else {
            std::fs::write(destination.join("b"), b"OLD!").unwrap();
        }
        let outside = fixture.path().join("outside");
        std::fs::hard_link(destination.join("a"), &outside).unwrap();
        let backup = if retained_external {
            let path = fixture.path().join("backup");
            std::fs::create_dir(&path).unwrap();
            path
        } else {
            destination.clone()
        };
        for name in ["a~", "b~"] {
            std::fs::write(backup.join(name), b"previous backup").unwrap();
        }
        let before = identity(&source.join("a"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
        command
            .arg(format!("{}/", source.display()))
            .arg(&destination)
            .args(["--no-hooks", "-H", "-p", "--size-only", "--backup"]);
        if retained_external {
            command.arg("--backup-dir").arg(&backup);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(identity(&source.join("a")), before);
        assert_eq!(
            identity(&destination.join("a")),
            identity(&destination.join("b"))
        );
        assert_eq!(std::fs::metadata(destination.join("a")).unwrap().nlink(), 2);
        for name in ["a", "b"] {
            assert_eq!(
                std::fs::read(destination.join(name)).unwrap(),
                if retained_external { b"OLD!" } else { b"NEW!" }
            );
            assert_eq!(
                std::fs::read(backup.join(format!("{name}~"))).unwrap(),
                b"OLD!"
            );
        }
        assert_eq!(std::fs::read(&outside).unwrap(), b"OLD!");
        assert_eq!(std::fs::metadata(outside).unwrap().mode() & 0o7777, 0o600);
    }
}

#[test]
fn hardlink_all_member_old_and_backup_effects_precede_any_publication() {
    for backup_slot in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let (source, destination) = directories(fixture.path());
        std::fs::write(source.join("a"), b"NEW!").unwrap();
        std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
        std::fs::write(source.join("protected"), b"KEEP").unwrap();
        std::fs::write(destination.join("a"), b"OLD!").unwrap();
        std::fs::write(destination.join("b"), b"OLD!").unwrap();
        let protected_target = destination.join(if backup_slot { "b~" } else { "b" });
        if !backup_slot {
            std::fs::remove_file(&protected_target).unwrap();
        }
        std::fs::hard_link(source.join("protected"), &protected_target).unwrap();
        let before = [
            identity(&source.join("a")),
            identity(&source.join("protected")),
            identity(&destination.join("a")),
            identity(&destination.join("b")),
            identity(&protected_target),
        ];
        let mut flags = vec!["-H", "--size-only", "--exclude", "protected"];
        if backup_slot {
            flags.push("--backup");
        }
        let output = sync(&source, &destination, &flags);
        // On the qualified lane, the later member/slot is the refusal, not
        // a representative-only declaration or late native partial failure.
        if cfg!(target_os = "macos") {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(if backup_slot {
                    "b~"
                } else {
                    "b"
                }),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        refused(output);
        assert_eq!(
            [
                identity(&source.join("a")),
                identity(&source.join("protected")),
                identity(&destination.join("a")),
                identity(&destination.join("b")),
                identity(&protected_target)
            ],
            before
        );
        assert_eq!(std::fs::read(destination.join("a")).unwrap(), b"OLD!");
        assert_eq!(std::fs::read(&protected_target).unwrap(), b"KEEP");
        assert!(!destination.join("a~").exists());
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            if backup_slot { 3 } else { 2 }
        );
    }
}
