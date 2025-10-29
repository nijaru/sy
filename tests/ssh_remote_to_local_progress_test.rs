/// SSH remote→local progress tests (requires fedora)
///
/// These tests verify that progress callbacks work correctly when downloading
/// files from remote servers via SSH transport.
///
/// **Scope**: Remote→Local transfers only
/// **Local→Remote**: Tested via manual CLI testing (see ssh_per_file_progress_test.rs)
///
/// Run with: cargo test --test ssh_remote_to_local_progress_test -- --ignored
///
/// Prerequisites:
/// - fedora accessible via SSH
/// - SSH keys configured for passwordless login
///
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

const FEDORA_HOST: &str = "fedora";
const FEDORA_USER: &str = "nick";

/// Helper to create SSH config for fedora
fn create_fedora_config() -> sy::ssh::config::SshConfig {
    use sy::ssh::config::SshConfig;

    let mut config = SshConfig::new(FEDORA_HOST);
    config.user = FEDORA_USER.to_string();
    config.port = 22;
    config
}

/// Helper to create a remote file via SSH command
fn create_remote_file(path: &str, size_mb: usize) {
    let create_cmd = format!(
        "ssh {} 'dd if=/dev/zero of={} bs=1M count={} 2>/dev/null'",
        FEDORA_HOST, path, size_mb
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote file");
}

/// Helper to cleanup remote file
fn cleanup_remote_file(path: &str) {
    let cleanup_cmd = format!("ssh {} 'rm -f {}'", FEDORA_HOST, path);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();
}

#[tokio::test]
#[ignore]
async fn test_remote_to_local_with_progress() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_source = format!("/tmp/sy_test_{}_2mb.dat", std::process::id());
    create_remote_file(&remote_source, 2);

    let config = create_fedora_config();
    let transport = SshTransport::new(&config)
        .await
        .expect("Failed to connect to fedora");

    let dest_dir = TempDir::new().unwrap();
    let local_dest = dest_dir.path().join("downloaded.dat");

    // Track progress
    let updates = Arc::new(AtomicU64::new(0));
    let updates_clone = updates.clone();
    let last_bytes = Arc::new(AtomicU64::new(0));
    let last_bytes_clone = last_bytes.clone();

    let progress_callback = Arc::new(move |bytes: u64, total: u64| {
        updates_clone.fetch_add(1, Ordering::SeqCst);
        last_bytes_clone.store(bytes, Ordering::SeqCst);
        assert!(bytes <= total, "bytes should not exceed total");
    });

    // Download with progress
    let result = transport
        .copy_file_streaming(
            &std::path::Path::new(&remote_source),
            &local_dest,
            Some(progress_callback),
        )
        .await
        .expect("Download failed");

    // Verify
    assert_eq!(result.bytes_written, 2 * 1024 * 1024);
    assert!(local_dest.exists());

    let update_count = updates.load(Ordering::SeqCst);
    assert!(update_count >= 3, "Expected progress updates, got {}", update_count);

    let final_bytes = last_bytes.load(Ordering::SeqCst);
    assert_eq!(final_bytes, 2 * 1024 * 1024);

    cleanup_remote_file(&remote_source);

    println!("✅ Remote→Local (2MB) with progress: PASS");
}

#[tokio::test]
#[ignore]
async fn test_remote_to_local_large_file() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_source = format!("/tmp/sy_test_{}_10mb.dat", std::process::id());
    create_remote_file(&remote_source, 10);

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    let dest_dir = TempDir::new().unwrap();
    let local_dest = dest_dir.path().join("large.dat");

    let updates = Arc::new(AtomicU64::new(0));
    let updates_clone = updates.clone();

    let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
        updates_clone.fetch_add(1, Ordering::SeqCst);
    });

    let result = transport
        .copy_file_streaming(
            &std::path::Path::new(&remote_source),
            &local_dest,
            Some(progress_callback),
        )
        .await
        .expect("Large file download failed");

    assert_eq!(result.bytes_written, 10 * 1024 * 1024);

    let update_count = updates.load(Ordering::SeqCst);
    assert!(update_count >= 10, "Expected >= 10 updates for 10MB, got {}", update_count);

    cleanup_remote_file(&remote_source);

    println!("✅ Remote→Local (10MB) with progress: PASS");
}

