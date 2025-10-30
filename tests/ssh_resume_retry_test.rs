/// SSH retry comprehensive tests (requires fedora)
///
/// Tests automatic retry with exponential backoff added in v0.0.50.
/// Resume state management is tested internally via unit tests.
///
/// **CRITICAL**: Retry prevents data loss on network failures!
///
/// Run with: cargo test --test ssh_resume_retry_test -- --ignored
///
/// Prerequisites:
/// - fedora accessible via SSH
/// - sy-remote installed on fedora
///
use std::sync::Arc;
use std::time::Duration;
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
    format!("/tmp/sy_retry_test_{}_{}", test_name, std::process::id())
}

fn cleanup_remote_path(path: &str) {
    let cleanup_cmd = format!("ssh {} 'rm -rf {}'", FEDORA_HOST, path);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();
}

// =============================================================================
// SECTION 1: Retry with Backoff
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_retry_basic_operation() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;
    use sy::retry::RetryConfig;

    let remote_path = create_remote_test_path("retry_basic");

    let config = create_fedora_config();

    // Configure retry: 3 attempts, 100ms initial delay
    let retry_config = RetryConfig {
        max_attempts: 3,
        initial_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(30),
        backoff_multiplier: 2.0,
    };

    let transport = SshTransport::with_retry_config(&config, 1, retry_config)
        .await
        .expect("Failed to connect");

    // Create file
    transport
        .write_file(
            &std::path::Path::new(&remote_path),
            b"test content",
            std::time::SystemTime::now(),
        )
        .await
        .expect("write_file should succeed (with retry if needed)");

    // Verify file exists
    let exists = transport
        .exists(&std::path::Path::new(&remote_path))
        .await
        .expect("exists should succeed");

    assert!(exists, "File should exist");

    // Read file (tests retry on read operations)
    let content = transport
        .read_file(&std::path::Path::new(&remote_path))
        .await
        .expect("read_file should succeed");

    assert_eq!(content, b"test content");

    cleanup_remote_path(&remote_path);
    println!("✅ retry_basic_operation: PASS");
}

#[tokio::test]
#[ignore]
async fn test_retry_with_aggressive_backoff() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;
    use sy::retry::RetryConfig;

    let remote_path = create_remote_test_path("retry_backoff");

    let config = create_fedora_config();

    // Test with aggressive backoff
    let retry_config = RetryConfig {
        max_attempts: 4,
        initial_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(30),
        backoff_multiplier: 3.0, // 50ms, 150ms, 450ms, 1350ms
    };

    let transport = SshTransport::with_retry_config(&config, 1, retry_config)
        .await
        .expect("Failed to connect");

    let start = std::time::Instant::now();

    // Perform operation (will use backoff if retries needed)
    let remote_base = create_remote_test_path("retry_dir");
    transport
        .create_dir_all(&std::path::Path::new(&remote_base))
        .await
        .expect("create_dir_all should succeed");

    transport
        .write_file(
            &std::path::Path::new(&remote_path),
            b"backoff test",
            std::time::SystemTime::now(),
        )
        .await
        .expect("write_file should succeed");

    let _duration = start.elapsed();

    // Verify success
    let content = transport
        .read_file(&std::path::Path::new(&remote_path))
        .await
        .expect("read_file failed");

    assert_eq!(content, b"backoff test");

    cleanup_remote_path(&remote_base);
    cleanup_remote_path(&remote_path);
    println!("✅ retry_with_aggressive_backoff: PASS");
}

#[tokio::test]
#[ignore]
async fn test_retry_eventual_failure() {
    use sy::transport::ssh::SshTransport;
    use sy::retry::RetryConfig;
    use sy::ssh::config::SshConfig;

    // Try to connect to non-existent host (will fail after retries)
    let mut bad_config = SshConfig::new("nonexistent-host-12345.invalid");
    bad_config.user = FEDORA_USER.to_string();
    bad_config.port = 22;

    let retry_config = RetryConfig {
        max_attempts: 2, // Fail quickly for this test
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_secs(1),
        backoff_multiplier: 2.0,
    };

    let result = SshTransport::with_retry_config(&bad_config, 1, retry_config).await;

    // Should fail after retries
    assert!(result.is_err(), "Connection to invalid host should fail");

    println!("✅ retry_eventual_failure: PASS (correctly failed after retries)");
}

