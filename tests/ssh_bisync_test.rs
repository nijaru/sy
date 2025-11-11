/// SSH bidirectional sync comprehensive tests (requires fedora)
///
/// Tests bidirectional sync functionality over SSH transport.
/// These scenarios were manually tested in v0.0.46-v0.0.48 but lacked
/// comprehensive automated testing.
///
/// **CRITICAL**: Bisync errors could cause data loss!
///
/// Run with: cargo test --test ssh_bisync_test -- --ignored
///
/// Prerequisites:
/// - fedora accessible via SSH
/// - sy-remote installed on fedora
/// - SSH keys configured
/// - Clean ~/.cache/sy/bisync/ directory
///
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::sleep;
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
    format!("/tmp/sy_bisync_test_{}_{}", test_name, std::process::id())
}

fn cleanup_remote_path(path: &str) {
    let cleanup_cmd = format!("ssh {} 'rm -rf {}'", FEDORA_HOST, path);
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cleanup_cmd)
        .output();
}

/// Helper to create a local file
fn create_local_file(path: &Path, content: &[u8]) {
    fs::write(path, content).expect("Failed to create local file");
}

/// Helper to read remote file content
fn read_remote_file(remote_path: &str) -> Vec<u8> {
    let read_cmd = format!("ssh {} 'cat {}'", FEDORA_HOST, remote_path);
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&read_cmd)
        .output()
        .expect("Failed to read remote file");

    if !output.status.success() {
        panic!(
            "Failed to read remote file: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    output.stdout
}

/// Helper to create remote file
fn create_remote_file(remote_path: &str, content: &[u8]) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("ssh {} 'cat > {}'", FEDORA_HOST, remote_path))
        .stdin(Stdio::piped())
        .spawn()
        .expect("Failed to spawn ssh");

    child.stdin.as_mut().unwrap().write_all(content).unwrap();
    let status = child.wait().expect("Failed to wait for ssh");

    if !status.success() {
        panic!("Failed to create remote file");
    }
}

// =============================================================================
// SECTION 1: Initial Sync (Baseline)
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_bisync_initial_local_to_remote() {
    use sy::bisync::{BisyncEngine, BisyncOptions};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("initial_l2r");

    // Create local files
    create_local_file(&local_dir.path().join("file1.txt"), b"content 1");
    create_local_file(&local_dir.path().join("file2.txt"), b"content 2");

    // Create remote directory
    let create_cmd = format!("ssh {} 'mkdir -p {}'", FEDORA_HOST, remote_dir);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote directory");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport, remote_transport);

    // Initial sync
    let result = engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    assert!(
        result.stats.files_synced_to_dest >= 2,
        "Should copy at least 2 files in initial sync"
    );

    // Verify remote files exist
    let file1_content = read_remote_file(&format!("{}/file1.txt", remote_dir));
    let file2_content = read_remote_file(&format!("{}/file2.txt", remote_dir));

    assert_eq!(file1_content, b"content 1");
    assert_eq!(file2_content, b"content 2");

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_initial_local_to_remote: PASS");
}

#[tokio::test]
#[ignore]
async fn test_bisync_initial_remote_to_local() {
    use sy::bisync::{BisyncEngine, BisyncOptions};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("initial_r2l");

    // Create remote files
    let create_cmd = format!(
        "ssh {} 'mkdir -p {} && echo \"remote 1\" > {}/file1.txt && echo \"remote 2\" > {}/file2.txt'",
        FEDORA_HOST, remote_dir, remote_dir, remote_dir
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote files");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport, remote_transport);

    // Initial sync
    let result = engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    assert!(
        result.stats.files_synced_to_source >= 2,
        "Should copy at least 2 files from remote"
    );

    // Verify local files
    let file1 = fs::read(local_dir.path().join("file1.txt")).unwrap();
    let file2 = fs::read(local_dir.path().join("file2.txt")).unwrap();

    assert_eq!(file1.trim_ascii(), b"remote 1");
    assert_eq!(file2.trim_ascii(), b"remote 2");

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_initial_remote_to_local: PASS");
}

// =============================================================================
// SECTION 2: Incremental Sync (No Conflicts)
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_bisync_add_file_to_local() {
    use sy::bisync::{BisyncEngine, BisyncOptions};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("add_local");

    // Setup: Initial sync with 1 file
    create_local_file(&local_dir.path().join("existing.txt"), b"exists");

    let create_cmd = format!("ssh {} 'mkdir -p {}'", FEDORA_HOST, remote_dir);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote directory");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport.clone(), remote_transport.clone());

    // Initial sync
    engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    // Add new file to local
    sleep(Duration::from_secs(1)); // Ensure mtime difference
    create_local_file(&local_dir.path().join("new.txt"), b"new content");

    // Second sync (recreate engine to clear state cache)
    let local_transport2 = Arc::new(LocalTransport::new());
    let remote_transport2 = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );
    let engine2 = BisyncEngine::new(local_transport2, remote_transport2);

    let result = engine2
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Second sync failed");

    assert!(
        result.stats.files_synced_to_dest >= 1,
        "Should copy at least 1 new file"
    );

    // Verify remote has new file
    let new_content = read_remote_file(&format!("{}/new.txt", remote_dir));
    assert_eq!(new_content, b"new content");

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_add_file_to_local: PASS");
}

