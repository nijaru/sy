#![cfg(unix)]

#[test]
fn sparse_group_uses_logical_length_not_written_extent_bytes() {
    use std::io::{Seek, SeekFrom, Write};
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&destination).unwrap();
    let size = 8 * 1024 * 1024;
    let mut file = std::fs::File::create(source.join("a")).unwrap();
    file.write_all(b"head").unwrap();
    file.seek(SeekFrom::Start(size - 4)).unwrap();
    file.write_all(b"tail").unwrap();
    drop(file);
    std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
    let before = observe(&source.join("a"));
    let expected = std::fs::read(source.join("a")).unwrap();
    run(&source, &destination, fixture.path(), &[]);
    for name in ["a", "b"] {
        assert_eq!(
            std::fs::metadata(destination.join(name)).unwrap().len(),
            size
        );
        assert_eq!(std::fs::read(destination.join(name)).unwrap(), expected);
    }
    assert_eq!(
        observe(&destination.join("a")).1,
        observe(&destination.join("b")).1
    );
    assert_eq!(observe(&source.join("a")), before);
}

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

fn run(source: &Path, destination: &Path, home: &Path, flags: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["-rH", "--no-hooks", "-j4"])
        .args(flags)
        .arg(format!("{}/", source.display()))
        .arg(destination)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn observe(path: &Path) -> (u64, u64, u64, i64, i64, u32, i64, i64) {
    let metadata = std::fs::metadata(path).unwrap();
    (
        metadata.dev(),
        metadata.ino(),
        metadata.nlink(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.mode() & 0o7777,
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}

fn file(path: &Path, bytes: &[u8], mode: u32, seconds: i64) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(seconds, 0)).unwrap();
}

#[test]
fn representative_metadata_and_source_bytes_repair_quick_equal_topology() {
    // Separate equal-sized OLD files formerly bypassed -H entirely. A missing
    // first member formerly conflicted with the other member's retained bytes.
    for (missing_first, preserve_mode, mode) in [
        (false, false, 0o600),
        (false, true, 0o640),
        (true, false, 0o640),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        for directory in [&source, &destination] {
            std::fs::create_dir(directory).unwrap();
        }
        file(&source.join("a"), b"NEW!", 0o640, 1_600_000_000);
        std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
        if !missing_first {
            file(&destination.join("a"), b"OLD!", 0o600, 1_600_000_000);
        }
        file(&destination.join("b"), b"OLD!", 0o644, 1_600_000_000);
        let before = observe(&source.join("a"));
        run(
            &source,
            &destination,
            fixture.path(),
            if preserve_mode { &["-p"] } else { &[] },
        );
        let a = observe(&destination.join("a"));
        let b = observe(&destination.join("b"));
        assert_eq!((a.0, a.1), (b.0, b.1));
        assert_eq!((a.2, a.5), (2, mode));
        for name in ["a", "b"] {
            assert_eq!(std::fs::read(destination.join(name)).unwrap(), b"NEW!");
        }
        assert_eq!(observe(&source.join("a")), before);
    }
}

