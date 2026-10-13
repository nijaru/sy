//! Backup declarations exercised through the actual local CLI.
#![cfg(unix)]
use std::path::Path;
use std::process::{Command, Output};

fn sync(source: &Path, destination: &Path, backup: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(format!("{}/", source.display()))
        .arg(destination)
        .arg("--backup")
        .arg("--backup-dir")
        .arg(backup)
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn external_original_and_pending_backup_roots_publish_old_bytes() {
    for pending in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        let backup = fixture.path().join("backup/missing");
        for root in [&source, &destination] {
            std::fs::create_dir(root).unwrap();
        }
        if !pending {
            std::fs::create_dir_all(&backup).unwrap();
        }
        std::fs::write(source.join("file"), b"new source bytes").unwrap();
        std::fs::write(destination.join("file"), b"old").unwrap();
        let output = sync(&source, &destination, &backup, &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(backup.join("file~")).unwrap(), b"old");
        assert_eq!(
            std::fs::read(destination.join("file")).unwrap(),
            b"new source bytes"
        );
    }
}

#[test]
fn unchanged_hardlink_topology_repair_keeps_backups_and_shared_retirement() {
    use std::os::unix::fs::MetadataExt;
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    let backup = fixture.path().join("backup");
    for root in [&source, &destination, &backup] {
        std::fs::create_dir(root).unwrap();
    }
    std::fs::write(source.join("a"), b"source-data").unwrap();
    std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
    for path in [destination.join("a"), destination.join("b")] {
        std::fs::write(path, b"old-bytes!!").unwrap();
    }
    // Publishing a's backup retires one alias of OLD b. The later b backup
    // must retain the same run's retirement continuation across both roots.
    std::fs::hard_link(destination.join("b"), backup.join("a~")).unwrap();
    let modified = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    for root in [&source, &destination] {
        for name in ["a", "b"] {
            filetime::set_file_mtime(root.join(name), modified).unwrap();
        }
    }
    let output = sync(&source, &destination, &backup, &["-H", "--size-only"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in ["a", "b"] {
        assert_eq!(std::fs::read(source.join(name)).unwrap(), b"source-data");
        assert_eq!(
            std::fs::read(destination.join(name)).unwrap(),
            b"source-data"
        );
        assert_eq!(
            std::fs::read(backup.join(format!("{name}~"))).unwrap(),
            b"old-bytes!!"
        );
    }
    assert_eq!(
        std::fs::metadata(destination.join("a")).unwrap().ino(),
        std::fs::metadata(destination.join("b")).unwrap().ino()
    );
}

#[test]
fn unrelated_backup_capture_failure_is_conditional_on_backup_work() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    for root in [&source, &destination] {
        std::fs::create_dir(root).unwrap();
    }
    let bad_parent = fixture.path().join("not-directory");
    std::fs::write(&bad_parent, b"untouched").unwrap();
    let backup = bad_parent.join("backup");
    std::fs::write(source.join("file"), b"equal").unwrap();
    std::fs::copy(source.join("file"), destination.join("file")).unwrap();
    // Size comparison makes this intentionally effect-free without relying on
    // timestamp rounding, permissions, or elevated test-runner privileges.
    let output = sync(&source, &destination, &backup, &["--size-only"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(source.join("file"), b"changed bytes").unwrap();
    let output = sync(&source, &destination, &backup, &["--size-only"]);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(destination.join("file")).unwrap(), b"equal");
    assert_eq!(std::fs::read(bad_parent).unwrap(), b"untouched");
}

#[test]
fn selected_local_backup_rejects_unscanned_native_aliases_before_effects() {
    for prefix in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        for root in [&source, &destination] {
            std::fs::create_dir(root).unwrap();
        }
        std::os::unix::fs::symlink("opaque", source.join("original")).unwrap();
        std::fs::write(destination.join("foo"), b"old").unwrap();
        std::fs::write(destination.join("unrelated"), b"untouched").unwrap();
        let kept = if prefix {
            std::fs::create_dir(destination.join("BACKUPS")).unwrap();
            destination.join("BACKUPS/keep")
        } else {
            destination.join("FOO~")
        };
        std::fs::write(&kept, b"keep").unwrap();
        let alias = if prefix {
            destination.join("backups")
        } else {
            destination.join("foo~")
        };
        if !alias.exists() {
            eprintln!("observed prerequisite unavailable: native case aliases do not resolve");
            return;
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_sy"));
        command
            .arg(source.join("original"))
            .arg(destination.join("foo"))
            .arg("--backup");
        if prefix {
            command.arg("--backup-dir=backups");
        }
        let output = command.output().unwrap();
        assert!(
            !output.status.success(),
            "unscanned backup alias was admitted"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("native spelling"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(destination.join("foo")).unwrap(), b"old");
        assert_eq!(std::fs::read(&kept).unwrap(), b"keep");
        assert_eq!(
            std::fs::read(destination.join("unrelated")).unwrap(),
            b"untouched"
        );
        assert_eq!(
            std::fs::read_link(source.join("original")).unwrap(),
            Path::new("opaque")
        );
    }
}

#[test]
fn selected_local_literal_backup_slot_is_overwritable_across_runs() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    std::fs::write(&source, b"new long bytes").unwrap();
    std::fs::write(&destination, b"old").unwrap();
    let backup = fixture.path().join("destination~");
    std::fs::write(&backup, b"previous backup").unwrap();
    for old in [b"old".as_slice(), b"new long bytes"] {
        if old != b"old" {
            std::fs::write(&source, b"next").unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(&source)
            .arg(&destination)
            .args(["--backup", "--size-only"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&backup).unwrap(), old);
    }
}
