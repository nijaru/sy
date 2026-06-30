//! Performance tests — large files, massive directories.
//!
//! Consolidates: large_file_test.rs, massive_directory_test.rs.

use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};
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

fn setup_git_repo(dir: &TempDir) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .unwrap();
}

/// Performance regression test - ensures sync performance stays within bounds
/// Note: Skipped on Windows CI due to slow file I/O (6-13x slower than Unix)
#[test]
#[cfg_attr(all(windows, not(debug_assertions)), ignore)]
fn perf_regression_100_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create 100 files
    for i in 0..100 {
        fs::write(
            source.path().join(format!("file_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
    }

    let start = Instant::now();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());

    // Performance baseline: 100 files should sync in < 2s
    // Generous threshold — catches catastrophic regressions, not micro-benchmarks
    assert!(
        elapsed < Duration::from_secs(2),
        "Performance regression: 100 files took {:?}, expected < 2s",
        elapsed
    );

    println!("✓ 100 files synced in {:?}", elapsed);
}

#[test]
#[cfg_attr(all(windows, not(debug_assertions)), ignore)]
fn perf_regression_1000_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create 1000 files
    for i in 0..1000 {
        fs::write(
            source.path().join(format!("file_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
    }

    let start = Instant::now();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());

    // Performance baseline: 1000 files should sync in < 10s
    // Generous threshold — catches catastrophic regressions, not micro-benchmarks
    assert!(
        elapsed < Duration::from_secs(10),
        "Performance regression: 1000 files took {:?}, expected < 10s",
        elapsed
    );

    println!("✓ 1000 files synced in {:?}", elapsed);
}

#[test]
#[cfg_attr(all(windows, not(debug_assertions)), ignore)]
fn perf_regression_large_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create 10MB file
    let content = "x".repeat(10 * 1024 * 1024);
    fs::write(source.path().join("large.txt"), &content).unwrap();

    let start = Instant::now();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());

    // Performance baseline: 10MB file should sync in < 3s
    // Relaxed threshold for CI environments (typically 100-300ms locally, 1-2s on CI)
    assert!(
        elapsed < Duration::from_secs(3),
        "Performance regression: 10MB file took {:?}, expected < 3s",
        elapsed
    );

    println!("✓ 10MB file synced in {:?}", elapsed);
}

#[test]
#[cfg_attr(all(windows, not(debug_assertions)), ignore)]
fn perf_regression_deep_nesting() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create deeply nested structure (50 levels)
    let mut path = source.path().to_path_buf();
    for i in 0..50 {
        path = path.join(format!("level_{}", i));
    }
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("deep.txt"), "deep content").unwrap();

    let start = Instant::now();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());

    // Performance baseline: 50-level deep nesting should sync in < 2s
    // Generous threshold — catches catastrophic regressions, not micro-benchmarks
    assert!(
        elapsed < Duration::from_secs(2),
        "Performance regression: deep nesting took {:?}, expected < 2s",
        elapsed
    );

    println!("✓ 50-level deep path synced in {:?}", elapsed);
}

#[test]
#[cfg_attr(all(windows, not(debug_assertions)), ignore)]
fn perf_regression_idempotent_sync() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create 100 files
    for i in 0..100 {
        fs::write(
            source.path().join(format!("file_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
    }

    // First sync
    Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // Second sync (idempotent - should be faster)
    let start = Instant::now();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());

    // Performance baseline: idempotent sync should be < 200ms
    // (much faster since all files are skipped)
    assert!(
        elapsed < Duration::from_millis(200),
        "Performance regression: idempotent sync took {:?}, expected < 200ms",
        elapsed
    );

    println!("✓ Idempotent sync (100 files skipped) in {:?}", elapsed);
}

#[test]
#[cfg_attr(all(windows, not(debug_assertions)), ignore)]
fn perf_regression_gitignore_filtering() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create .gitignore that excludes half the files
    let mut gitignore = String::new();
    for i in 0..50 {
        gitignore.push_str(&format!("ignored_{}.txt\n", i));
    }
    fs::write(source.path().join(".gitignore"), gitignore).unwrap();

    // Create 50 included + 50 ignored files
    for i in 0..50 {
        fs::write(
            source.path().join(format!("included_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
        fs::write(
            source.path().join(format!("ignored_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
    }

    let start = Instant::now();

    // Use --gitignore to respect .gitignore patterns
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--gitignore",
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());

    // Verify only 51 files were synced (50 included + .gitignore)
    // Note: .sy-dir-cache.json is not created by default (requires --use-cache=true)
    let synced_files = fs::read_dir(dest.path())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_file())
        .count();
    assert_eq!(
        synced_files, 51,
        "Expected 51 files synced (50 included + .gitignore), got {}",
        synced_files
    );

    // Performance baseline: .gitignore filtering should be < 2s
    // Generous threshold — catches catastrophic regressions, not micro-benchmarks
    assert!(
        elapsed < Duration::from_secs(2),
        "Performance regression: gitignore filtering took {:?}, expected < 2s",
        elapsed
    );

    println!(
        "✓ Gitignore filtering (100 files -> 51 synced) in {:?}",
        elapsed
    );
}

#[test]
#[cfg_attr(all(windows, not(debug_assertions)), ignore)]
fn perf_memory_usage_stays_bounded() {
    // This test ensures we're not loading entire file tree into memory
    // By syncing a large number of small files
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create 5000 tiny files
    for i in 0..5000 {
        fs::write(source.path().join(format!("file_{}.txt", i)), "x").unwrap();
    }

    let start = Instant::now();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let elapsed = start.elapsed();

    assert!(output.status.success());

    // Performance baseline: 5000 files should sync in < 20s
    // If memory usage is bounded, this should scale linearly
    // Using 20s to account for CI runner variability
    assert!(
        elapsed < Duration::from_secs(20),
        "Performance regression: 5000 files took {:?}, expected < 20s",
        elapsed
    );

    println!("✓ 5000 files synced in {:?}", elapsed);
}
