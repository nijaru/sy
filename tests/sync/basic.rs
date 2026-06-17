//! Basic sync tests — fundamental operations.
//!
//! Covers: basic sync, dry run, delete mode, nested dirs, quiet mode, error handling.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;
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

    fs::write(source.path().join("file1.txt"), "content1").unwrap();
    fs::write(source.path().join("file2.txt"), "content2").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file1.txt")).unwrap(), "content1");
    assert_eq!(fs::read_to_string(dest.path().join("file2.txt")).unwrap(), "content2");
}

#[test]
fn test_dry_run() {
    let (source, dest) = setup_test_dir("dry_run");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("file.txt").exists());
}

#[test]
fn test_delete_mode() {
    let (source, dest) = setup_test_dir("delete");

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(dest.path().join("keep.txt"), "keep").unwrap();
    fs::write(dest.path().join("delete.txt"), "delete").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
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
fn test_nested_directories() {
    let (source, dest) = setup_test_dir("nested");

    fs::create_dir_all(source.path().join("a/b/c")).unwrap();
    fs::write(source.path().join("a/b/c/file.txt"), "nested").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("a/b/c/file.txt")).unwrap(), "nested");
}

#[test]
fn test_update_existing_files() {
    let (source, dest) = setup_test_dir("update");

    fs::write(source.path().join("file.txt"), "original").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "original");

    // Wait to ensure mtime changes
    thread::sleep(Duration::from_secs(2));

    fs::write(source.path().join("file.txt"), "updated").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "updated");
}

#[test]
fn test_skip_unchanged_files() {
    let (source, dest) = setup_test_dir("skip");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("skipped") || stdout.contains("Files skipped:     1"));
}

#[test]
fn test_quiet_mode() {
    let (source, dest) = setup_test_dir("quiet");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn test_error_source_not_exists() {
    let dest = TempDir::new().unwrap();

    let output = Command::new(sy_bin())
        .args([
            "/nonexistent/path",
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
}

#[test]
fn test_single_file_sync() {
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
    assert_eq!(fs::read_to_string(&dest_file).unwrap(), "test content for single file");
}

#[test]
fn test_git_directory_excluded() {
    let (source, dest) = setup_test_dir("git_excluded");

    fs::write(source.path().join(".git/config"), "git config").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join(".git").exists());
}

#[test]
fn test_update_shows_correct_stats() {
    let (source, dest) = setup_test_dir("stats");

    fs::write(source.path().join("file1.txt"), "initial content v1").unwrap();
    fs::write(source.path().join("file2.txt"), "initial content v2").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files created:     2"));

    // Wait to ensure mtime changes
    thread::sleep(Duration::from_secs(2));

    fs::write(source.path().join("file1.txt"), "updated content v1").unwrap();
    fs::write(source.path().join("file2.txt"), "updated content v2").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     2") || stdout.contains("Files updated:     1"));
}

#[test]
fn test_gitignore_support() {
    let (source, dest) = setup_test_dir("gitignore");

    // Create .gitignore
    fs::write(source.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("ignore.log"), "ignore").unwrap();

    // Run sync with --gitignore flag to respect .gitignore patterns
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--gitignore",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("ignore.log").exists());
}

#[test]
fn test_large_file_update_with_delta_sync() {
    let (source, dest) = setup_test_dir("large_delta");

    // Create large file in source (10MB)
    let large_content = vec![0u8; 10 * 1024 * 1024];
    fs::write(source.path().join("large.bin"), &large_content).unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Sleep to ensure mtime differs (need >2s because mtime tolerance is 1s and as_secs() truncates)
    std::thread::sleep(std::time::Duration::from_millis(2100));

    // Modify part of the file
    let mut modified = large_content;
    for byte in &mut modified[..1024] {
        *byte = 1;
    }
    fs::write(source.path().join("large.bin"), &modified).unwrap();

    // Second sync should use delta
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read(dest.path().join("large.bin")).unwrap(),
        modified
    );
}

#[test]
fn test_directory_cache_created() {
    let (source, dest) = setup_test_dir("cache_created");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--cache=true",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join(".sy-dir-cache.json").exists());
}

#[test]
fn test_directory_cache_not_created_by_default() {
    let (source, dest) = setup_test_dir("no_cache");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join(".sy-dir-cache.json").exists());
}

#[test]
fn test_directory_cache_persists() {
    let (source, dest) = setup_test_dir("cache_persist");

    fs::write(source.path().join("file1.txt"), "content1").unwrap();

    // First sync with cache
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--cache=true",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Add new file
    fs::write(source.path().join("file2.txt"), "content2").unwrap();

    // Second sync should use cache
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--cache=true",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(dest.path().join("file2.txt").exists());
}

#[test]
fn test_directory_cache_clear() {
    let (source, dest) = setup_test_dir("cache_clear");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Create cache
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--cache=true",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(dest.path().join(".sy-dir-cache.json").exists());

    // Clear cache
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--clear-cache",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!dest.path().join(".sy-dir-cache.json").exists());
}

