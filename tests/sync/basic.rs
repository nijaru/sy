//! Basic sync tests — fundamental operations.
//!
//! Covers: basic sync, dry run, delete mode, nested dirs, quiet mode, error handling.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir(_name: &str) -> (TempDir, TempDir) {
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
fn test_basic_sync() {
    let (source, dest) = setup_test_dir("basic");

    fs::write(source.path().join("file1.txt"), "content1").unwrap();
    fs::write(source.path().join("file2.txt"), "content2").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file1.txt")).unwrap(), "content1");
    assert_eq!(fs::read_to_string(dest.path().join("file2.txt")).unwrap(), "content2");
}

#[test]
fn test_dry_run() {
    let (source, dest) = setup_test_dir("dry_run");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("file.txt").exists());
}

#[test]
fn test_delete_mode() {
    let (source, dest) = setup_test_dir("delete");

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(dest.path().join("keep.txt"), "keep").unwrap();
    fs::write(dest.path().join("delete.txt"), "delete").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--delete",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("delete.txt").exists());
}

#[test]
fn test_nested_directories() {
    let (source, dest) = setup_test_dir("nested");

    fs::create_dir_all(source.path().join("a/b/c")).unwrap();
    fs::write(source.path().join("a/b/c/file.txt"), "nested").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("a/b/c/file.txt")).unwrap(), "nested");
}

#[test]
fn test_update_existing_files() {
    let (source, dest) = setup_test_dir("update");

    fs::write(source.path().join("file.txt"), "original").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "original");

    // Wait to ensure mtime changes
    thread::sleep(Duration::from_secs(2));

    fs::write(source.path().join("file.txt"), "updated").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "updated");
}

#[test]
fn test_skip_unchanged_files() {
    let (source, dest) = setup_test_dir("skip");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("skipped") || stdout.contains("Files skipped:     1"));
}

#[test]
fn test_quiet_mode() {
    let (source, dest) = setup_test_dir("quiet");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn test_error_source_not_exists() {
    let dest = TempDir::new().unwrap();

    let output = Command::new(sy_bin())
        .args([
            "/nonexistent/path",
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
}

#[test]
fn test_single_file_sync() {
    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("file.txt");
    fs::write(&file_path, "test content for single file").unwrap();

    let dest_file = temp.path().join("dest.txt");

    let output = Command::new(sy_bin())
        .args([file_path.to_str().unwrap(), dest_file.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dest_file.exists());
    assert_eq!(fs::read_to_string(&dest_file).unwrap(), "test content for single file");
}

#[test]
fn test_git_directory_excluded() {
    let (source, dest) = setup_test_dir("git_excluded");

    fs::write(source.path().join(".git/config"), "git config").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

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
fn test_update_shows_correct_stats() {
    let (source, dest) = setup_test_dir("stats");

    fs::write(source.path().join("file1.txt"), "initial content v1").unwrap();
    fs::write(source.path().join("file2.txt"), "initial content v2").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files created:     2"));

    // Wait to ensure mtime changes
    thread::sleep(Duration::from_secs(2));

    fs::write(source.path().join("file1.txt"), "updated content v1").unwrap();
    fs::write(source.path().join("file2.txt"), "updated content v2").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     2") || stdout.contains("Files updated:     1"));
}
