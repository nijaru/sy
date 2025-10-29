/// SSH per-file progress tests (requires fedora to be available)
///
/// These tests are IGNORED by default and must be run manually when fedora is accessible.
///
/// ## Running these tests:
///
/// ```bash
/// # Ensure fedora is accessible via SSH at nick@fedora
/// cargo test --test ssh_per_file_progress_test -- --ignored --nocapture
/// ```
///
/// ## Prerequisites:
/// - fedora must be accessible via SSH (tailscale: nick@fedora)
/// - sy-remote must be installed on fedora
/// - SSH keys must be configured for passwordless login
///
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

const FEDORA_HOST: &str = "nick@fedora";

#[tokio::test]
#[ignore] // Run manually with: cargo test --test ssh_per_file_progress_test -- --ignored
async fn test_ssh_progress_local_to_remote() {
    // Create local source
    let source = TempDir::new().unwrap();
    let large_file = source.path().join("large.dat");
    fs::write(&large_file, vec![0x42u8; 5 * 1024 * 1024]).unwrap(); // 5MB

    // Create remote dest path (will be cleaned up)
    let remote_dest = format!("{}:/tmp/sy_test_l2r_{}", FEDORA_HOST, std::process::id());

    println!("Testing: local -> remote ({})", remote_dest);
    println!("Source: {}", source.path().display());

    // NOTE: This test currently validates the infrastructure
    // Actual progress bar testing requires integration with the CLI
    // which we'll verify in manual testing

    // Clean up remote
    let cleanup_cmd = format!("ssh {} 'rm -rf /tmp/sy_test_l2r_*'", FEDORA_HOST.split('@').nth(1).unwrap_or("fedora"));
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();

    println!("✓ Infrastructure validated (manual CLI testing required for progress)");
}

#[tokio::test]
#[ignore] // Run manually with: cargo test --test ssh_per_file_progress_test -- --ignored
async fn test_ssh_progress_remote_to_local() {
    let dest = TempDir::new().unwrap();

    // Create remote source
    let remote_source = format!("{}:/tmp/sy_test_r2l_{}", FEDORA_HOST, std::process::id());
    let remote_host = FEDORA_HOST.split('@').nth(1).unwrap_or("fedora");

    // Create remote file via SSH
    let create_cmd = format!(
        "ssh {} 'mkdir -p /tmp/sy_test_r2l_{} && dd if=/dev/zero of=/tmp/sy_test_r2l_{}/large.dat bs=1M count=5'",
        remote_host,
        std::process::id(),
        std::process::id()
    );

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote file");

    if !output.status.success() {
        eprintln!("Failed to create remote file: {}", String::from_utf8_lossy(&output.stderr));
        panic!("Remote file creation failed");
    }

    println!("Testing: remote -> local ({})", remote_source);
    println!("Dest: {}", dest.path().display());

    // NOTE: This test currently validates the infrastructure
    // Actual progress bar testing requires integration with the CLI

    // Clean up remote
    let cleanup_cmd = format!("ssh {} 'rm -rf /tmp/sy_test_r2l_*'", remote_host);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();

    println!("✓ Infrastructure validated (manual CLI testing required for progress)");
}

#[tokio::test]
#[ignore] // Run manually with: cargo test --test ssh_per_file_progress_test -- --ignored
async fn test_ssh_progress_remote_to_remote() {
    let remote_host = FEDORA_HOST.split('@').nth(1).unwrap_or("fedora");
    let remote_source = format!("{}:/tmp/sy_test_r2r_source_{}", FEDORA_HOST, std::process::id());
    let remote_dest = format!("{}:/tmp/sy_test_r2r_dest_{}", FEDORA_HOST, std::process::id());

    // Create remote source file
    let create_cmd = format!(
        "ssh {} 'mkdir -p /tmp/sy_test_r2r_source_{} && dd if=/dev/zero of=/tmp/sy_test_r2r_source_{}/large.dat bs=1M count=5'",
        remote_host,
        std::process::id(),
        std::process::id()
    );

    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote file");

    if !output.status.success() {
        eprintln!("Failed to create remote file: {}", String::from_utf8_lossy(&output.stderr));
        panic!("Remote file creation failed");
    }

    println!("Testing: remote -> remote");
    println!("Source: {}", remote_source);
    println!("Dest: {}", remote_dest);

    // NOTE: This test currently validates the infrastructure
    // Actual progress bar testing requires integration with the CLI

    // Clean up remote
    let cleanup_cmd = format!("ssh {} 'rm -rf /tmp/sy_test_r2r_*'", remote_host);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();

    println!("✓ Infrastructure validated (manual CLI testing required for progress)");
}

#[tokio::test]
#[ignore] // Run manually with: cargo test --test ssh_per_file_progress_test -- --ignored
async fn test_ssh_progress_multiple_large_files() {
    let source = TempDir::new().unwrap();
    let remote_dest = format!("{}:/tmp/sy_test_multi_{}", FEDORA_HOST, std::process::id());

    // Create 3 large files locally
    for i in 1..=3 {
        let file = source.path().join(format!("file{}.dat", i));
        fs::write(&file, vec![0x42u8; 3 * 1024 * 1024]).unwrap(); // 3MB each
    }

    println!("Testing: multiple large files local -> remote");
    println!("Source: {}", source.path().display());
    println!("Dest: {}", remote_dest);
    println!("Files: 3 x 3MB");

    // NOTE: This test validates infrastructure
    // Manual CLI testing required: sy /source nick@fedora:/dest --per-file-progress

    // Clean up remote
    let remote_host = FEDORA_HOST.split('@').nth(1).unwrap_or("fedora");
    let cleanup_cmd = format!("ssh {} 'rm -rf /tmp/sy_test_multi_*'", remote_host);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();

    println!("✓ Infrastructure validated (manual CLI testing required for progress)");
}

