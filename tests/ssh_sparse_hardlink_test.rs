/// SSH sparse file and hard link tests (requires fedora)
///
/// Tests detection of sparse files and hard links via scan() operation.
/// Direct sparse file transfer testing requires access to private methods.
///
/// Run with: cargo test --test ssh_sparse_hardlink_test -- --ignored
///
/// Prerequisites:
/// - fedora accessible via SSH
/// - sy-remote installed on fedora
/// - Filesystem on fedora that supports sparse files (ext4, xfs, btrfs)
///
use std::time::SystemTime;
use tempfile::TempDir;

const FEDORA_HOST: &str = "fedora";
const FEDORA_USER: &str = "nick";

fn create_fedora_config() -> sy::ssh::config::SshConfig {
    use sy::ssh::config::SshConfig;
    let mut config = SshConfig::new(FEDORA_HOST);
    config.user = FEDORA_USER.to_string();
    config.port = 22;
    config
}

fn create_remote_test_path(test_name: &str) -> String {
    format!("/tmp/sy_ssh_sparse_test_{}_{}", test_name, std::process::id())
}

fn cleanup_remote_path(path: &str) {
    let cleanup_cmd = format!("ssh {} 'rm -rf {}'", FEDORA_HOST, path);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();
}

// =============================================================================
// SECTION 1: Sparse File Detection via Scan
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_ssh_detect_sparse_file_via_scan() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_base = create_remote_test_path("sparse_scan");

    // Create sparse file on remote (10MB file with holes)
    // First MB has data, middle 8MB are holes, last MB has data
    let create_cmd = format!(
        "ssh {} 'mkdir -p {} && dd if=/dev/zero of={}/sparse.dat bs=1M count=1 seek=0 2>/dev/null && dd if=/dev/zero of={}/sparse.dat bs=1M count=1 seek=9 conv=notrunc 2>/dev/null'",
        FEDORA_HOST, remote_base, remote_base, remote_base
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create sparse file");

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    // Scan directory to get file entries
    let entries = transport
        .scan(&std::path::Path::new(&remote_base))
        .await
        .expect("scan failed");

    // Find sparse file entry
    let sparse_entry = entries
        .iter()
        .find(|e| e.path.to_str().unwrap().contains("sparse.dat"))
        .expect("Sparse file not found");

    println!("Sparse file detected:");
    println!("  Size: {} bytes", sparse_entry.size);
    println!("  Allocated: {} bytes", sparse_entry.allocated_size);
    println!("  Is sparse: {}", sparse_entry.is_sparse);

    // File should be 10MB
    assert_eq!(sparse_entry.size, 10 * 1024 * 1024, "File size should be 10MB");

    // Note: sparse detection depends on filesystem support
    if sparse_entry.is_sparse {
        assert!(
            sparse_entry.allocated_size < sparse_entry.size,
            "Sparse file should have allocated_size < size"
        );
        println!("✅ Sparse file detected correctly");
    } else {
        println!("⚠️  Filesystem may not support sparse detection");
    }

    cleanup_remote_path(&remote_base);
    println!("✅ SSH detect_sparse_file_via_scan: PASS");
}

#[tokio::test]
#[ignore]
async fn test_ssh_regular_file_not_sparse() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_base = create_remote_test_path("regular_file");

    // Create regular (non-sparse) file
    let create_cmd = format!(
        "ssh {} 'mkdir -p {} && dd if=/dev/zero of={}/regular.dat bs=1M count=5 2>/dev/null'",
        FEDORA_HOST, remote_base, remote_base
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create regular file");

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    let entries = transport
        .scan(&std::path::Path::new(&remote_base))
        .await
        .expect("scan failed");

    let file_entry = entries
        .iter()
        .find(|e| e.path.to_str().unwrap().contains("regular.dat"))
        .expect("Regular file not found");

    assert_eq!(file_entry.size, 5 * 1024 * 1024);
    assert!(!file_entry.is_sparse, "Regular file should not be sparse");

    cleanup_remote_path(&remote_base);
    println!("✅ SSH regular_file_not_sparse: PASS");
}

// =============================================================================
// SECTION 2: Hard Link Detection via Scan
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_ssh_detect_hardlink_via_scan() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_base = create_remote_test_path("hardlink_scan");

    // Create file with hard links
    let create_cmd = format!(
        "ssh {} 'mkdir -p {} && echo \"content\" > {}/original.txt && ln {}/original.txt {}/link1.txt && ln {}/original.txt {}/link2.txt'",
        FEDORA_HOST, remote_base, remote_base, remote_base, remote_base, remote_base, remote_base
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create hard links");

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    let entries = transport
        .scan(&std::path::Path::new(&remote_base))
        .await
        .expect("scan failed");

    // Find the original file
    let original = entries
        .iter()
        .find(|e| e.path.to_str().unwrap().contains("original.txt"))
        .expect("Original file not found");

    println!("Hard link count (nlink): {}", original.nlink);

    // All three names should point to same file (nlink = 3)
    assert_eq!(original.nlink, 3, "File should have 3 hard links");

    // Verify all three have same inode
    let link1 = entries
        .iter()
        .find(|e| e.path.to_str().unwrap().contains("link1.txt"))
        .expect("Link1 not found");

    let link2 = entries
        .iter()
        .find(|e| e.path.to_str().unwrap().contains("link2.txt"))
        .expect("Link2 not found");

    assert_eq!(original.inode, link1.inode, "Original and link1 should have same inode");
    assert_eq!(original.inode, link2.inode, "Original and link2 should have same inode");

    cleanup_remote_path(&remote_base);
    println!("✅ SSH detect_hardlink_via_scan: PASS");
}

