//! Comparison mode tests — ignore-times, size-only, checksum, update.
//!
//! Consolidates: comparison_modes_test.rs.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir() -> (TempDir, TempDir) {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    // Initialize git repo for consistent scanning
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
fn test_ignore_times_forces_comparison() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same content, --ignore-times should still "update"
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--ignore-times"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_ignore_times_with_identical_files() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // --ignore-times on identical files should still "update"
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--ignore-times"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_size_only_skips_mtime_check() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same size, --size-only should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--size-only"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_size_only_updates_different_size() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Change size
    fs::write(source.path().join("file.txt"), "new longer content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--size-only"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_default_uses_mtime_and_size() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same content, no changes - should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_ignore_existing() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "new").unwrap();
    fs::write(dest.path().join("file.txt"), "old").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--ignore-existing"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "old");
}

#[test]
fn test_update_skips_newer_dest() {
    let (source, dest) = setup_test_dir();

    // Create file in dest with newer mtime
    fs::write(dest.path().join("file.txt"), "dest content").unwrap();
    // Set source to older time
    fs::write(source.path().join("file.txt"), "source content").unwrap();

    // Make dest newer by touching it
    let dest_file = dest.path().join("file.txt");
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(
        &dest_file,
        filetime::FileTime::from_system_time(now + std::time::Duration::from_secs(10)),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--update"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "dest content");
}

#[test]
fn test_update_long_flag() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "source").unwrap();
    fs::write(dest.path().join("file.txt"), "dest").unwrap();

    // Make dest newer
    let dest_file = dest.path().join("file.txt");
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(
        &dest_file,
        filetime::FileTime::from_system_time(now + std::time::Duration::from_secs(10)),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--update"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:"));
}

#[test]
fn test_checksum_compares_content() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same content, --checksum should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--checksum"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_checksum_skips_identical_content() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "same content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Force same mtime by touching dest to be same time as source
    let src_meta = fs::metadata(source.path().join("file.txt")).unwrap();
    let dest_file = dest.path().join("file.txt");
    filetime::set_file_mtime(
        &dest_file,
        filetime::FileTime::from_system_time(src_meta.modified().unwrap()),
    )
    .unwrap();

    // With --checksum, same content should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--checksum"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_comparison_flags_mutually_exclusive() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // --size-only and --checksum together should fail
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--size-only", "--checksum"]))
        .output()
        .unwrap();

    assert!(!output.status.success());
}
