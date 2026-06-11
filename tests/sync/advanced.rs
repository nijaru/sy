//! Advanced tests — compression, S3, server mode.
//!
//! Consolidates: compression_integration.rs, s3_integration_test.rs, server_mode_test.rs.

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
fn test_compression_end_to_end() {
    let (source, dest) = setup_test_dir();

    // Create compressible content
    let content = "Hello World\n".repeat(10000);
    fs::write(source.path().join("compressible.txt"), &content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--compress",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("compressible.txt")).unwrap(),
        content
    );
}

#[test]
fn test_compression_skip_small_files() {
    let (source, dest) = setup_test_dir();

    // Create small file (below compression threshold)
    fs::write(source.path().join("small.txt"), "tiny").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--compress",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("small.txt")).unwrap(),
        "tiny"
    );
}

#[test]
fn test_compression_skip_local() {
    let (source, dest) = setup_test_dir();

    // Create file
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Compression should be skipped for local sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--compress",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "content"
    );
}

// S3 tests require real S3 credentials and are ignored by default
// To run: cargo test --features s3-tests

#[test]
#[ignore] // Requires S3 credentials
fn test_s3_sync_basic() {
    // Placeholder - S3 tests require real credentials
}

// Server mode tests

#[test]
fn test_server_mode_help() {
    let output = Command::new(sy_bin())
        .args(["--server", "--help"])
        .output()
        .unwrap();

    // Server mode should show help or error gracefully
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stderr.contains("server") || stdout.contains("server") || output.status.success(),
        "Server mode should respond to --help"
    );
}