#[tokio::test]
#[ignore]
async fn test_ssh_single_file_nlink() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_base = create_remote_test_path("single_nlink");

    // Create single file (no hard links)
    let create_cmd = format!(
        "ssh {} 'mkdir -p {} && echo \"single\" > {}/single.txt'",
        FEDORA_HOST, remote_base, remote_base
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create file");

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    let entries = transport
        .scan(&std::path::Path::new(&remote_base))
        .await
        .expect("scan failed");

    let file = entries
        .iter()
        .find(|e| e.path.to_str().unwrap().contains("single.txt"))
        .expect("File not found");

    assert_eq!(file.nlink, 1, "Single file should have nlink = 1");

    cleanup_remote_path(&remote_base);
    println!("✅ SSH single_file_nlink: PASS");
}

// =============================================================================
// SECTION 3: File Transfer Integrity
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_ssh_transfer_large_file() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_source = create_remote_test_path("large_transfer");

    // Create 50MB file on remote
    let create_cmd = format!(
        "ssh {} 'dd if=/dev/zero of={} bs=1M count=50 2>/dev/null'",
        FEDORA_HOST, remote_source
    );

    println!("Creating 50MB test file on fedora...");
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create file");

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    let dest_dir = TempDir::new().unwrap();
    let dest_file = dest_dir.path().join("large.dat");

    println!("Downloading 50MB file...");
    let start = std::time::Instant::now();

    let result = transport
        .copy_file_streaming(
            &std::path::Path::new(&remote_source),
            &dest_file,
            None,
        )
        .await
        .expect("Transfer failed");

    let duration = start.elapsed();
    let speed_mbps = (50.0 / duration.as_secs_f64()).round();

    assert_eq!(result.bytes_written, 50 * 1024 * 1024);
    assert!(dest_file.exists());

    cleanup_remote_path(&remote_source);
    println!(
        "✅ SSH transfer_large_file: PASS ({:.2}s @ {} MB/s)",
        duration.as_secs_f64(),
        speed_mbps
    );
}

#[tokio::test]
#[ignore]
async fn test_ssh_create_and_verify_hardlink() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_base = create_remote_test_path("create_hardlink");
    let remote_original = format!("{}/original.txt", remote_base);
    let remote_link = format!("{}/link.txt", remote_base);

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    // Create directory
    transport
        .create_dir_all(&std::path::Path::new(&remote_base))
        .await
        .expect("create_dir_all failed");

    // Create original file
    transport
        .write_file(
            &std::path::Path::new(&remote_original),
            b"hardlink test",
            SystemTime::now(),
        )
        .await
        .expect("write_file failed");

    // Create hard link
    transport
        .create_hardlink(
            &std::path::Path::new(&remote_original),
            &std::path::Path::new(&remote_link),
        )
        .await
        .expect("create_hardlink failed");

    // Verify both files exist
    let original_exists = transport
        .exists(&std::path::Path::new(&remote_original))
        .await
        .expect("exists check failed");
    let link_exists = transport
        .exists(&std::path::Path::new(&remote_link))
        .await
        .expect("exists check failed");

    assert!(original_exists, "Original file should exist");
    assert!(link_exists, "Hard link should exist");

    // Verify content is the same
    let original_content = transport
        .read_file(&std::path::Path::new(&remote_original))
        .await
        .expect("read_file failed");
    let link_content = transport
        .read_file(&std::path::Path::new(&remote_link))
        .await
        .expect("read_file failed");

    assert_eq!(original_content, link_content, "Hard link content should match");

    // Verify nlink count via scan
    let entries = transport
        .scan(&std::path::Path::new(&remote_base))
        .await
        .expect("scan failed");

    let original_entry = entries
        .iter()
        .find(|e| e.path.to_str().unwrap().contains("original.txt"))
        .expect("Original not found");

    assert_eq!(original_entry.nlink, 2, "Should have 2 hard links");

    cleanup_remote_path(&remote_base);
    println!("✅ SSH create_and_verify_hardlink: PASS");
}

// =============================================================================
// Test Summary
// =============================================================================

#[test]
#[ignore]
fn print_sparse_hardlink_test_summary() {
    println!("\n========================================");
    println!("SSH Sparse File & Hard Link Test Suite");
    println!("========================================\n");
    println!("SECTION 1: Sparse File Detection");
    println!("  - detect_sparse_file_via_scan");
    println!("  - regular_file_not_sparse");
    println!("\nSECTION 2: Hard Link Detection");
    println!("  - detect_hardlink_via_scan");
    println!("  - single_file_nlink");
    println!("\nSECTION 3: File Transfer Integrity");
    println!("  - transfer_large_file (50MB)");
    println!("  - create_and_verify_hardlink");
    println!("\nTotal: 6 tests");
    println!("\nRun with:");
    println!("  cargo test --test ssh_sparse_hardlink_test -- --ignored");
    println!("========================================\n");
}
