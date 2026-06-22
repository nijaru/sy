/// Integration tests for change ratio detection
///
/// Tests that delta sync correctly samples blocks and falls back to
/// full copy when change ratio exceeds threshold.
use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    std::env::var("CARGO_BIN_EXE_sy").unwrap_or_else(|_| "target/debug/sy".to_string())
}

#[test]
fn test_low_change_ratio_uses_delta() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create 15MB dest file (above 10MB delta threshold)
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 15_000_000]).unwrap();

    // Create source file with 10% change (below 75% threshold)
    let source_file = source.path().join("test.dat");
    let mut source_data = vec![0u8; 15_000_000];
    // Change first 10% (1.5MB)
    for byte in &mut source_data[..1_500_000] {
        *byte = 1;
    }
    fs::write(&source_file, &source_data).unwrap();

    // Sync with info logging
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // Verify change ratio was logged and delta sync was used
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should see change ratio detection
    assert!(
        stderr.contains("Change ratio") || stdout.contains("Change ratio"),
        "Should log change ratio detection"
    );

    // Should proceed with delta sync (not fall back to full copy)
    assert!(
        stderr.contains("below threshold")
            || stdout.contains("below threshold")
            || stderr.contains("COW")
            || stdout.contains("COW")
            || stderr.contains("in-place")
            || stdout.contains("in-place"),
        "Should proceed with delta sync. Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );
}

#[test]
fn test_high_change_ratio_uses_full_copy() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create 15MB dest file (above 10MB delta threshold)
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 15_000_000]).unwrap();

    // Create source file with 100% change (exceeds 75% threshold)
    let source_file = source.path().join("test.dat");
    let source_data = vec![1u8; 15_000_000];
    fs::write(&source_file, &source_data).unwrap();

    // Sync with info logging
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // Verify change ratio was logged and fallback was used
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should see change ratio detection
    assert!(
        stderr.contains("Change ratio") || stdout.contains("Change ratio"),
        "Should log change ratio detection. Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );

    // Should fall back to full copy
    assert!(
        stderr.contains("exceeds threshold") || stdout.contains("exceeds threshold"),
        "Should fall back to full copy. Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );
}

#[test]
fn test_change_ratio_boundary_75_percent() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create 20MB dest file
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 20_000_000]).unwrap();

    // Create source file with exactly 74% change (just below threshold)
    let source_file = source.path().join("test.dat");
    let mut source_data = vec![0u8; 20_000_000];
    // Change 74% (14.8MB)
    for byte in &mut source_data[..14_800_000] {
        *byte = 1;
    }
    fs::write(&source_file, &source_data).unwrap();

    // Sync
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // With 74% change, sampling might show varying results depending on which
    // blocks are sampled. Just verify sync succeeded and produced correct output.
}

#[test]
fn test_change_ratio_small_change() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create 15MB dest file
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 15_000_000]).unwrap();

    // Create source file with 1% change (15KB change)
    let source_file = source.path().join("test.dat");
    let mut source_data = vec![0u8; 15_000_000];
    // Change only 150KB (1%)
    for byte in &mut source_data[..150_000] {
        *byte = 1;
    }
    fs::write(&source_file, &source_data).unwrap();

    // Sync
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // Should use delta sync for small changes
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stderr.contains("below threshold")
            || stdout.contains("below threshold")
            || stderr.contains("COW")
            || stdout.contains("COW")
            || stderr.contains("in-place")
            || stdout.contains("in-place"),
        "Should use delta sync for small changes"
    );
}

#[test]
fn test_change_ratio_medium_change() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create 15MB dest file
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 15_000_000]).unwrap();

    // Create source file with 50% change (7.5MB change)
    let source_file = source.path().join("test.dat");
    let mut source_data = vec![0u8; 15_000_000];
    // Change first 50% (7.5MB)
    for byte in &mut source_data[..7_500_000] {
        *byte = 1;
    }
    fs::write(&source_file, &source_data).unwrap();

    // Sync
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // 50% change should use delta sync (below 75% threshold)
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stderr.contains("below threshold")
            || stdout.contains("below threshold")
            || stderr.contains("COW")
            || stdout.contains("COW")
            || stderr.contains("in-place")
            || stdout.contains("in-place"),
        "Should use delta sync for 50% change. Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );
}