#[tokio::test]
#[ignore]
async fn test_remote_to_local_progress_monotonic() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    let remote_source = format!("/tmp/sy_test_{}_5mb.dat", std::process::id());
    create_remote_file(&remote_source, 5);

    let config = create_fedora_config();
    let transport = SshTransport::new(&config).await.expect("Failed to connect");

    let dest_dir = TempDir::new().unwrap();
    let local_dest = dest_dir.path().join("mono.dat");

    let last_bytes = Arc::new(AtomicU64::new(0));
    let last_bytes_clone = last_bytes.clone();
    let monotonic = Arc::new(AtomicBool::new(true));
    let monotonic_clone = monotonic.clone();

    let progress_callback = Arc::new(move |bytes: u64, _total: u64| {
        let prev = last_bytes_clone.load(Ordering::SeqCst);
        if bytes < prev {
            monotonic_clone.store(false, Ordering::SeqCst);
        }
        last_bytes_clone.store(bytes, Ordering::SeqCst);
    });

    let _ = transport
        .copy_file_streaming(
            &std::path::Path::new(&remote_source),
            &local_dest,
            Some(progress_callback),
        )
        .await
        .expect("Monotonic test failed");

    assert!(monotonic.load(Ordering::SeqCst), "Progress must increase monotonically");

    cleanup_remote_file(&remote_source);

    println!("✅ Remote→Local progress monotonic: PASS");
}

#[tokio::test]
#[ignore]
async fn test_remote_to_local_connection_pool() {
    use sy::transport::ssh::SshTransport;
    use sy::transport::Transport;

    // Create 2 remote files
    let remote1 = format!("/tmp/sy_test_{}_pool1.dat", std::process::id());
    let remote2 = format!("/tmp/sy_test_{}_pool2.dat", std::process::id());
    create_remote_file(&remote1, 3);
    create_remote_file(&remote2, 3);

    let config = create_fedora_config();
    let transport = Arc::new(
        SshTransport::with_pool_size(&config, 2)
            .await
            .expect("Failed to create pool"),
    );

    assert_eq!(transport.pool_size(), 2);

    let dest_dir = TempDir::new().unwrap();

    // Download files concurrently
    let t1 = transport.clone();
    let t2 = transport.clone();
    let dest_path = dest_dir.path().to_path_buf();
    let remote1_clone = remote1.clone();
    let remote2_clone = remote2.clone();

    let h1 = tokio::spawn(async move {
        let local_dest = dest_path.join("pool1.dat");
        let updates = Arc::new(AtomicU64::new(0));
        let updates_clone = updates.clone();

        let progress_callback = Arc::new(move |_: u64, _: u64| {
            updates_clone.fetch_add(1, Ordering::SeqCst);
        });

        let result = t1
            .copy_file_streaming(
                &std::path::Path::new(&remote1_clone),
                &local_dest,
                Some(progress_callback),
            )
            .await
            .unwrap();

        (result, updates.load(Ordering::SeqCst))
    });

    let dest_path2 = dest_dir.path().to_path_buf();
    let h2 = tokio::spawn(async move {
        let local_dest = dest_path2.join("pool2.dat");
        let updates = Arc::new(AtomicU64::new(0));
        let updates_clone = updates.clone();

        let progress_callback = Arc::new(move |_: u64, _: u64| {
            updates_clone.fetch_add(1, Ordering::SeqCst);
        });

        let result = t2
            .copy_file_streaming(
                &std::path::Path::new(&remote2_clone),
                &local_dest,
                Some(progress_callback),
            )
            .await
            .unwrap();

        (result, updates.load(Ordering::SeqCst))
    });

    let (result1, updates1) = h1.await.unwrap();
    let (result2, updates2) = h2.await.unwrap();

    assert_eq!(result1.bytes_written, 3 * 1024 * 1024);
    assert_eq!(result2.bytes_written, 3 * 1024 * 1024);
    assert!(updates1 >= 3, "Pool transfer 1 should have progress");
    assert!(updates2 >= 3, "Pool transfer 2 should have progress");

    cleanup_remote_file(&remote1);
    cleanup_remote_file(&remote2);

    println!("✅ Remote→Local connection pool with progress: PASS");
}

/// Note: Local→Remote testing
///
/// Local→Remote transfers use DualTransport (not SshTransport directly).
/// These should be tested via:
///
/// 1. Manual CLI testing with --per-file-progress:
///    ```
///    sy /local/file nick@fedora:/remote/dest --per-file-progress
///    ```
///
/// 2. The manual test script generator in ssh_per_file_progress_test.rs:
///    ```
///    cargo test --test ssh_per_file_progress_test generate_manual_test_script -- --ignored --nocapture > manual_test.sh
///    bash manual_test.sh
///    ```
///
/// DualTransport testing would require more complex setup and is better
/// validated through end-to-end CLI testing.
#[test]
#[ignore]
fn note_local_to_remote_testing() {
    println!("Local→Remote transfers require DualTransport.");
    println!("Test via CLI: sy /local nick@fedora:/remote --per-file-progress");
}