#[test]
fn coalesced_metadata_isolation_retains_old_bytes_and_protects_outside_alias() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    for directory in [&source, &destination] {
        std::fs::create_dir(directory).unwrap();
    }
    file(&source.join("a"), b"NEW!", 0o640, 1_600_000_000);
    std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
    file(&destination.join("a"), b"OLD!", 0o600, 1_600_000_000);
    std::fs::hard_link(destination.join("a"), destination.join("b")).unwrap();
    let outside = fixture.path().join("outside");
    std::fs::hard_link(destination.join("a"), &outside).unwrap();
    let source_before = observe(&source.join("a"));
    let outside_before = observe(&outside);
    run(
        &source,
        &destination,
        fixture.path(),
        &["-p", "--remove-source-files"],
    );
    let a = observe(&destination.join("a"));
    let b = observe(&destination.join("b"));
    assert_eq!((a.0, a.1), (b.0, b.1));
    assert_eq!((a.2, a.5), (2, 0o640));
    assert_ne!(a.1, outside_before.1);
    for name in ["a", "b"] {
        assert_eq!(std::fs::read(destination.join(name)).unwrap(), b"OLD!");
    }
    assert_eq!(std::fs::read(&outside).unwrap(), b"OLD!");
    let outside_after = observe(&outside);
    assert_eq!(
        (
            outside_after.0,
            outside_after.1,
            outside_after.5,
            outside_after.6,
            outside_after.7
        ),
        (
            outside_before.0,
            outside_before.1,
            outside_before.5,
            outside_before.6,
            outside_before.7
        )
    );
    assert_eq!(outside_after.2, 1);
    assert_eq!(observe(&source.join("a")), source_before);
    assert!(
        source.join("b").exists(),
        "unequal retained bytes never authorize source removal"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn source_xattr_override_does_not_read_discarded_old_values() {
    for source_xattrs in [true, false] {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let destination = fixture.path().join("destination");
        for directory in [&source, &destination] {
            std::fs::create_dir(directory).unwrap();
        }
        file(&source.join("a"), b"NEW!", 0o640, 1_600_000_000);
        std::fs::hard_link(source.join("a"), source.join("b")).unwrap();
        xattr::set(source.join("a"), "user.sy-source", b"requested").unwrap();
        file(&destination.join("a"), b"OLD!", 0o600, 1_600_000_000);
        std::fs::hard_link(destination.join("a"), destination.join("b")).unwrap();
        let outside = fixture.path().join("outside");
        std::fs::hard_link(destination.join("a"), &outside).unwrap();
        let large = vec![b'x'; sy::protocol::MAX_XATTR_TOTAL_BYTES + 1];
        xattr::set(&outside, "user.sy-old-large", &large).unwrap();
        let source_before = observe(&source.join("a"));
        let old_before = observe(&outside);
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .args(["-rH", "--no-hooks"])
            .arg(if source_xattrs { "-X" } else { "-p" })
            .arg(format!("{}/", source.display()))
            .arg(&destination)
            .env("HOME", fixture.path())
            .env("XDG_CONFIG_HOME", fixture.path())
            .output()
            .unwrap();
        assert_eq!(observe(&source.join("a")), source_before);
        assert_eq!(std::fs::read(source.join("b")).unwrap(), b"NEW!");
        assert_eq!(std::fs::read(&outside).unwrap(), b"OLD!");
        assert_eq!(
            xattr::get(&outside, "user.sy-old-large").unwrap(),
            Some(large)
        );
        if source_xattrs {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let a = observe(&destination.join("a"));
            assert_eq!(a, observe(&destination.join("b")));
            assert_ne!(a.1, old_before.1);
            assert_eq!((a.2, a.5, a.6, a.7), (2, 0o600, 1_600_000_000, 0));
            assert_eq!(
                xattr::get(destination.join("a"), "user.sy-source").unwrap(),
                Some(b"requested".to_vec())
            );
            assert_eq!(
                xattr::get(destination.join("a"), "user.sy-old-large").unwrap(),
                None
            );
            assert_eq!(std::fs::read(destination.join("a")).unwrap(), b"OLD!");
            let outside_after = observe(&outside);
            assert_eq!(outside_after.2, 1);
            assert_eq!(
                (
                    outside_after.0,
                    outside_after.1,
                    outside_after.5,
                    outside_after.6,
                    outside_after.7
                ),
                (
                    old_before.0,
                    old_before.1,
                    old_before.5,
                    old_before.6,
                    old_before.7
                )
            );
        } else {
            assert!(
                !output.status.success(),
                "unrequested OLD xattrs still require bounded native reads"
            );
            assert_eq!(observe(&outside), old_before);
            assert_eq!(observe(&destination.join("a")), old_before);
            assert_eq!(observe(&destination.join("b")), old_before);
        }
    }
}

#[test]
fn matching_unreadable_group_needs_no_byte_read_or_metadata_setter() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    for (root, bytes) in [(&source, b"NEW!"), (&destination, b"OLD!")] {
        std::fs::create_dir(root).unwrap();
        file(&root.join("a"), bytes, 0o600, 1_600_000_000);
        std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
        std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let source_before = observe(&source.join("a"));
    let destination_before = observe(&destination.join("a"));
    run(&source, &destination, fixture.path(), &["-pt"]);
    assert_eq!(observe(&source.join("a")), source_before);
    assert_eq!(observe(&source.join("b")), source_before);
    assert_eq!(observe(&destination.join("a")), destination_before);
    assert_eq!(observe(&destination.join("b")), destination_before);
}

#[test]
fn matching_coalesced_group_is_effect_free_even_with_unequal_quick_bytes() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let destination = fixture.path().join("destination");
    for directory in [&source, &destination] {
        std::fs::create_dir(directory).unwrap();
    }
    for (root, bytes) in [(&source, b"NEW!"), (&destination, b"OLD!")] {
        file(&root.join("a"), bytes, 0o600, 1_600_000_000);
        std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
    }
    let source_before = observe(&source.join("a"));
    let destination_before = observe(&destination.join("a"));
    run(
        &source,
        &destination,
        fixture.path(),
        &["-pt", "--remove-source-files"],
    );
    assert_eq!(observe(&source.join("a")), source_before);
    assert_eq!(observe(&destination.join("a")), destination_before);
    assert_eq!(std::fs::read(destination.join("a")).unwrap(), b"OLD!");
    assert!(source.join("a").exists() && source.join("b").exists());
}
