//! Metadata tests — permissions, symlinks, hardlinks, xattrs, archive mode.
//!
//! Consolidates: archive_mode_test.rs, hardlink_test.rs, symlink_overwrite_test.rs.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir() -> (TempDir, TempDir) {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(source.path())
        .output()
        .unwrap();
    (source, dest)
}

fn sync_args<'a>(source: &'a TempDir, dest: &'a TempDir, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        format!("{}/", source.path().display()),
        dest.path().to_str().unwrap().to_string(),
        "--exclude-vcs".to_string(),
    ];
    for e in extra {
        args.push(e.to_string());
    }
    args
}

#[test]
fn test_archive_mode_preserves_permissions() {
    let (source, dest) = setup_test_dir();

    let file = source.path().join("file.txt");
    fs::write(&file, "content").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &["--archive"]))
            .output()
            .unwrap();

        assert!(output.status.success());

        let dest_file = dest.path().join("file.txt");
        let perms = fs::metadata(&dest_file).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o755);
    }
}

#[test]
fn test_symlink_preserve() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("target.txt"), "target").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("target.txt", source.path().join("link.txt")).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &[]))
            .output()
            .unwrap();

        assert!(output.status.success());

        let dest_link = dest.path().join("link.txt");
        let meta = fs::symlink_metadata(&dest_link).unwrap();
        assert!(meta.is_symlink());
        assert_eq!(fs::read_link(&dest_link).unwrap().to_str().unwrap(), "target.txt");
    }
}

#[test]
fn test_symlink_overwrite() {
    let (source, dest) = setup_test_dir();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("old_target.txt", dest.path().join("link.txt")).unwrap();
        std::os::unix::fs::symlink("new_target.txt", source.path().join("link.txt")).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &[]))
            .output()
            .unwrap();

        assert!(output.status.success());

        let dest_link = dest.path().join("link.txt");
        let meta = fs::symlink_metadata(&dest_link).unwrap();
        assert!(meta.is_symlink());
        assert_eq!(fs::read_link(&dest_link).unwrap().to_str().unwrap(), "new_target.txt");
    }
}

#[test]
fn test_symlink_over_regular_file() {
    let (source, dest) = setup_test_dir();

    fs::write(dest.path().join("file.txt"), "old").unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("target.txt", source.path().join("file.txt")).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &["--ignore-times"]))
            .output()
            .unwrap();

        assert!(output.status.success());

        let dest_file = dest.path().join("file.txt");
        let meta = fs::symlink_metadata(&dest_file).unwrap();
        assert!(meta.is_symlink());
    }
}

#[test]
fn test_multiple_symlinks_same_target() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("target.txt"), "target").unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("target.txt", source.path().join("link1.txt")).unwrap();
        std::os::unix::fs::symlink("target.txt", source.path().join("link2.txt")).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &[]))
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(fs::read_link(dest.path().join("link1.txt")).unwrap().to_str().unwrap(), "target.txt");
        assert_eq!(fs::read_link(dest.path().join("link2.txt")).unwrap().to_str().unwrap(), "target.txt");
    }
}

#[test]
fn test_hardlink_preservation() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("original.txt"), "content").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::hard_link(source.path().join("original.txt"), source.path().join("hardlink.txt")).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &["--preserve-hardlinks"]))
            .output()
            .unwrap();

        assert!(output.status.success());
        assert!(dest.path().join("original.txt").exists());
        assert!(dest.path().join("hardlink.txt").exists());
        assert_eq!(
            fs::read_to_string(dest.path().join("original.txt")).unwrap(),
            fs::read_to_string(dest.path().join("hardlink.txt")).unwrap()
        );

        let orig_meta = fs::metadata(dest.path().join("original.txt")).unwrap();
        let link_meta = fs::metadata(dest.path().join("hardlink.txt")).unwrap();
        assert_eq!(orig_meta.ino(), link_meta.ino());
    }
}

#[test]
fn test_hardlink_not_preserved_without_flag() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("original.txt"), "content").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::hard_link(source.path().join("original.txt"), source.path().join("hardlink.txt")).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &[]))
            .output()
            .unwrap();

        assert!(output.status.success());
        assert!(dest.path().join("original.txt").exists());
        assert!(dest.path().join("hardlink.txt").exists());

        let orig_meta = fs::metadata(dest.path().join("original.txt")).unwrap();
        let link_meta = fs::metadata(dest.path().join("hardlink.txt")).unwrap();
        assert_ne!(orig_meta.ino(), link_meta.ino());
    }
}

#[test]
fn test_xattr_preservation() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    #[cfg(unix)]
    {
        if xattr::set(source.path().join("file.txt"), "user.test", b"value").is_ok() {
            let output = Command::new(sy_bin())
                .args(sync_args(&source, &dest, &["--preserve-xattrs"]))
                .output()
                .unwrap();

            assert!(output.status.success());
            let dest_xattr = xattr::get(dest.path().join("file.txt"), "user.test").unwrap();
            assert_eq!(dest_xattr, Some(b"value".to_vec()));
        }
    }
}

#[test]
fn test_xattr_not_preserved_without_flag() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    #[cfg(unix)]
    {
        if xattr::set(source.path().join("file.txt"), "user.test", b"value").is_ok() {
            let output = Command::new(sy_bin())
                .args(sync_args(&source, &dest, &[]))
                .output()
                .unwrap();

            assert!(output.status.success());
            let dest_xattr = xattr::get(dest.path().join("file.txt"), "user.test").unwrap();
            assert_eq!(dest_xattr, None);
        }
    }
}

#[test]
fn test_directory_permissions_preserved() {
    let (source, dest) = setup_test_dir();

    let dir = source.path().join("subdir");
    fs::create_dir(&dir).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        let output = Command::new(sy_bin())
            .args(sync_args(&source, &dest, &["--archive"]))
            .output()
            .unwrap();

        assert!(output.status.success());
        let dest_dir = dest.path().join("subdir");
        let perms = fs::metadata(&dest_dir).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o700);
    }
}