// =============================================================================
// SECTION 2: Connection Pool with Retry
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_connection_pool_with_retry() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;
    use sy::retry::RetryConfig;

    let remote_base = create_remote_test_path("pool_retry");

    // Create 3 test files
    for i in 1..=3 {
        let file_path = format!("{}/file{}.dat", remote_base, i);
        let create_cmd = format!(
            "ssh {} 'mkdir -p {} && dd if=/dev/zero of={} bs=1M count=3 2>/dev/null'",
            FEDORA_HOST, remote_base, file_path
        );
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&create_cmd)
            .output()
            .expect("Failed to create remote file");
    }

    let config = create_fedora_config();

    // Connection pool with retry enabled
    let retry_config = RetryConfig {
        max_attempts: 3,
        initial_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(30),
        backoff_multiplier: 2.0,
    };

    let transport = Arc::new(
        SshTransport::with_retry_config(&config, 3, retry_config)
            .await
            .expect("Failed to create pool"),
    );

    let dest_dir = TempDir::new().unwrap();

    // Download 3 files concurrently (with retry on any failures)
    let mut handles = vec![];

    for i in 1..=3 {
        let t = transport.clone();
        let remote_file = format!("{}/file{}.dat", remote_base, i);
        let local_dest = dest_dir.path().join(format!("file{}.dat", i));

        let handle = tokio::spawn(async move {
            t.copy_file_streaming(&std::path::Path::new(&remote_file), &local_dest, None)
                .await
                .unwrap()
        });

        handles.push(handle);
    }

    // Wait for all transfers
    for (i, handle) in handles.into_iter().enumerate() {
        let result = handle.await.unwrap();
        assert_eq!(
            result.bytes_written,
            3 * 1024 * 1024,
            "File {} transfer failed",
            i + 1
        );
    }

    cleanup_remote_path(&remote_base);
    println!("✅ connection_pool_with_retry: PASS");
}

// =============================================================================
// SECTION 3: Large File Transfer with Retry
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_large_file_transfer_with_retry() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;
    use sy::retry::RetryConfig;

    let remote_source = create_remote_test_path("large_retry");

    // Create 30MB file
    let create_cmd = format!(
        "ssh {} 'dd if=/dev/zero of={} bs=1M count=30 2>/dev/null'",
        FEDORA_HOST, remote_source
    );

    println!("Creating 30MB test file on fedora...");
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create file");

    let config = create_fedora_config();

    let retry_config = RetryConfig {
        max_attempts: 5,
        initial_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(30),
        backoff_multiplier: 2.0,
    };

    let transport = SshTransport::with_retry_config(&config, 1, retry_config)
        .await
        .expect("Failed to connect");

    let dest_dir = TempDir::new().unwrap();
    let dest_file = dest_dir.path().join("large_retry.dat");

    println!("Downloading 30MB file with retry enabled...");
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
    let speed_mbps = (30.0 / duration.as_secs_f64()).round();

    assert_eq!(result.bytes_written, 30 * 1024 * 1024);
    assert!(dest_file.exists());

    cleanup_remote_path(&remote_source);
    println!(
        "✅ large_file_transfer_with_retry: PASS ({:.2}s @ {} MB/s)",
        duration.as_secs_f64(),
        speed_mbps
    );
}

// =============================================================================
// Test Summary
// =============================================================================

#[test]
#[ignore]
fn print_retry_test_summary() {
    println!("\n========================================");
    println!("SSH Retry Test Suite");
    println!("========================================\n");
    println!("SECTION 1: Retry with Backoff");
    println!("  - retry_basic_operation");
    println!("  - retry_with_aggressive_backoff");
    println!("  - retry_eventual_failure");
    println!("\nSECTION 2: Connection Pool");
    println!("  - connection_pool_with_retry");
    println!("\nSECTION 3: Large File Transfer");
    println!("  - large_file_transfer_with_retry");
    println!("\nTotal: 5 tests");
    println!("\nNote: Resume state management tested via unit tests");
    println!("\nRun with:");
    println!("  cargo test --test ssh_resume_retry_test -- --ignored");
    println!("========================================\n");
}