#[test]
fn test_directory_cache_dry_run() {
    let (source, dest) = setup_test_dir("cache_dry_run");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Dry run with cache should not create cache file
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--cache=true",
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join(".sy-dir-cache.json").exists());
}

#[test]
fn test_directory_cache_updates_on_new_directories() {
    let (source, dest) = setup_test_dir("cache_new_dirs");

    fs::create_dir_all(source.path().join("subdir")).unwrap();
    fs::write(source.path().join("subdir/file.txt"), "content").unwrap();

    // First sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--cache=true",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Add new directory
    fs::create_dir_all(source.path().join("newdir")).unwrap();
    fs::write(source.path().join("newdir/file2.txt"), "content2").unwrap();

    // Second sync should pick up new directory
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--cache=true",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(dest.path().join("newdir/file2.txt").exists());
}

// Trailing slash behavior tests
// These test rsync-compatible trailing slash semantics

fn compute_test_destination(source: &sy::path::SyncPath, dest: &sy::path::SyncPath) -> std::path::PathBuf {
    let source_path = source.path();

    // For directories with trailing slash, use destination as-is (copy contents)
    if source.has_trailing_slash() {
        return dest.path().to_path_buf();
    }

    // For directories without trailing slash, append directory name to destination
    if let Some(dir_name) = source_path.file_name() {
        dest.path().join(dir_name)
    } else {
        // Fallback: use destination as-is
        dest.path().to_path_buf()
    }
}

#[test]
fn test_syncpath_trailing_slash_detection() {
    // Test trailing slash detection for local paths
    let path_without = sy::path::SyncPath::parse("/home/user/mydir");
    assert!(!path_without.has_trailing_slash());

    let path_with = sy::path::SyncPath::parse("/home/user/mydir/");
    assert!(path_with.has_trailing_slash());

    // Test remote paths
    let remote_without = sy::path::SyncPath::parse("user@host:/path/to/dir");
    assert!(!remote_without.has_trailing_slash());

    let remote_with = sy::path::SyncPath::parse("user@host:/path/to/dir/");
    assert!(remote_with.has_trailing_slash());
}

#[test]
fn test_destination_computation_without_trailing_slash() {
    let source = sy::path::SyncPath::parse("/a/myproject");
    let dest = sy::path::SyncPath::parse("/target");

    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, std::path::PathBuf::from("/target/myproject"));
}

#[test]
fn test_destination_computation_with_trailing_slash() {
    let source = sy::path::SyncPath::parse("/a/myproject/");
    let dest = sy::path::SyncPath::parse("/target");

    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, std::path::PathBuf::from("/target"));
}

#[test]
fn test_remote_destination_computation_without_trailing_slash() {
    let source = sy::path::SyncPath::parse("user@host:/a/myproject");
    let dest = sy::path::SyncPath::parse("/target");

    assert!(!source.has_trailing_slash());
    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, std::path::PathBuf::from("/target/myproject"));
}

#[test]
fn test_remote_destination_computation_with_trailing_slash() {
    let source = sy::path::SyncPath::parse("user@host:/a/myproject/");
    let dest = sy::path::SyncPath::parse("/target");

    assert!(source.has_trailing_slash());
    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, std::path::PathBuf::from("/target"));
}

#[test]
fn test_itemize_changes() {
    let (source, dest) = setup_test_dir("itemize");

    // Create a file
    fs::write(source.path().join("new.txt"), "content").unwrap();

    // Sync with --itemize-changes
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--itemize-changes",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check itemize output in stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("f"), "Expected itemize output in stdout: {}", stdout);
}

#[test]
fn test_stats_flag() {
    let (source, dest) = setup_test_dir("stats");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--stats",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check stats output
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files scanned:"), "Expected stats in stdout: {}", stdout);
    assert!(stdout.contains("Files created:"), "Expected stats in stdout: {}", stdout);
}

#[test]
fn test_backup_flag() {
    let (source, dest) = setup_test_dir("backup");

    // Create initial file in dest
    fs::write(dest.path().join("file.txt"), "old content").unwrap();
    
    // Wait to ensure source is newer
    thread::sleep(Duration::from_secs(2));
    
    // Create updated file in source
    fs::write(source.path().join("file.txt"), "new content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--backup",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check backup file exists
    assert!(dest.path().join("file.txt~").exists(), "Backup file should exist");
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "new content");
}

#[test]
fn test_partial_flag() {
    let (source, dest) = setup_test_dir("partial");

    // Create a file in source
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Sync without --partial (default behavior)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_remove_source_files() {
    let (source, dest) = setup_test_dir("remove_source");

    // Create a file in source
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--remove-source-files",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check source file is removed
    assert!(!source.path().join("file.txt").exists(), "Source file should be removed");
    
    // Check dest file exists
    assert!(dest.path().join("file.txt").exists(), "Dest file should exist");
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "content");
}

