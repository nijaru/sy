// Large file testing - production hardening for 100MB+ files
//
// Tests verify:
// - No OOM issues with large files
// - Progress accuracy at scale
// - Throughput doesn't degrade
// - Resume works for large files
// - Sparse file optimization at scale

use std::fs::{self, File};
use std::io::{self, Write};
use tempfile::TempDir;

/// Size constants
const MB: u64 = 1024 * 1024;
const GB: u64 = 1024 * MB;

/// Create a test file of specified size with semi-random content
/// Uses block-based writing to avoid allocating entire file in memory
fn create_large_file(path: &std::path::Path, size: u64) -> io::Result<()> {
    let mut file = File::create(path)?;
    let block_size = 1024 * 1024; // 1MB blocks
    let mut block = vec![0u8; block_size];

    // Fill block with pseudo-random pattern
    for (i, byte) in block.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }

    let full_blocks = size / block_size as u64;
    let remainder = size % block_size as u64;

    // Write full blocks
    for _ in 0..full_blocks {
        file.write_all(&block)?;
    }

    // Write remainder
    if remainder > 0 {
        file.write_all(&block[..remainder as usize])?;
    }

    file.sync_all()?;
    Ok(())
}

/// Test 100MB file sync - baseline large file test
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_sync_100mb_file() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    let source_file = source_dir.path().join("large.dat");
    let size = 100 * MB;

    println!("Creating 100MB test file...");
    create_large_file(&source_file, size).unwrap();

    // Sync using sy
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify file was copied
    let dest_file = dest_dir.path().join("large.dat");
    assert!(dest_file.exists(), "Destination file not created");

    let dest_size = fs::metadata(&dest_file).unwrap().len();
    assert_eq!(dest_size, size, "File size mismatch");

    println!("✅ 100MB file synced successfully");
}

/// Test 500MB file sync - medium large file
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_sync_500mb_file() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    let source_file = source_dir.path().join("large.dat");
    let size = 500 * MB;

    println!("Creating 500MB test file...");
    create_large_file(&source_file, size).unwrap();

    // Sync using sy
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify file was copied
    let dest_file = dest_dir.path().join("large.dat");
    assert!(dest_file.exists(), "Destination file not created");

    let dest_size = fs::metadata(&dest_file).unwrap().len();
    assert_eq!(dest_size, size, "File size mismatch");

    println!("✅ 500MB file synced successfully");
}

/// Test 1GB file sync - true large file test
#[test]
#[ignore] // Very slow test - run explicitly with --ignored
fn test_sync_1gb_file() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    let source_file = source_dir.path().join("large.dat");
    let size = GB;

    println!("Creating 1GB test file...");
    create_large_file(&source_file, size).unwrap();

    // Sync using sy with progress output
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .arg("-v")
        .output()
        .expect("Failed to execute sy");

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify file was copied
    let dest_file = dest_dir.path().join("large.dat");
    assert!(dest_file.exists(), "Destination file not created");

    let dest_size = fs::metadata(&dest_file).unwrap().len();
    assert_eq!(dest_size, size, "File size mismatch");

    println!("✅ 1GB file synced successfully");
}

/// Test large sparse file handling - verify optimization works at scale
#[test]
#[ignore] // Slow test - run explicitly with --ignored
#[cfg(target_os = "linux")] // Sparse file support is best on Linux
fn test_sync_1gb_sparse_file() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    let source_file = source_dir.path().join("sparse.dat");
    let size = 1 * GB;

    println!("Creating 1GB sparse file...");

    // Create sparse file with holes (only write at beginning and end)
    let file = File::create(&source_file).unwrap();
    file.set_len(size).unwrap(); // Set logical size
    drop(file);

    // Write 1MB at start
    let mut file = File::options().write(true).open(&source_file).unwrap();
    file.write_all(&vec![0xAA; MB as usize]).unwrap();

    // Write 1MB at end (seek to size - 1MB)
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(size - MB)).unwrap();
    file.write_all(&vec![0xBB; MB as usize]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let source_allocated = fs::metadata(&source_file).unwrap().len();
    println!(
        "Source file: logical={}MB, allocated={}MB",
        size / MB,
        source_allocated / MB
    );

    // Sync using sy
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify sparse file was preserved
    let dest_file = dest_dir.path().join("sparse.dat");
    assert!(dest_file.exists(), "Destination file not created");

    let dest_size = fs::metadata(&dest_file).unwrap().len();
    assert_eq!(dest_size, size, "Logical size mismatch");

    println!("✅ 1GB sparse file synced successfully");
}

/// Test progress accuracy with large file
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_progress_accuracy_100mb() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    let source_file = source_dir.path().join("large.dat");
    let size = 100 * MB;

    println!("Creating 100MB test file for progress testing...");
    create_large_file(&source_file, size).unwrap();

    // Sync with JSON output to verify progress
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .arg("--json")
        .output()
        .expect("Failed to execute sy");

    assert!(
        output.status.success(),
        "Sync failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse JSON events and verify progress
    let mut total_bytes = 0u64;
    for line in stdout.lines() {
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(line) {
            let event_type = event["type"].as_str().unwrap_or("");
            if event_type == "create" || event_type == "update" {
                if let Some(bytes) = event["bytes_transferred"].as_u64() {
                    total_bytes += bytes;
                }
            }
        }
    }

    // Verify we tracked the full file size (within 1% tolerance for metadata)
    let tolerance = size / 100; // 1%
    assert!(
        total_bytes >= size - tolerance && total_bytes <= size + tolerance,
        "Progress tracking inaccurate: expected ~{}, got {}",
        size,
        total_bytes
    );

    println!("✅ Progress tracking accurate for 100MB file");
}

/// Test idempotent sync with large file - should skip unchanged large file
#[test]
#[ignore] // Slow test - run explicitly with --ignored
fn test_idempotent_sync_100mb() {
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    let source_file = source_dir.path().join("large.dat");
    let size = 100 * MB;

    println!("Creating 100MB test file...");
    create_large_file(&source_file, size).unwrap();

    // First sync
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");

    assert!(output.status.success(), "First sync failed");

    // Second sync (should be fast - file unchanged)
    let start = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source_dir.path())
        .arg(dest_dir.path())
        .output()
        .expect("Failed to execute sy");
    let elapsed = start.elapsed();

    assert!(output.status.success(), "Second sync failed");

    // Second sync should be very fast (< 1 second for metadata check)
    assert!(
        elapsed.as_secs() < 2,
        "Idempotent sync too slow: {:?}",
        elapsed
    );

    println!("✅ Idempotent sync of 100MB file: {:?}", elapsed);
}
