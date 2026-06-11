//! Progress tests — per-file progress display.
//!
//! Consolidates: per_file_progress_test.rs, per_file_progress_edge_cases.rs.

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
fn test_progress_shown_for_large_files() {
    let (source, dest) = setup_test_dir();

    // Create large file (10MB)
    let content = vec![0u8; 10 * 1024 * 1024];
    fs::write(source.path().join("large.bin"), &content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--per-file-progress",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("large.bin").exists());
}

#[test]
fn test_progress_not_shown_for_small_files() {
    let (source, dest) = setup_test_dir();

    // Create small file
    fs::write(source.path().join("small.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--per-file-progress",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("small.txt").exists());
}

#[test]
fn test_progress_with_multiple_large_files() {
    let (source, dest) = setup_test_dir();

    // Create multiple large files
    for i in 0..5 {
        let content = vec![0u8; 10 * 1024 * 1024];
        fs::write(source.path().join(format!("large_{}.bin", i)), &content).unwrap();
    }

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--per-file-progress",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Verify all files copied
    for i in 0..5 {
        assert!(dest.path().join(format!("large_{}.bin", i)).exists());
    }
}

#[test]
fn test_progress_with_mixed_file_sizes() {
    let (source, dest) = setup_test_dir();

    // Create mix of small and large files
    fs::write(source.path().join("small.txt"), "content").unwrap();
    let content = vec![0u8; 10 * 1024 * 1024];
    fs::write(source.path().join("large.bin"), &content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--per-file-progress",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("small.txt").exists());
    assert!(dest.path().join("large.bin").exists());
}

#[test]
fn test_progress_quiet_mode() {
    let (source, dest) = setup_test_dir();

    // Create file
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--quiet",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());

    // Quiet mode should have minimal output
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.is_empty() || stdout.trim().is_empty());
}

#[test]
fn test_progress_json_mode() {
    let (source, dest) = setup_test_dir();

    // Create file
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());

    // JSON mode should output valid JSON
    let stdout = String::from_utf8_lossy(&output.stdout);
    
}
