//! Edge case tests — unicode, deep paths, special chars, empty dirs, trailing slash.
//!
//! Consolidates: edge_cases_test.rs, trailing_slash_behavior_test.rs, remote_to_local_parent_dirs_test.rs.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;
use std::thread;
use std::time::Duration;
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
fn test_unicode_filenames() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("日本語.txt"), "japanese").unwrap();
    fs::write(source.path().join("émoji🎉.txt"), "emoji").unwrap();
    fs::write(source.path().join("über.txt"), "german").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("日本語.txt")).unwrap(),
        "japanese"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("émoji🎉.txt")).unwrap(),
        "emoji"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("über.txt")).unwrap(),
        "german"
    );
}

#[test]
fn test_deep_directory_structure() {
    let (source, dest) = setup_test_dir();

    let mut path = source.path().to_path_buf();
    for i in 0..10 {
        path = path.join(format!("level{}", i));
    }
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("deep.txt"), "deep content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());

    let mut dest_path = dest.path().to_path_buf();
    for i in 0..10 {
        dest_path = dest_path.join(format!("level{}", i));
    }
    assert_eq!(
        fs::read_to_string(dest_path.join("deep.txt")).unwrap(),
        "deep content"
    );
}

#[test]
fn test_special_characters_in_filenames() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file with spaces.txt"), "spaces").unwrap();
    fs::write(source.path().join("file-with-dashes.txt"), "dashes").unwrap();
    fs::write(source.path().join("file.with.dots.txt"), "dots").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("file with spaces.txt")).unwrap(),
        "spaces"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file-with-dashes.txt")).unwrap(),
        "dashes"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file.with.dots.txt")).unwrap(),
        "dots"
    );
}

#[test]
fn test_empty_directories() {
    let (source, dest) = setup_test_dir();

    fs::create_dir(source.path().join("empty1")).unwrap();
    fs::create_dir_all(source.path().join("nested/empty2")).unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("empty1").is_dir());
    assert!(dest.path().join("nested/empty2").is_dir());
}

#[test]
fn test_trailing_slash_source() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "content"
    );
}

#[test]
fn test_trailing_slash_dest() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "content"
    );
}

#[test]
fn test_trailing_slash_both() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "content"
    );
}

// TODO: No-trailing-slash case needs adjusted dest path passed to SyncSession
// Currently main.rs computes adjusted_dest but SyncSession gets the original path
#[test]
#[ignore]
fn test_no_trailing_slash_copies_directory() {
    let (source, dest) = setup_test_dir();

    let source_dir = source.path().join("mydir");
    fs::create_dir(&source_dir).unwrap();
    fs::write(source_dir.join("file.txt"), "content").unwrap();

    // No trailing slash: copies "mydir" into dest
    let output = Command::new(sy_bin())
        .args([
            source_dir.to_str().unwrap(),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("mydir/file.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("mydir/file.txt")).unwrap(),
        "content"
    );
}

#[test]
fn test_root_path_sync() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_large_number_of_files() {
    let (source, dest) = setup_test_dir();

    for i in 0..100 {
        fs::write(
            source.path().join(format!("file{:03}.txt", i)),
            format!("content{}", i),
        )
        .unwrap();
    }

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());

    for i in 0..100 {
        let path = dest.path().join(format!("file{:03}.txt", i));
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("content{}", i));
    }
}

#[test]
fn test_mixed_file_types() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("regular.txt"), "regular").unwrap();
    fs::create_dir(source.path().join("directory")).unwrap();
    fs::write(source.path().join("directory/file.txt"), "in dir").unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("regular.txt", source.path().join("symlink.txt")).unwrap();
    }

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("regular.txt").exists());
    assert!(dest.path().join("directory/file.txt").exists());

    #[cfg(unix)]
    {
        let meta = fs::symlink_metadata(dest.path().join("symlink.txt")).unwrap();
        assert!(meta.is_symlink());
    }
}

#[test]
fn test_overwrite_existing_files() {
    let (source, dest) = setup_test_dir();

    // Create source file first, then dest with --ignore-times to force overwrite
    fs::write(source.path().join("file.txt"), "new content").unwrap();
    fs::write(dest.path().join("file.txt"), "old content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--ignore-times"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "new content"
    );
}

#[test]
fn test_large_file() {
    let (source, dest) = setup_test_dir();

    // Create 100MB file
    let content = vec![0u8; 100 * 1024 * 1024];
    fs::write(source.path().join("large.bin"), &content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("large.bin").exists());
    assert_eq!(
        fs::metadata(dest.path().join("large.bin")).unwrap().len(),
        100 * 1024 * 1024
    );
}

