//! Performance tests — large files, massive directories.
//!
//! Consolidates: large_file_test.rs, massive_directory_test.rs.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir() -> (TempDir, TempDir) {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create git repo in source for .gitignore support
    Command::new("git")
        .args(["init"])
        .current_dir(source.path())
        .output()
        .unwrap();

    (source, dest)
}

#[test]
fn test_sync_100mb_file() {
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
fn test_sync_500mb_file() {
    let (source, dest) = setup_test_dir();

    // Create 500MB file
    let content = vec![0u8; 500 * 1024 * 1024];
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
}

#[test]
fn test_sync_1gb_file() {
    let (source, dest) = setup_test_dir();

    // Create 1GB file
    let content = vec![0u8; 1024 * 1024 * 1024];
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
}

#[test]
fn test_idempotent_sync_100mb_file() {
    let (source, dest) = setup_test_dir();

    // Create 100MB file
    let content = vec![0u8; 100 * 1024 * 1024];
    fs::write(source.path().join("large.bin"), &content).unwrap();

    // First sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Second sync should be idempotent
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

#[test]
fn test_sync_many_small_files() {
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
fn test_sync_deep_directory_structure() {
    let (source, dest) = setup_test_dir();

    // Create deep directory structure (10 levels, 5 dirs each)
    let mut path = source.path().to_path_buf();
    for i in 0..10 {
        path = path.join(format!("dir_{}", i));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("file.txt"), format!("content {}", i)).unwrap();
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

    // Verify deep file exists
    let mut dest_path = dest.path().to_path_buf();
    for i in 0..10 {
        dest_path = dest_path.join(format!("dir_{}", i));
    }
    assert!(dest_path.join("file.txt").exists());
}
