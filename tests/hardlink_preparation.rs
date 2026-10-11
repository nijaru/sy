#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

fn cli(source: &Path, destination: &Path, home: &Path, remove: bool) {
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "ulimit -n 64; exec \"$@\"",
            "hardlink-preparation-test",
        ])
        .arg(env!("CARGO_BIN_EXE_sy"))
        .args(["-rHpt", "-j1", "--no-hooks"])
        .arg(format!("{}/", source.display()))
        .arg(destination)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("NO_COLOR", "1");
    if remove {
        command.arg("--remove-source-files");
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
}

#[test]
fn real_cli_interleaved_groups_stay_bounded_and_restore_directory_metadata() {
    const GROUPS: usize = 96;
    for remove in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        let home = fixture.path().join("home");
        for path in [&source, &destination, &home] {
            std::fs::create_dir(path).unwrap();
        }
        for parent in ["a", "z", "z/inner"] {
            std::fs::create_dir(source.join(parent)).unwrap();
        }
        for group in 0..GROUPS {
            let name = format!("{group:04}");
            std::fs::write(source.join("a").join(&name), name.as_bytes()).unwrap();
            std::fs::hard_link(
                source.join("a").join(&name),
                source.join("z/inner").join(&name),
            )
            .unwrap();
        }
        for parent in ["a", "z", "z/inner"] {
            std::fs::set_permissions(source.join(parent), std::fs::Permissions::from_mode(0o750))
                .unwrap();
            filetime::set_file_mtime(
                source.join(parent),
                filetime::FileTime::from_unix_time(1_600_000_000, 123),
            )
            .unwrap();
        }
        let source_observation = std::fs::metadata(source.join("a/0000")).unwrap();
        cli(&source, &destination, &home, remove);
        for group in 0..GROUPS {
            let name = format!("{group:04}");
            let first = std::fs::metadata(destination.join("a").join(&name)).unwrap();
            let second = std::fs::metadata(destination.join("z/inner").join(&name)).unwrap();
            assert_eq!((first.dev(), first.ino()), (second.dev(), second.ino()));
            assert_eq!(first.nlink(), 2);
            assert_eq!(
                std::fs::read(destination.join("a").join(&name)).unwrap(),
                name.as_bytes()
            );
            for parent in ["a", "z/inner"] {
                assert_eq!(source.join(parent).join(&name).exists(), !remove);
            }
        }
        for parent in ["a", "z", "z/inner"] {
            let metadata = std::fs::metadata(destination.join(parent)).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o750);
            let modified = filetime::FileTime::from_last_modification_time(&metadata);
            assert_eq!(
                (modified.unix_seconds(), modified.nanoseconds()),
                (1_600_000_000, 123)
            );
        }
        if !remove {
            let destination_observation = std::fs::metadata(destination.join("a/0000")).unwrap();
            cli(&source, &destination, &home, false);
            for (path, original) in [
                (source.join("a/0000"), source_observation),
                (destination.join("a/0000"), destination_observation),
            ] {
                let current = std::fs::metadata(path).unwrap();
                assert_eq!(
                    (
                        current.dev(),
                        current.ino(),
                        current.nlink(),
                        current.ctime(),
                        current.ctime_nsec()
                    ),
                    (
                        original.dev(),
                        original.ino(),
                        original.nlink(),
                        original.ctime(),
                        original.ctime_nsec()
                    ),
                    "matching group replay must not mutate either inode",
                );
            }
        }
    }
}