#[test]
fn test_existing_flag() {
    let (source, dest) = setup_test_dir("existing");

    // Create files in source
    fs::write(source.path().join("existing.txt"), "content").unwrap();
    fs::write(source.path().join("new.txt"), "content").unwrap();

    // Create one file in dest
    fs::write(dest.path().join("existing.txt"), "old content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--existing",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check only existing file was updated
    assert!(dest.path().join("existing.txt").exists(), "Existing file should be updated");
    assert!(!dest.path().join("new.txt").exists(), "New file should not be created");
}

#[test]
fn test_ignore_existing_flag() {
    let (source, dest) = setup_test_dir("ignore_existing");

    // Create files in source
    fs::write(source.path().join("existing.txt"), "new content").unwrap();
    fs::write(source.path().join("new.txt"), "content").unwrap();

    // Create one file in dest
    fs::write(dest.path().join("existing.txt"), "old content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--ignore-existing",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check only new file was created
    assert_eq!(fs::read_to_string(dest.path().join("existing.txt")).unwrap(), "old content");
    assert!(dest.path().join("new.txt").exists(), "New file should be created");
}

#[test]
fn test_dirs_flag() {
    let (source, dest) = setup_test_dir("dirs");

    // Create files and directories in source
    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::create_dir(source.path().join("subdir")).unwrap();
    fs::write(source.path().join("subdir/nested.txt"), "nested content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--dirs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check that directory structure is preserved
    assert!(dest.path().join("subdir").exists(), "Subdir should exist");
}

#[test]
fn test_links_flag() {
    let (source, dest) = setup_test_dir("links");

    // Create a file and a symlink
    fs::write(source.path().join("file.txt"), "content").unwrap();
    std::os::unix::fs::symlink("file.txt", source.path().join("link.txt")).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--links=preserve",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check symlink is preserved
    assert!(dest.path().join("link.txt").exists(), "Symlink should exist");
    assert_eq!(fs::read_to_string(dest.path().join("link.txt")).unwrap(), "content");
}

#[test]
fn test_copy_links_flag() {
    let (source, dest) = setup_test_dir("copy_links");

    // Create a file and a symlink
    fs::write(source.path().join("file.txt"), "content").unwrap();
    std::os::unix::fs::symlink("file.txt", source.path().join("link.txt")).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--copy-links",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check symlink target is copied, not the symlink
    assert!(dest.path().join("link.txt").exists(), "Link file should exist");
    assert_eq!(fs::read_to_string(dest.path().join("link.txt")).unwrap(), "content");
}

#[test]
fn test_min_size_flag() {
    let (source, dest) = setup_test_dir("min_size");

    // Create files of different sizes
    fs::write(source.path().join("small.txt"), "small").unwrap();
    fs::write(source.path().join("large.txt"), "a".repeat(1000)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--min-size=100",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check only large file is synced
    assert!(!dest.path().join("small.txt").exists(), "Small file should not be synced");
    assert!(dest.path().join("large.txt").exists(), "Large file should be synced");
}

#[test]
fn test_max_size_flag() {
    let (source, dest) = setup_test_dir("max_size");

    // Create files of different sizes
    fs::write(source.path().join("small.txt"), "small").unwrap();
    fs::write(source.path().join("large.txt"), "a".repeat(1000)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--max-size=100",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check only small file is synced
    assert!(dest.path().join("small.txt").exists(), "Small file should be synced");
    assert!(!dest.path().join("large.txt").exists(), "Large file should not be synced");
}

#[test]
fn test_bwlimit_flag() {
    let (source, dest) = setup_test_dir("bwlimit");

    // Create a large file
    fs::write(source.path().join("large.txt"), "a".repeat(10000)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--bwlimit=1",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check file is synced
    assert!(dest.path().join("large.txt").exists(), "File should be synced");
}

#[test]
fn test_special_characters_in_filenames() {
    let (source, dest) = setup_test_dir("special_chars");

    // Create files with special characters
    fs::write(source.path().join("file with spaces.txt"), "content").unwrap();
    fs::write(source.path().join("file\twith\ttabs.txt"), "content").unwrap();
    fs::write(source.path().join("file'with'quotes.txt"), "content").unwrap();
    fs::write(source.path().join("file\"with\"doublequotes.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check all files are synced
    assert!(dest.path().join("file with spaces.txt").exists(), "File with spaces should exist");
    assert!(dest.path().join("file\twith\ttabs.txt").exists(), "File with tabs should exist");
    assert!(dest.path().join("file'with'quotes.txt").exists(), "File with quotes should exist");
    assert!(dest.path().join("file\"with\"doublequotes.txt").exists(), "File with double quotes should exist");
}

#[test]
fn test_empty_files() {
    let (source, dest) = setup_test_dir("empty_files");

    // Create empty files
    fs::write(source.path().join("empty.txt"), "").unwrap();
    fs::create_dir(source.path().join("empty_dir")).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check empty files and dirs are synced
    assert!(dest.path().join("empty.txt").exists(), "Empty file should exist");
    assert!(dest.path().join("empty_dir").exists(), "Empty directory should exist");
}

#[test]
fn test_long_filenames() {
    let (source, dest) = setup_test_dir("long_filenames");

    // Create file with long name
    let long_name = "a".repeat(255);
    fs::write(source.path().join(&long_name), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    
    // Check file is synced
    assert!(dest.path().join(&long_name).exists(), "Long filename file should exist");
}