#[tokio::test]
#[ignore]
async fn test_bisync_add_file_to_remote() {
    use sy::bisync::{BisyncEngine, BisyncOptions};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("add_remote");

    // Setup: Initial sync
    create_local_file(&local_dir.path().join("existing.txt"), b"exists");

    let create_cmd = format!("ssh {} 'mkdir -p {}'", FEDORA_HOST, remote_dir);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote directory");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport, remote_transport);

    engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    // Add new file to remote
    sleep(Duration::from_secs(1));
    create_remote_file(&format!("{}/remote_new.txt", remote_dir), b"remote new");

    // Second sync
    let local_transport2 = Arc::new(LocalTransport::new());
    let remote_transport2 = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );
    let engine2 = BisyncEngine::new(local_transport2, remote_transport2);

    let result = engine2
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Second sync failed");

    assert!(
        result.stats.files_synced_to_source >= 1,
        "Should copy at least 1 file from remote"
    );

    // Verify local has new file
    let local_new = fs::read(local_dir.path().join("remote_new.txt")).unwrap();
    assert_eq!(local_new, b"remote new");

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_add_file_to_remote: PASS");
}

#[tokio::test]
#[ignore]
async fn test_bisync_delete_from_local() {
    use sy::bisync::{BisyncEngine, BisyncOptions};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("delete_local");

    // Setup: Initial sync with 2 files
    create_local_file(&local_dir.path().join("keep.txt"), b"keep");
    create_local_file(&local_dir.path().join("delete.txt"), b"delete me");

    let create_cmd = format!("ssh {} 'mkdir -p {}'", FEDORA_HOST, remote_dir);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote directory");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport, remote_transport);

    engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    // Delete file locally
    fs::remove_file(local_dir.path().join("delete.txt")).unwrap();

    // Second sync
    let local_transport2 = Arc::new(LocalTransport::new());
    let remote_transport2 = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );
    let engine2 = BisyncEngine::new(local_transport2, remote_transport2);

    let result = engine2
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions {
                max_delete_percent: 0, // Allow all deletions for this test
                ..BisyncOptions::default()
            },
        )
        .await
        .expect("Second sync failed");

    assert!(
        result.stats.files_deleted_from_dest >= 1,
        "Should delete at least 1 file from remote"
    );

    // Verify remote file is gone
    let check_cmd = format!(
        "ssh {} 'test -f {}/delete.txt && echo exists || echo missing'",
        FEDORA_HOST, remote_dir
    );
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&check_cmd)
        .output()
        .expect("Failed to check remote file");

    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "missing");

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_delete_from_local: PASS");
}

// =============================================================================
// SECTION 3: Conflict Resolution
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_bisync_conflict_newer_wins() {
    use sy::bisync::{BisyncEngine, BisyncOptions, ConflictResolution};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("conflict_newer");

    // Setup: Initial sync
    create_local_file(&local_dir.path().join("conflict.txt"), b"original");

    let create_cmd = format!("ssh {} 'mkdir -p {}'", FEDORA_HOST, remote_dir);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote directory");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport, remote_transport);

    engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    // Modify locally (earlier timestamp)
    create_local_file(&local_dir.path().join("conflict.txt"), b"local version");

    // Wait and modify remote (later timestamp)
    sleep(Duration::from_secs(2));
    create_remote_file(
        &format!("{}/conflict.txt", remote_dir),
        b"remote version newer",
    );

    // Sync with Newer strategy
    let local_transport2 = Arc::new(LocalTransport::new());
    let remote_transport2 = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );
    let engine2 = BisyncEngine::new(local_transport2, remote_transport2);

    let opts = BisyncOptions {
        conflict_resolution: ConflictResolution::Newer,
        ..Default::default()
    };

    let result = engine2
        .sync(local_dir.path(), &PathBuf::from(&remote_dir), opts)
        .await
        .expect("Sync with conflict failed");

    assert!(
        result.stats.conflicts_resolved >= 1 || result.stats.files_synced_to_source >= 1,
        "Should resolve conflict or sync newer file"
    );

    // Newer (remote) should win
    let local_content = fs::read(local_dir.path().join("conflict.txt")).unwrap();
    assert_eq!(local_content, b"remote version newer");

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_conflict_newer_wins: PASS");
}

