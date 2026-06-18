//! SSH sync tests — remote operations via CLI.
//!
//! Tests the full SyncSession → StreamingSync pipeline.
//! Requires SSH agent with fedora host configured.
//!
//! Run: cargo test --test sync_ssh -- --ignored

use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir(name: &str) -> (TempDir, TempDir) {
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

fn fedora_path(test_name: &str) -> String {
    format!("/tmp/sy-ssh-test-{}-{}", test_name, std::process::id())
}

fn cleanup_fedora(path: &str) {
    let _ = Command::new("ssh")
        .args(["fedora", &format!("rm -rf {}", path)])
        .output();
}

/// Sync single file to remote
#[test]
#[ignore]
fn test_ssh_push_single_file() {
    let (source, _dest) = setup_test_dir("ssh_push_single");
    let remote = fedora_path("push_single");
    cleanup_fedora(&remote);

    std::fs::write(source.path().join("file.txt"), "hello world").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify file exists on remote
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "hello world");

    cleanup_fedora(&remote);
}

/// Sync directory structure to remote
#[test]
#[ignore]
fn test_ssh_push_directory() {
    let (source, _dest) = setup_test_dir("ssh_push_dir");
    let remote = fedora_path("push_dir");
    cleanup_fedora(&remote);

    std::fs::create_dir_all(source.path().join("subdir")).unwrap();
    std::fs::write(source.path().join("file1.txt"), "file1").unwrap();
    std::fs::write(source.path().join("subdir/file2.txt"), "file2").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify files exist on remote
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file1.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "file1");

    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/subdir/file2.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "file2");

    cleanup_fedora(&remote);
}

/// Pull from remote to local
#[test]
#[ignore]
fn test_ssh_pull() {
    let (_source, dest) = setup_test_dir("ssh_pull");
    let remote = fedora_path("pull");
    cleanup_fedora(&remote);

    // Create files on remote
    let _ = Command::new("ssh")
        .args(["fedora", &format!("mkdir -p {}", remote)])
        .output();
    let _ = Command::new("ssh")
        .args(["fedora", &format!("echo 'remote file' > {}/file.txt", remote)])
        .output();

    let output = Command::new(sy_bin())
        .args([
            &format!("fedora:{}/", remote),
            &format!("{}/", dest.path().display()),
            "--exclude-vcs",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify file exists locally
    let content = std::fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(content.trim(), "remote file");

    cleanup_fedora(&remote);
}

/// Incremental sync — only changed files transferred
#[test]
#[ignore]
fn test_ssh_incremental() {
    let (source, _dest) = setup_test_dir("ssh_incremental");
    let remote = fedora_path("incremental");
    cleanup_fedora(&remote);

    // Initial sync
    std::fs::write(source.path().join("file1.txt"), "original").unwrap();
    std::fs::write(source.path().join("file2.txt"), "unchanged").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Sleep to ensure mtime changes (protocol truncates to seconds)
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Modify only file1
    std::fs::write(source.path().join("file1.txt"), "modified").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Verify file1 was updated
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file1.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "modified");

    // Verify file2 unchanged
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file2.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "unchanged");

    cleanup_fedora(&remote);
}