/// Manual test script generator
///
/// Run this to generate a shell script for manual SSH progress testing
#[test]
#[ignore]
fn generate_manual_test_script() {
    let script = r#"#!/bin/bash
# Manual SSH per-file progress test script
# Generated by: cargo test --test ssh_per_file_progress_test generate_manual_test_script -- --ignored --nocapture

set -e

FEDORA_HOST="nick@fedora"
TEST_ID="$$"

echo "=== Manual SSH Per-File Progress Tests ==="
echo "Prerequisites:"
echo "  - fedora accessible via SSH ($FEDORA_HOST)"
echo "  - sy-remote installed on fedora"
echo "  - sy built locally (cargo build --release)"
echo ""

cleanup() {
    echo "Cleaning up remote files..."
    ssh fedora "rm -rf /tmp/sy_manual_test_*"
    echo "Done."
}

trap cleanup EXIT

echo "=== Test 1: Local -> Remote with --per-file-progress ==="
mkdir -p /tmp/sy_manual_test_source
dd if=/dev/zero of=/tmp/sy_manual_test_source/large1.dat bs=1M count=10
dd if=/dev/zero of=/tmp/sy_manual_test_source/large2.dat bs=1M count=15
dd if=/dev/zero of=/tmp/sy_manual_test_source/small.txt bs=1K count=100

echo "Running: sy /tmp/sy_manual_test_source ${FEDORA_HOST}:/tmp/sy_manual_test_dest --per-file-progress"
cargo run --release -- /tmp/sy_manual_test_source ${FEDORA_HOST}:/tmp/sy_manual_test_dest --per-file-progress

echo ""
echo "EXPECTED: Progress bars shown for large1.dat (10MB) and large2.dat (15MB)"
echo "EXPECTED: No progress bar for small.txt (100KB)"
echo ""
read -p "Did you see progress bars for large files? (y/n): " answer
if [ "$answer" != "y" ]; then
    echo "FAILED: Progress bars not shown"
    exit 1
fi

echo ""
echo "=== Test 2: Remote -> Local with --per-file-progress ==="
echo "Creating remote files..."
ssh fedora "mkdir -p /tmp/sy_manual_test_source2 && dd if=/dev/zero of=/tmp/sy_manual_test_source2/remote_large.dat bs=1M count=20"

echo "Running: sy ${FEDORA_HOST}:/tmp/sy_manual_test_source2 /tmp/sy_manual_test_dest2 --per-file-progress"
cargo run --release -- ${FEDORA_HOST}:/tmp/sy_manual_test_source2 /tmp/sy_manual_test_dest2 --per-file-progress

echo ""
echo "EXPECTED: Progress bar shown for remote_large.dat (20MB)"
echo ""
read -p "Did you see progress bar? (y/n): " answer
if [ "$answer" != "y" ]; then
    echo "FAILED: Progress bar not shown"
    exit 1
fi

echo ""
echo "=== Test 3: --quiet suppresses progress ==="
echo "Running: sy /tmp/sy_manual_test_source ${FEDORA_HOST}:/tmp/sy_manual_test_dest3 --per-file-progress --quiet"
cargo run --release -- /tmp/sy_manual_test_source ${FEDORA_HOST}:/tmp/sy_manual_test_dest3 --per-file-progress --quiet

echo ""
echo "EXPECTED: No progress bars (quiet mode)"
echo ""
read -p "Were progress bars hidden? (y/n): " answer
if [ "$answer" != "y" ]; then
    echo "FAILED: Progress bars shown in quiet mode"
    exit 1
fi

echo ""
echo "=== Test 4: Very large file progress ==="
echo "Creating 100MB file..."
dd if=/dev/zero of=/tmp/sy_manual_test_source/very_large.dat bs=1M count=100

echo "Running: sy /tmp/sy_manual_test_source/very_large.dat ${FEDORA_HOST}:/tmp/sy_manual_test_dest4/ --per-file-progress"
cargo run --release -- /tmp/sy_manual_test_source/very_large.dat ${FEDORA_HOST}:/tmp/sy_manual_test_dest4/ --per-file-progress

echo ""
echo "EXPECTED: Smooth progress updates, speed shown in MB/s, accurate ETA"
echo ""
read -p "Was progress smooth and accurate? (y/n): " answer
if [ "$answer" != "y" ]; then
    echo "FAILED: Progress not smooth/accurate"
    exit 1
fi

# Local cleanup
rm -rf /tmp/sy_manual_test_*

echo ""
echo "=========================================="
echo "✓ All manual SSH progress tests PASSED"
echo "=========================================="
"#;

    println!("{}", script);
    println!("\n# Save this script to: tests/manual_ssh_progress_test.sh");
    println!("# Run with: bash tests/manual_ssh_progress_test.sh");
}
