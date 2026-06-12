//! Metadata tests — permissions, symlinks, hardlinks, xattrs, archive mode.
//!
//! Consolidates: archive_mode_test.rs, hardlink_test.rs, symlink_overwrite_test.rs.

use std::fs;
use std::os::unix::fs::MetadataExt;
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

#[test]
fn test_archive_includes_git_directory() {
    let (source, dest) = setup_test_dir();

    // Create .git directory with content
    fs::create_dir_all(source.path().join(".git/objects")).unwrap();
    fs::write(source.path().join(".git/config"), "[core]").unwrap();
    fs::write(source.path().join(".git/objects/abc"), "data").unwrap();

    // Sync with --archive
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--archive",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join(".git/config").exists());
    assert!(dest.path().join(".git/objects/abc").exists());
}

#[test]
fn test_archive_includes_hidden_files() {
    let (source, dest) = setup_test_dir();

    // Create hidden files
    fs::write(source.path().join(".hidden"), "hidden content").unwrap();
    fs::write(source.path().join(".config"), "config content").unwrap();

    // Sync with --archive
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--archive",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join(".hidden").exists());
    assert!(dest.path().join(".config").exists());
}

#[test]
fn test_archive_syncs_gitignored_files() {
    let (source, dest) = setup_test_dir();

    // Create .gitignore and ignored file
    fs::write(source.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join("debug.log"), "log content").unwrap();

    // Sync with --archive (should include gitignored files)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--archive",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(dest.path().join("debug.log").exists());
}

#[test]
fn test_archive_is_complete_backup() {
    let (source, dest) = setup_test_dir();

    // Create various file types
    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join(".hidden"), "hidden").unwrap();
    fs::create_dir_all(source.path().join("subdir")).unwrap();
    fs::write(source.path().join("subdir/nested.txt"), "nested").unwrap();

    // Sync with --archive
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--archive",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(dest.path().join(".hidden").exists());
    assert!(dest.path().join("subdir/nested.txt").exists());
}

#[test]
fn test_exclude_vcs_excludes_git() {
    let (source, dest) = setup_test_dir();

    // Create .git directory with content
    fs::create_dir_all(source.path().join(".git/objects")).unwrap();
    fs::write(source.path().join(".git/config"), "[core]").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Sync with --exclude-vcs
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join(".git").exists());
}

#[test]
fn test_hardlink_creation() {
    let (source, dest) = setup_test_dir();

    // Create hard linked files
    fs::write(source.path().join("original.txt"), "content").unwrap();
    fs::hard_link(
        source.path().join("original.txt"),
        source.path().join("link.txt"),
    )
    .unwrap();

    // Sync with --preserve-hardlinks
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("original.txt").exists());
    assert!(dest.path().join("link.txt").exists());
}

#[test]
fn test_hardlink_set() {
    let (source, dest) = setup_test_dir();

    // Create multiple hard linked files
    fs::write(source.path().join("file1.txt"), "content").unwrap();
    fs::hard_link(
        source.path().join("file1.txt"),
        source.path().join("file2.txt"),
    )
    .unwrap();
    fs::hard_link(
        source.path().join("file1.txt"),
        source.path().join("file3.txt"),
    )
    .unwrap();

    // Sync with --preserve-hardlinks
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // All three files should be hard linked
    let meta1 = fs::metadata(dest.path().join("file1.txt")).unwrap();
    let meta2 = fs::metadata(dest.path().join("file2.txt")).unwrap();
    let meta3 = fs::metadata(dest.path().join("file3.txt")).unwrap();
    assert_eq!(meta1.ino(), meta2.ino());
    assert_eq!(meta2.ino(), meta3.ino());
}

#[test]
fn test_hardlinks_across_directories() {
    let (source, dest) = setup_test_dir();

    // Create hard links across directories
    fs::create_dir_all(source.path().join("dir1")).unwrap();
    fs::create_dir_all(source.path().join("dir2")).unwrap();
    fs::write(source.path().join("dir1/file.txt"), "content").unwrap();
    fs::hard_link(
        source.path().join("dir1/file.txt"),
        source.path().join("dir2/link.txt"),
    )
    .unwrap();

    // Sync with --preserve-hardlinks
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("dir1/file.txt").exists());
    assert!(dest.path().join("dir2/link.txt").exists());
}

#[test]
fn test_sync_overwrites_existing_symlink() {
    let (source, dest) = setup_test_dir();

    // Create symlink in source
    std::os::unix::fs::symlink("target.txt", source.path().join("link.txt")).unwrap();

    // Create different symlink in dest
    std::os::unix::fs::symlink("other.txt", dest.path().join("link.txt")).unwrap();

    // Sync should overwrite
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let link_target = fs::read_link(dest.path().join("link.txt")).unwrap();
    assert_eq!(link_target, std::path::PathBuf::from("target.txt"));
}

#[test]
fn test_sync_symlink_to_empty_dest() {
    let (source, dest) = setup_test_dir();

    // Create symlink in source
    std::os::unix::fs::symlink("target.txt", source.path().join("link.txt")).unwrap();

    // Sync to empty dest
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    // Use symlink_metadata since Path::exists() follows symlinks (returns false for dangling symlinks)
    assert!(fs::symlink_metadata(dest.path().join("link.txt")).is_ok());
    let link_target = fs::read_link(dest.path().join("link.txt")).unwrap();
    assert_eq!(link_target, std::path::PathBuf::from("target.txt"));
}

#[test]
fn test_sync_skips_identical_symlink() {
    let (source, dest) = setup_test_dir();

    // Create identical symlinks
    std::os::unix::fs::symlink("target.txt", source.path().join("link.txt")).unwrap();
    std::os::unix::fs::symlink("target.txt", dest.path().join("link.txt")).unwrap();

    // Sync should skip (identical)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
}
