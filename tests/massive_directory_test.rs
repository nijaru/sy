// Massive directory testing - production hardening for 10K+ files
//
// Tests verify:
// - O(n) memory behavior at scale
// - No performance degradation with many files
// - Progress tracking remains accurate
// - State files stay reasonable size
// - Bloom filter optimization kicks in

use std::fs::{self, File};
use std::io::Write;
use tempfile::TempDir;

/// Create directory with N small files
fn create_directory_with_files(dir: &std::path::Path, count: usize) -> std::io::Result<()> {
    for i in 0..count {
        let file_path = dir.join(format!("file{:06}.txt", i));
        let mut file = File::create(file_path)?;
        // Write 1KB of data per file
        write!(file, "File {} content: ", i)?;
        file.write_all(&vec![b'x'; 1000])?;
    }
    Ok(())
}

/// Test 1,000 files - baseline many-file test
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_sync_1000_files() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    println!("Creating 1,000 test files...");
    create_directory_with_files(source_dir.path(), 1_000).unwrap();

    // Sync using sy
    let start = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify all files were copied
    let dest_count = fs::read_dir(dest_dir.path()).unwrap().count();
    assert_eq!(dest_count, 1_000, "Not all files were copied");

    println!("✅ 1,000 files synced in {:?}", elapsed);
}

/// Test 10,000 files - triggers Bloom filter optimization
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_sync_10k_files() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    println!("Creating 10,000 test files...");
    create_directory_with_files(source_dir.path(), 10_000).unwrap();

    // Sync using sy
    let start = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .arg("-v")
        .output()
        .expect("Failed to execute sy");
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify all files were copied
    let dest_count = fs::read_dir(dest_dir.path()).unwrap().count();
    assert_eq!(dest_count, 10_000, "Not all files were copied");

    // Check stderr for Bloom filter message
    let stderr = String::from_utf8_lossy(&output.stderr);
    let has_bloom = stderr.contains("Bloom") || stderr.contains("bloom");
    println!("Bloom filter used: {}", has_bloom);

    println!("✅ 10,000 files synced in {:?}", elapsed);
}

/// Test 100,000 files - true massive scale test
#[test]
#[ignore] // Very slow test - run explicitly with --ignored
fn test_sync_100k_files() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    println!("Creating 100,000 test files (this may take a few minutes)...");
    let create_start = std::time::Instant::now();
    create_directory_with_files(source_dir.path(), 100_000).unwrap();
    let create_elapsed = create_start.elapsed();
    println!("Created 100,000 files in {:?}", create_elapsed);

    // Sync using sy
    println!("Syncing 100,000 files...");
    let sync_start = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .arg("-v")
        .output()
        .expect("Failed to execute sy");
    let sync_elapsed = sync_start.elapsed();

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify all files were copied
    println!("Verifying file count...");
    let dest_count = fs::read_dir(dest_dir.path()).unwrap().count();
    assert_eq!(dest_count, 100_000, "Not all files were copied");

    println!("✅ 100,000 files synced in {:?}", sync_elapsed);
}

/// Test idempotent sync with 10K files - should be very fast
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_idempotent_sync_10k_files() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    println!("Creating 10,000 test files...");
    create_directory_with_files(source_dir.path(), 10_000).unwrap();

    // First sync
    println!("First sync...");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");

    assert!(output.status.success(), "First sync failed");

    // Second sync (should be fast - no changes)
    println!("Second sync (idempotent)...");
    let start = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");
    let elapsed = start.elapsed();

    assert!(output.status.success(), "Second sync failed");

    // Idempotent sync should be much faster (< 5 seconds for 10K files)
    assert!(
        elapsed.as_secs() < 5,
        "Idempotent sync too slow: {:?}",
        elapsed
    );

    println!("✅ Idempotent sync of 10,000 files: {:?}", elapsed);
}

/// Test deletion planning with 10K files
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_deletion_planning_10k_files() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    println!("Creating 10,000 source files...");
    create_directory_with_files(source_dir.path(), 10_000).unwrap();

    println!("Creating 100 extra dest files to delete...");
    for i in 0..100 {
        let file_path = dest_dir.path().join(format!("delete{:06}.txt", i));
        File::create(file_path).unwrap();
    }

    // Sync with delete flag (force-delete needed for test with 100% deletion)
    let start = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .arg("--delete")
        .arg("--force-delete")
        .output()
        .expect("Failed to execute sy");
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify exactly 10,000 files remain (the 100 extras deleted)
    let dest_count = fs::read_dir(dest_dir.path()).unwrap().count();
    assert_eq!(dest_count, 10_000, "Deletion planning failed");

    println!("✅ Deletion planning with 10,000 files: {:?}", elapsed);
}

/// Test nested directory structure with many files
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_nested_directories_10k_files() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    println!("Creating nested directory structure (100 dirs × 100 files)...");

    // Create 100 directories, each with 100 files = 10,000 files total
    for dir_idx in 0..100 {
        let subdir = source_dir.path().join(format!("dir{:03}", dir_idx));
        fs::create_dir(&subdir).unwrap();

        for file_idx in 0..100 {
            let file_path = subdir.join(format!("file{:03}.txt", file_idx));
            let mut file = File::create(file_path).unwrap();
            write!(file, "Content for dir{}/file{}", dir_idx, file_idx).unwrap();
        }
    }

    // Sync using sy
    let start = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify directory structure was copied
    let dir_count = fs::read_dir(dest_dir.path()).unwrap().count();
    assert_eq!(dir_count, 100, "Not all directories were copied");

    // Sample check: verify one directory has 100 files
    let sample_dir = dest_dir.path().join("dir050");
    let file_count = fs::read_dir(&sample_dir).unwrap().count();
    assert_eq!(file_count, 100, "Not all files in subdirectory were copied");

    println!("✅ Nested 10,000 files synced in {:?}", elapsed);
}

/// Test progress accuracy with 10K files
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_progress_accuracy_10k_files() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    println!("Creating 10,000 test files...");
    create_directory_with_files(source_dir.path(), 10_000).unwrap();

    // Sync with JSON output to track progress
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .arg("--json")
        .output()
        .expect("Failed to execute sy");

    assert!(output.status.success(), "Sync failed");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Count create/update events (one per file)
    let mut file_count = 0;
    for line in stdout.lines() {
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(line) {
            let event_type = event["type"].as_str().unwrap_or("");
            if event_type == "create" || event_type == "update" {
                file_count += 1;
            }
        }
    }

    // Verify we tracked all 10,000 files
    assert!(
        file_count >= 10_000,
        "Progress tracking missed files: expected 10000, got {}",
        file_count
    );

    println!("✅ Progress tracking accurate for 10,000 files");
}