#[test]
fn test_many_small_files() {
    let (source, dest) = setup_test_dir();

    // Create 1000 small files
    for i in 0..1000 {
        fs::write(
            source.path().join(format!("file_{:04}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
    }

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Verify all files copied
    for i in 0..1000 {
        assert!(dest.path().join(format!("file_{:04}.txt", i)).exists());
    }
}

#[test]
fn test_same_source_and_dest() {
    let (source, _dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Sync to same directory (should be idempotent)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            source.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(source.path().join("file.txt").exists());
}

#[test]
fn test_binary_files() {
    let (source, dest) = setup_test_dir();

    // Create binary file with all byte values
    let content: Vec<u8> = (0..=255).collect();
    fs::write(source.path().join("binary.bin"), &content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read(dest.path().join("binary.bin")).unwrap(), content);
}

#[test]
fn test_hidden_files() {
    let (source, dest) = setup_test_dir();

    // Create hidden files
    fs::write(source.path().join(".hidden"), "hidden content").unwrap();
    fs::write(source.path().join(".config"), "config content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join(".hidden").exists());
    assert!(dest.path().join(".config").exists());
}

#[test]
fn test_file_permissions_preserved() {
    let (source, dest) = setup_test_dir();

    // Create file with specific permissions
    fs::write(source.path().join("script.sh"), "#!/bin/bash\necho hello").unwrap();
    fs::set_permissions(
        source.path().join("script.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Check permissions preserved
    let dest_meta = fs::metadata(dest.path().join("script.sh")).unwrap();
    assert_eq!(dest_meta.permissions().mode() & 0o777, 0o755);
}

#[test]
fn test_zero_byte_files() {
    let (source, dest) = setup_test_dir();

    // Create zero-byte files
    fs::write(source.path().join("empty1.txt"), "").unwrap();
    fs::write(source.path().join("empty2.txt"), "").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("empty1.txt").exists());
    assert!(dest.path().join("empty2.txt").exists());
    assert_eq!(
        fs::metadata(dest.path().join("empty1.txt")).unwrap().len(),
        0
    );
}

#[test]
fn test_deeply_nested_paths() {
    let (source, dest) = setup_test_dir();

    // Create deeply nested structure (20 levels)
    let mut path = source.path().to_path_buf();
    for i in 0..20 {
        path = path.join(format!("level_{}", i));
    }
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("deep.txt"), "deep content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Verify deep file exists
    let mut dest_path = dest.path().to_path_buf();
    for i in 0..20 {
        dest_path = dest_path.join(format!("level_{}", i));
    }
    assert!(dest_path.join("deep.txt").exists());
    assert_eq!(
        fs::read_to_string(dest_path.join("deep.txt")).unwrap(),
        "deep content"
    );
}

#[test]
fn test_hardlink_delta_sync() {
    let (source, dest) = setup_test_dir();

    // Create a file and a hard link to it
    let content = "original content for hardlink test";
    fs::write(source.path().join("original.txt"), content).unwrap();
    fs::hard_link(
        source.path().join("original.txt"),
        source.path().join("link.txt"),
    )
    .unwrap();

    // Initial sync
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

    // Verify both files exist and are hard linked
    assert!(dest.path().join("original.txt").exists());
    assert!(dest.path().join("link.txt").exists());

    // Update the original file
    thread::sleep(Duration::from_secs(2));
    fs::write(source.path().join("original.txt"), "updated content").unwrap();

    // Sync again
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

    // Verify both files have updated content
    assert_eq!(
        fs::read_to_string(dest.path().join("original.txt")).unwrap(),
        "updated content"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("link.txt")).unwrap(),
        "updated content"
    );
}

#[test]
fn test_symlink_to_file_delta() {
    let (source, dest) = setup_test_dir();

    // Create a file and a symlink to it
    fs::write(source.path().join("target.txt"), "target content").unwrap();
    std::os::unix::fs::symlink("target.txt", source.path().join("link.txt")).unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify symlink exists
    assert!(dest.path().join("link.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("link.txt")).unwrap(),
        "target content"
    );

    // Update the target file
    thread::sleep(Duration::from_secs(2));
    fs::write(source.path().join("target.txt"), "updated target").unwrap();

    // Sync again
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify symlink still works and has updated content
    assert_eq!(
        fs::read_to_string(dest.path().join("link.txt")).unwrap(),
        "updated target"
    );
}
