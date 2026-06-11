//! Metadata tests — permissions, symlinks, hardlinks, xattrs, archive mode.
//!
//! Consolidates: archive_mode_test.rs, hardlink_test.rs, symlink_overwrite_test.rs.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn sy() -> Command {
    Command::new(sy_bin())
}

#[test]
fn test_archive_mode_preserves_permissions() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    let file = source.path().join("file.txt");
    fs::write(&file, "content").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();

        let output = sy()
            .arg("--archive")
            .arg(source.path())
            .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("target.txt"), "target").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("target.txt", source.path().join("link.txt")).unwrap();

        let output = sy()
            .arg(source.path())
            .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("old_target.txt", dest.path().join("link.txt")).unwrap();
        std::os::unix::fs::symlink("new_target.txt", source.path().join("link.txt")).unwrap();

        let output = sy()
            .arg(source.path())
            .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(dest.path().join("file.txt"), "old").unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("target.txt", source.path().join("file.txt")).unwrap();

        let output = sy()
            .arg(source.path())
            .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("target.txt"), "target").unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("target.txt", source.path().join("link1.txt")).unwrap();
        std::os::unix::fs::symlink("target.txt", source.path().join("link2.txt")).unwrap();

        let output = sy()
            .arg(source.path())
            .arg(dest.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(fs::read_link(dest.path().join("link1.txt")).unwrap().to_str().unwrap(), "target.txt");
        assert_eq!(fs::read_link(dest.path().join("link2.txt")).unwrap().to_str().unwrap(), "target.txt");
    }
}

#[test]
fn test_hardlink_preservation() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("original.txt"), "content").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::hard_link(source.path().join("original.txt"), source.path().join("hardlink.txt")).unwrap();

        let output = sy()
            .arg("--hard-links")
            .arg(source.path())
            .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("original.txt"), "content").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::hard_link(source.path().join("original.txt"), source.path().join("hardlink.txt")).unwrap();

        let output = sy()
            .arg(source.path())
            .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    #[cfg(unix)]
    {
        if xattr::set(source.path().join("file.txt"), "user.test", b"value").is_ok() {
            let output = sy()
                .arg("--xattrs")
                .arg(source.path())
                .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    #[cfg(unix)]
    {
        if xattr::set(source.path().join("file.txt"), "user.test", b"value").is_ok() {
            let output = sy()
                .arg(source.path())
                .arg(dest.path())
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
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    let dir = source.path().join("subdir");
    fs::create_dir(&dir).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        let output = sy()
            .arg("--archive")
            .arg(source.path())
            .arg(dest.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        let dest_dir = dest.path().join("subdir");
        let perms = fs::metadata(&dest_dir).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o700);
    }
}
