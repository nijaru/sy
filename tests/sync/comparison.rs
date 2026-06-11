//! Comparison mode tests — ignore-times, size-only, checksum, update.
//!
//! Consolidates: comparison_modes_test.rs.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn sy() -> Command {
    Command::new(sy_bin())
}

#[test]
fn test_ignore_times_forces_comparison() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    let output = sy()
        .arg("--ignore-times")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_ignore_times_with_identical_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    let output = sy()
        .arg("--ignore-times")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_size_only_skips_mtime_check() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    thread::sleep(Duration::from_millis(1100));

    let output = sy()
        .arg("--size-only")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_size_only_updates_different_size() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    fs::write(source.path().join("file.txt"), "updated content here").unwrap();

    let output = sy()
        .arg("--size-only")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_checksum_compares_content() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    fs::write(source.path().join("file.txt"), "contEnt").unwrap();

    let output = sy()
        .arg("--checksum")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_checksum_skips_identical_content() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    let output = sy()
        .arg("--checksum")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_comparison_flags_mutually_exclusive() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy()
        .arg("--size-only")
        .arg("--checksum")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
}

#[test]
fn test_default_uses_mtime_and_size() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_update_skips_newer_dest() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "old").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    thread::sleep(Duration::from_millis(1100));
    fs::write(source.path().join("file.txt"), "new").unwrap();

    let output = sy()
        .arg("--update")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_update_long_flag() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();
    assert!(output.status.success());

    let output = sy()
        .arg("--update")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_ignore_existing() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "new").unwrap();
    fs::write(dest.path().join("file.txt"), "old").unwrap();

    let output = sy()
        .arg("--ignore-existing")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "old");
}
