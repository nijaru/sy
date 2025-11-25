//! Integration tests for size filtering (--min-size, --max-size)
//!
//! These tests verify that size filters work correctly.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_git_repo(dir: &TempDir) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .unwrap();
}

// =============================================================================
// --min-size Tests
// =============================================================================

#[test]
fn test_min_size_filters_small_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create files of different sizes
    fs::write(source.path().join("tiny.txt"), "x").unwrap(); // 1 byte
    fs::write(source.path().join("small.txt"), "x".repeat(500)).unwrap(); // 500 bytes
    fs::write(source.path().join("medium.txt"), "x".repeat(2000)).unwrap(); // 2KB

    // Only sync files >= 1KB
    let output = Command::new(sy_bin())
        .args([
            "--min-size",
            "1KB",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --min-size failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Only medium.txt should be synced
    assert!(
        !dest.path().join("tiny.txt").exists(),
        "tiny.txt should be filtered out by --min-size 1KB"
    );
    assert!(
        !dest.path().join("small.txt").exists(),
        "small.txt should be filtered out by --min-size 1KB"
    );
    assert!(
        dest.path().join("medium.txt").exists(),
        "medium.txt should be synced (>= 1KB)"
    );
}

#[test]
fn test_min_size_exact_boundary() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create file exactly at boundary (1024 bytes = 1KB)
    fs::write(source.path().join("exact.txt"), "x".repeat(1024)).unwrap();
    fs::write(source.path().join("under.txt"), "x".repeat(1023)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--min-size",
            "1KB",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Exact boundary should be included
    assert!(
        dest.path().join("exact.txt").exists(),
        "exact.txt (exactly 1KB) should be synced"
    );
    assert!(
        !dest.path().join("under.txt").exists(),
        "under.txt (1023 bytes) should be filtered out"
    );
}

// =============================================================================
// --max-size Tests
// =============================================================================

#[test]
fn test_max_size_filters_large_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create files of different sizes
    fs::write(source.path().join("small.txt"), "x".repeat(500)).unwrap(); // 500 bytes
    fs::write(source.path().join("medium.txt"), "x".repeat(2000)).unwrap(); // ~2KB
    fs::write(source.path().join("large.txt"), "x".repeat(5000)).unwrap(); // ~5KB

    // Only sync files <= 1KB
    let output = Command::new(sy_bin())
        .args([
            "--max-size",
            "1KB",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --max-size failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Only small.txt should be synced
    assert!(
        dest.path().join("small.txt").exists(),
        "small.txt should be synced (<= 1KB)"
    );
    assert!(
        !dest.path().join("medium.txt").exists(),
        "medium.txt should be filtered out by --max-size 1KB"
    );
    assert!(
        !dest.path().join("large.txt").exists(),
        "large.txt should be filtered out by --max-size 1KB"
    );
}

#[test]
fn test_max_size_exact_boundary() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create file exactly at boundary
    fs::write(source.path().join("exact.txt"), "x".repeat(1024)).unwrap();
    fs::write(source.path().join("over.txt"), "x".repeat(1025)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--max-size",
            "1KB",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Exact boundary should be included
    assert!(
        dest.path().join("exact.txt").exists(),
        "exact.txt (exactly 1KB) should be synced"
    );
    assert!(
        !dest.path().join("over.txt").exists(),
        "over.txt (1025 bytes) should be filtered out"
    );
}

// =============================================================================
// Combined min/max Tests
// =============================================================================

#[test]
fn test_min_max_size_combined() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create files of various sizes
    fs::write(source.path().join("tiny.txt"), "x".repeat(100)).unwrap(); // 100 bytes
    fs::write(source.path().join("small.txt"), "x".repeat(600)).unwrap(); // 600 bytes
    fs::write(source.path().join("medium.txt"), "x".repeat(1500)).unwrap(); // 1.5KB
    fs::write(source.path().join("large.txt"), "x".repeat(3000)).unwrap(); // 3KB

    // Only sync files between 500 bytes and 2KB
    let output = Command::new(sy_bin())
        .args([
            "--min-size",
            "500",
            "--max-size",
            "2KB",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --min-size --max-size failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Only small and medium should be synced
    assert!(
        !dest.path().join("tiny.txt").exists(),
        "tiny.txt should be filtered (< 500)"
    );
    assert!(
        dest.path().join("small.txt").exists(),
        "small.txt should be synced (500-2KB)"
    );
    assert!(
        dest.path().join("medium.txt").exists(),
        "medium.txt should be synced (500-2KB)"
    );
    assert!(
        !dest.path().join("large.txt").exists(),
        "large.txt should be filtered (> 2KB)"
    );
}

#[test]
fn test_min_size_greater_than_max_size_fails() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Should fail: min > max
    let output = Command::new(sy_bin())
        .args([
            "--min-size",
            "10KB",
            "--max-size",
            "1KB",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Should fail when --min-size > --max-size"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be greater") || stderr.contains("greater than"),
        "Error message should mention min > max: {}",
        stderr
    );
}

// =============================================================================
// Human-Readable Size Tests
// =============================================================================

#[test]
fn test_size_human_readable_formats() {
    let source = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create a 1.5MB file (1536000 bytes = 1500 * 1024 = 1500KB exactly)
    fs::write(source.path().join("large.bin"), vec![0u8; 1_536_000]).unwrap();

    // Test various human-readable formats
    // Note: 1KB = 1024 bytes, 1MB = 1048576 bytes
    let formats = vec![
        ("1MB", true),    // Should sync (1.5MB > 1MB)
        ("2MB", false),   // Should not sync (1.5MB < 2MB)
        ("1500KB", true), // Should sync (exactly at boundary)
    ];

    for (size_str, should_exist) in formats {
        let dest_test = TempDir::new().unwrap();

        let output = Command::new(sy_bin())
            .args([
                "--min-size",
                size_str,
                &format!("{}/", source.path().display()),
                dest_test.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "sy --min-size {} failed: {}",
            size_str,
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(
            dest_test.path().join("large.bin").exists(),
            should_exist,
            "--min-size {} should {} large.bin",
            size_str,
            if should_exist { "include" } else { "exclude" }
        );
    }
}

// =============================================================================
// Edge Cases
// =============================================================================

#[test]
fn test_min_size_zero_byte_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create empty file
    fs::write(source.path().join("empty.txt"), "").unwrap();
    fs::write(source.path().join("nonempty.txt"), "content").unwrap();

    // Min size > 0 should exclude empty files
    let output = Command::new(sy_bin())
        .args([
            "--min-size",
            "1",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    assert!(
        !dest.path().join("empty.txt").exists(),
        "Empty file should be filtered by --min-size 1"
    );
    assert!(dest.path().join("nonempty.txt").exists());
}

#[test]
fn test_size_filter_with_directories() {
    // Size filters should only affect files, not directories
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create directory with small file inside
    fs::create_dir(source.path().join("subdir")).unwrap();
    fs::write(source.path().join("subdir/small.txt"), "x").unwrap();
    fs::write(source.path().join("subdir/large.txt"), "x".repeat(2000)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--min-size",
            "1KB",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Directory should exist (directories are always synced)
    assert!(
        dest.path().join("subdir").exists(),
        "Directories should be synced regardless of --min-size"
    );
    // Small file should be filtered, large file should be synced
    assert!(!dest.path().join("subdir/small.txt").exists());
    assert!(dest.path().join("subdir/large.txt").exists());
}