/// Delta sync — large file with small change
#[test]
#[ignore]
fn test_ssh_delta_sync() {
    let (source, _dest) = setup_test_dir("ssh_delta");
    let remote = fedora_path("delta");
    cleanup_fedora(&remote);

    // Create 1MB file
    let data = vec![0u8; 1024 * 1024];
    std::fs::write(source.path().join("large.bin"), &data).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Sleep to ensure mtime changes (protocol truncates to seconds)
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Modify small portion
    let mut modified = data;
    modified[0] = 1;
    modified[1000] = 2;
    std::fs::write(source.path().join("large.bin"), &modified).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Verify file was updated correctly
    let check = Command::new("ssh")
        .args(["fedora", &format!("md5sum {}/large.bin", remote)])
        .output()
        .unwrap();
    let remote_md5 = String::from_utf8_lossy(&check.stdout).split_whitespace().next().unwrap().to_string();

    let local_md5 = {
        let output = Command::new("md5")
            .arg("-q")
            .arg(source.path().join("large.bin"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    assert_eq!(local_md5, remote_md5, "File content mismatch after delta sync");

    cleanup_fedora(&remote);
}

/// Delete mode — remove files from dest that don't exist in source
#[test]
#[ignore]
fn test_ssh_delete() {
    let (source, _dest) = setup_test_dir("ssh_delete");
    let remote = fedora_path("delete");
    cleanup_fedora(&remote);

    // Create initial files
    std::fs::write(source.path().join("keep.txt"), "keep").unwrap();
    std::fs::write(source.path().join("remove.txt"), "remove").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--delete",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Remove file from source
    std::fs::remove_file(source.path().join("remove.txt")).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--delete",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Verify file was deleted from remote
    let check = Command::new("ssh")
        .args(["fedora", &format!("test -f {}/remove.txt && echo exists || echo gone", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "gone");

    // Verify kept file still exists
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/keep.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "keep");

    cleanup_fedora(&remote);
}

/// Dry run — no changes made
#[test]
#[ignore]
fn test_ssh_dry_run() {
    let (source, _dest) = setup_test_dir("ssh_dry_run");
    let remote = fedora_path("dry_run");
    cleanup_fedora(&remote);

    std::fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--dry-run",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success());

    // Verify no files were created on remote
    let check = Command::new("ssh")
        .args(["fedora", &format!("test -d {} && echo DIR_EXISTS || echo DIR_NOT_FOUND", remote)])
        .output()
        .unwrap();
    let dir_status = String::from_utf8_lossy(&check.stdout).trim().to_string();
    
    if dir_status == "DIR_EXISTS" {
        // Directory exists, check if it's empty
        let check = Command::new("ssh")
            .args(["fedora", &format!("ls -A {}", remote)])
            .output()
            .unwrap();
        let contents = String::from_utf8_lossy(&check.stdout).trim().to_string();
        assert!(contents.is_empty(), "Dry run should not create files, found: '{}'", contents);
    }
    // If directory doesn't exist, that's fine too

    cleanup_fedora(&remote);
}

/// Symlink sync
#[test]
#[ignore]
fn test_ssh_symlink() {
    let (source, _dest) = setup_test_dir("ssh_symlink");
    let remote = fedora_path("symlink");
    cleanup_fedora(&remote);

    std::fs::write(source.path().join("target.txt"), "target content").unwrap();
    std::os::unix::fs::symlink("target.txt", source.path().join("link.txt")).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify symlink exists and points to correct target
    let check = Command::new("ssh")
        .args(["fedora", &format!("readlink {}/link.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "target.txt");

    // Verify symlink resolves
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/link.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "target content");

    cleanup_fedora(&remote);
}

/// Compress mode (-z)
#[test]
#[ignore]
fn test_ssh_compress() {
    let (source, _dest) = setup_test_dir("ssh_compress");
    let remote = fedora_path("compress");
    cleanup_fedora(&remote);

    // Create compressible data
    let data = "hello world ".repeat(10000);
    std::fs::write(source.path().join("compressible.txt"), &data).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "-z",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify content matches
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/compressible.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout), data);

    cleanup_fedora(&remote);
}

/// Max delete threshold
/// Note: Deletion threshold is not yet enforced in SSH streaming mode.
/// This test verifies the sync completes but documents the limitation.
#[test]
#[ignore]
fn test_ssh_max_delete() {
    let (source, _dest) = setup_test_dir("ssh_max_delete");
    let remote = fedora_path("max_delete");
    cleanup_fedora(&remote);

    // Create 10 files
    for i in 0..10 {
        std::fs::write(source.path().join(format!("file{}.txt", i)), format!("content{}", i)).unwrap();
    }

    // Initial sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--delete",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Remove 9 files (90% deletion)
    for i in 1..10 {
        std::fs::remove_file(source.path().join(format!("file{}.txt", i))).unwrap();
    }

    // With max-delete=50%, should fail because 90% deletion exceeds threshold
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--delete",
            "--max-delete=50%",
        ])
        .output()
        .expect("Failed to run sy");

    // Should fail — threshold exceeded
    assert!(!output.status.success(), "SSH sync should fail when max-delete threshold exceeded, got success. stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("threshold") || stderr.contains("max-delete"), "Error should mention threshold");

    // With --force-delete, should succeed despite threshold
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--delete",
            "--max-delete=50%",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success(), "SSH sync should succeed with --force-delete");

    cleanup_fedora(&remote);
}

/// Exclude filter
#[test]
#[ignore]
fn test_ssh_exclude() {
    let (source, _dest) = setup_test_dir("ssh_exclude");
    let remote = fedora_path("exclude");
    cleanup_fedora(&remote);

    std::fs::write(source.path().join("keep.txt"), "keep").unwrap();
    std::fs::write(source.path().join("skip.log"), "skip").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--exclude=*.log",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify keep.txt exists
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/keep.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "keep");

    // Verify skip.log does NOT exist
    let check = Command::new("ssh")
        .args(["fedora", &format!("test -f {}/skip.log && echo exists || echo gone", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "gone");

    cleanup_fedora(&remote);
}

/// Special characters in filenames
#[test]
#[ignore]
fn test_ssh_special_chars() {
    let (source, _dest) = setup_test_dir("ssh_special");
    let remote = fedora_path("special");
    cleanup_fedora(&remote);

    std::fs::write(source.path().join("file with spaces.txt"), "spaces").unwrap();
    std::fs::write(source.path().join("file-with-dashes.txt"), "dashes").unwrap();
    std::fs::write(source.path().join("file.with.dots.txt"), "dots").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify all files exist
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat '{}/file with spaces.txt'", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "spaces");

    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file-with-dashes.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "dashes");

    cleanup_fedora(&remote);
}

/// Empty file sync
#[test]
#[ignore]
fn test_ssh_empty_file() {
    let (source, _dest) = setup_test_dir("ssh_empty");
    let remote = fedora_path("empty");
    cleanup_fedora(&remote);

    std::fs::write(source.path().join("empty.txt"), "").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify empty file exists on remote
    let check = Command::new("ssh")
        .args(["fedora", &format!("test -f {}/empty.txt && echo exists || echo gone", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "exists");

    // Verify file is actually empty
    let check = Command::new("ssh")
        .args(["fedora", &format!("wc -c < {}/empty.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "0");

    cleanup_fedora(&remote);
}

/// Idempotent sync — running twice produces same result
#[test]
#[ignore]
fn test_ssh_idempotent() {
    let (source, _dest) = setup_test_dir("ssh_idempotent");
    let remote = fedora_path("idempotent");
    cleanup_fedora(&remote);

    std::fs::write(source.path().join("file.txt"), "content").unwrap();

    // First sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Second sync — should be no-op
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");
    assert!(output.status.success());

    // Verify file still correct
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "content");

    cleanup_fedora(&remote);
}


#[test]
#[ignore]
fn test_ssh_preserve_permissions() {
    let (source, _dest) = setup_test_dir("ssh_perms");
    let remote = fedora_path("perms");
    cleanup_fedora(&remote);

    // Create files with specific permissions
    std::fs::write(source.path().join("script.sh"), "#!/bin/bash\necho hello").unwrap();
    std::fs::set_permissions(
        source.path().join("script.sh"),
        std::fs::Permissions::from_mode(0o755),
    ).unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--preserve-permissions",
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify permissions were propagated
    let check = Command::new("ssh")
        .args(["fedora", &format!("stat -c %a {}/script.sh", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "755");

    cleanup_fedora(&remote);
}

#[test]
#[ignore]
fn test_ssh_large_file() {
    let (source, _dest) = setup_test_dir("ssh_large");
    let remote = fedora_path("large");
    cleanup_fedora(&remote);

    // Create 100MB file
    let large_content = vec![0xAB_u8; 100 * 1024 * 1024];
    std::fs::write(source.path().join("large.bin"), &large_content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Check size on remote
    let check = Command::new("ssh")
        .args(["fedora", &format!("stat -c %s {}/large.bin", remote)])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&check.stdout).trim(),
        (100 * 1024 * 1024).to_string()
    );

    cleanup_fedora(&remote);
}

#[test]
#[ignore]
fn test_ssh_many_files() {
    let (source, _dest) = setup_test_dir("ssh_many");
    let remote = fedora_path("many");
    cleanup_fedora(&remote);

    // Create 1000 files
    for i in 0..1000 {
        std::fs::write(
            source.path().join(format!("file_{:04}.txt", i)),
            format!("content_{}", i),
        ).unwrap();
    }

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Verify count on remote
    let check = Command::new("ssh")
        .args(["fedora", &format!("ls {} | wc -l", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "1000");

    // Spot check a few files
    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file_0000.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "content_0");

    let check = Command::new("ssh")
        .args(["fedora", &format!("cat {}/file_0999.txt", remote)])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "content_999");

    cleanup_fedora(&remote);
}

#[test]
#[ignore]
fn test_ssh_include_filter() {
    let (source, _dest) = setup_test_dir("ssh_include");
    let remote = fedora_path("include");
    cleanup_fedora(&remote);

    // Create mixed files
    std::fs::write(source.path().join("include.rs"), "fn main() {}").unwrap();
    std::fs::write(source.path().join("include.txt"), "text").unwrap();
    std::fs::write(source.path().join("exclude.log"), "log data").unwrap();
    std::fs::write(source.path().join("exclude.tmp"), "temp").unwrap();

    // Sync with include filter (only .rs and .txt files)
    let output = Command::new(sy_bin())
        .args([
            "--include", "*.rs",
            "--include", "*.txt",
            "--exclude", "*",
            &format!("{}/", source.path().display()),
            &format!("fedora:{}/", remote),
            "--exclude-vcs",
            "--force-delete",
        ])
        .output()
        .expect("Failed to run sy");

    assert!(output.status.success(), "sy failed: {}", String::from_utf8_lossy(&output.stderr));

    // Should have .rs and .txt
    let check = Command::new("ssh")
        .args(["fedora", &format!("ls {}", remote)])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&check.stdout);
    assert!(listing.contains("include.rs"));
    assert!(listing.contains("include.txt"));

    // Should NOT have .log or .tmp
    assert!(!listing.contains("exclude.log"));
    assert!(!listing.contains("exclude.tmp"));

    cleanup_fedora(&remote);
}