#[tokio::test]
#[ignore]
async fn test_bisync_conflict_larger_wins() {
    use sy::bisync::{BisyncEngine, BisyncOptions, ConflictResolution};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("conflict_larger");

    // Setup
    create_local_file(&local_dir.path().join("conflict.txt"), b"original");

    let create_cmd = format!("ssh {} 'mkdir -p {}'", FEDORA_HOST, remote_dir);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote directory");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport, remote_transport);

    engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    // Create conflict: local is smaller, remote is larger
    sleep(Duration::from_secs(1));
    create_local_file(&local_dir.path().join("conflict.txt"), b"small");
    create_remote_file(
        &format!("{}/conflict.txt", remote_dir),
        b"this is much larger content",
    );

    // Sync with Larger strategy
    let local_transport2 = Arc::new(LocalTransport::new());
    let remote_transport2 = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );
    let engine2 = BisyncEngine::new(local_transport2, remote_transport2);

    let opts = BisyncOptions {
        conflict_resolution: ConflictResolution::Larger,
        ..Default::default()
    };

    let result = engine2
        .sync(local_dir.path(), &PathBuf::from(&remote_dir), opts)
        .await
        .expect("Sync failed");

    assert!(
        result.stats.conflicts_resolved >= 1 || result.stats.files_synced_to_source >= 1,
        "Should resolve conflict or sync larger file"
    );

    // Larger (remote) should win
    let local_content = fs::read(local_dir.path().join("conflict.txt")).unwrap();
    assert_eq!(local_content, b"this is much larger content");

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_conflict_larger_wins: PASS");
}

// =============================================================================
// SECTION 4: Safety Features
// =============================================================================

#[tokio::test]
#[ignore]
async fn test_bisync_max_delete_limit() {
    use sy::bisync::{BisyncEngine, BisyncOptions};
    use sy::transport::local::LocalTransport;
    use sy::transport::ssh::SshTransport;

    let local_dir = TempDir::new().unwrap();
    let remote_dir = create_remote_test_path("max_delete");

    // Setup: Create 10 files
    for i in 0..10 {
        create_local_file(&local_dir.path().join(format!("file{}.txt", i)), b"content");
    }

    let create_cmd = format!("ssh {} 'mkdir -p {}'", FEDORA_HOST, remote_dir);
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&create_cmd)
        .output()
        .expect("Failed to create remote directory");

    let local_transport = Arc::new(LocalTransport::new());
    let remote_config = create_fedora_config();
    let remote_transport = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );

    let engine = BisyncEngine::new(local_transport, remote_transport);

    engine
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await
        .expect("Initial sync failed");

    // Delete 6 files (60% > 50% limit)
    for i in 0..6 {
        fs::remove_file(local_dir.path().join(format!("file{}.txt", i))).unwrap();
    }

    // Sync should fail due to max_delete limit
    let local_transport2 = Arc::new(LocalTransport::new());
    let remote_transport2 = Arc::new(
        SshTransport::new(&remote_config)
            .await
            .expect("Failed to connect"),
    );
    let engine2 = BisyncEngine::new(local_transport2, remote_transport2);

    let result = engine2
        .sync(
            local_dir.path(),
            &PathBuf::from(&remote_dir),
            BisyncOptions::default(),
        )
        .await;

    // Should error due to deletion safety
    assert!(
        result.is_err(),
        "Sync should fail when deletions exceed max_delete"
    );

    cleanup_remote_path(&remote_dir);
    println!("✅ bisync_max_delete_limit: PASS");
}

// =============================================================================
// Test Summary
// =============================================================================

#[test]
#[ignore]
fn print_bisync_test_summary() {
    println!("\n========================================");
    println!("SSH Bidirectional Sync Test Suite");
    println!("========================================\n");
    println!("SECTION 1: Initial Sync");
    println!("  - initial_local_to_remote");
    println!("  - initial_remote_to_local");
    println!("\nSECTION 2: Incremental Sync");
    println!("  - add_file_to_local");
    println!("  - add_file_to_remote");
    println!("  - delete_from_local");
    println!("\nSECTION 3: Conflict Resolution");
    println!("  - conflict_newer_wins");
    println!("  - conflict_larger_wins");
    println!("\nSECTION 4: Safety Features");
    println!("  - max_delete_limit");
    println!("\nTotal: 8 tests");
    println!("\nRun with:");
    println!("  cargo test --test ssh_bisync_test -- --ignored");
    println!("========================================\n");
}
