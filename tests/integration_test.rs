use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir(_name: &str) -> (TempDir, TempDir) {
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
fn test_basic_sync() {
    let (source, dest) = setup_test_dir("basic");

    // Create test files
    fs::write(source.path().join("file1.txt"), "content1").unwrap();
    fs::write(source.path().join("file2.txt"), "content2").unwrap();

    // Run sync
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file1.txt").exists());
    assert!(dest.path().join("file2.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("file1.txt")).unwrap(),
        "content1"
    );
}

#[test]
fn test_dry_run() {
    let (source, dest) = setup_test_dir("dry_run");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Run dry-run
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("file.txt").exists());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dry-run"));
}

#[test]
fn test_delete_mode() {
    let (source, dest) = setup_test_dir("delete");

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(dest.path().join("keep.txt"), "keep").unwrap();
    fs::write(dest.path().join("delete.txt"), "delete").unwrap();

    // Run with --delete
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
            "--delete",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("delete.txt").exists());
}

#[test]
fn test_gitignore_support() {
    let (source, dest) = setup_test_dir("gitignore");

    // Create .gitignore
    fs::write(source.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("ignore.log"), "ignore").unwrap();

    // Run sync
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("ignore.log").exists());
    assert!(dest.path().join(".gitignore").exists());
}

#[test]
fn test_nested_directories() {
    let (source, dest) = setup_test_dir("nested");

    // Create nested structure
    fs::create_dir_all(source.path().join("dir1/dir2/dir3")).unwrap();
    fs::write(source.path().join("dir1/file1.txt"), "content1").unwrap();
    fs::write(source.path().join("dir1/dir2/file2.txt"), "content2").unwrap();
    fs::write(source.path().join("dir1/dir2/dir3/file3.txt"), "content3").unwrap();

    // Run sync
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("dir1/file1.txt").exists());
    assert!(dest.path().join("dir1/dir2/file2.txt").exists());
    assert!(dest.path().join("dir1/dir2/dir3/file3.txt").exists());
}

#[test]
fn test_update_existing_files() {
    let (source, dest) = setup_test_dir("update");

    // Initial sync
    fs::write(source.path().join("file.txt"), "v1").unwrap();
    Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "v1"
    );

    // Wait to ensure mtime changes (mtime has 1s tolerance)
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Update source file
    fs::write(source.path().join("file.txt"), "v2").unwrap();

    // Sync again
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "v2"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_skip_unchanged_files() {
    let (source, dest) = setup_test_dir("skip");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // First sync
    Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // Second sync (should skip)
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_quiet_mode() {
    let (source, dest) = setup_test_dir("quiet");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should have minimal output in quiet mode
    assert!(!stdout.contains("sy v"));
}

#[test]
fn test_error_source_not_exists() {
    let dest = TempDir::new().unwrap();

    let output = Command::new(sy_bin())
        .args(["/nonexistent/path", dest.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does not exist"));
}

#[tokio::test]
async fn test_single_file_sync() {
    let temp = TempDir::new().unwrap();
    let file_path = temp.path().join("file.txt");
    fs::write(&file_path, "test content for single file").unwrap();

    let dest_file = temp.path().join("dest.txt");

    let output = Command::new(sy_bin())
        .args([file_path.to_str().unwrap(), dest_file.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dest_file.exists());
    assert_eq!(
        fs::read_to_string(&dest_file).unwrap(),
        "test content for single file"
    );
}

#[test]
fn test_git_directory_excluded() {
    let (source, dest) = setup_test_dir("git_exclude");

    // Git repo already initialized by setup
    // Add a file in .git
    fs::write(source.path().join(".git/config"), "test").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join(".git").exists());
}

#[test]
fn test_update_shows_correct_stats() {
    let (source, dest) = setup_test_dir("update_stats");

    // Create initial files
    fs::write(source.path().join("file1.txt"), "initial content v1").unwrap();
    fs::write(source.path().join("file2.txt"), "initial content v2").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files created:     2"));

    // Wait to ensure mtime changes (1s tolerance)
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Modify files
    fs::write(source.path().join("file1.txt"), "updated content v1").unwrap();
    fs::write(source.path().join("file2.txt"), "updated content v2").unwrap();

    // Sync again - should show updates
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify update stats
    assert!(stdout.contains("Files updated:     2"));
    assert!(stdout.contains("Files skipped:     0"));

    // Verify files were actually updated
    assert_eq!(
        fs::read_to_string(dest.path().join("file1.txt")).unwrap(),
        "updated content v1"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file2.txt")).unwrap(),
        "updated content v2"
    );
}

#[test]
#[ignore] // Slow test - requires 2GB file creation and sync
fn test_large_file_update_with_delta_sync() {
    let (source, dest) = setup_test_dir("delta_sync");

    // Create a large file (2GB) to trigger local delta sync
    // Using sparse file for speed - only allocates actual written blocks
    let large_file = source.path().join("large.bin");
    let file = fs::File::create(&large_file).unwrap();
    file.set_len(2 * 1024 * 1024 * 1024).unwrap(); // 2GB
    drop(file);

    // Write some actual data at the beginning
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(&large_file)
        .unwrap();
    file.write_all(b"START OF FILE").unwrap();
    file.seek(SeekFrom::End(-13)).unwrap();
    file.write_all(b"END OF FILE!!").unwrap();
    drop(file);

    // Initial sync
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("large.bin").exists());

    // Wait for mtime to change
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Modify the file slightly (change just the beginning)
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(&large_file)
        .unwrap();
    file.write_all(b"MODIFIED FILE").unwrap();
    drop(file);

    // Sync again - should use delta sync
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Delta sync should be used for large files
    // The output should mention delta sync
    assert!(stdout.contains("Files updated:     1"));

    // If delta sync was used, it should appear in summary
    // Note: Delta sync only triggers for files >1GB on local, and this is >2GB
    if stdout.contains("Delta sync:") {
        // Verify delta sync stats are shown
        assert!(stdout.contains("1 files"));
    }

    // Verify the file was updated correctly
    let dest_file = dest.path().join("large.bin");
    let mut file = fs::File::open(&dest_file).unwrap();
    let mut buf = [0u8; 13];
    file.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"MODIFIED FILE");
}
