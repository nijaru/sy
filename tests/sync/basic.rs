//! Basic sync tests — fundamental operations.
//!
//! Covers: basic sync, dry run, delete mode, nested dirs, quiet mode, error handling.

use std::fs;
use std::os::unix::fs::PermissionsExt;
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
    assert_eq!(
        fs::read_to_string(dest.path().join("file1.txt")).unwrap(),
        "content1"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file2.txt")).unwrap(),
        "content2"
    );
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
    assert_eq!(
        fs::read_to_string(dest.path().join("a/b/c/file.txt")).unwrap(),
        "nested"
    );
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
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "original"
    );

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
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "updated"
    );
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
        .args(["/nonexistent/path", dest.path().to_str().unwrap()])
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
    assert_eq!(
        fs::read_to_string(&dest_file).unwrap(),
        "test content for single file"
    );
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
    assert_eq!(fs::read(dest.path().join("large.bin")).unwrap(), modified);
}

#[test]
fn test_cache_flags_removed() {
    let (source, dest) = setup_test_dir("cache_removed");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // The directory cache never worked on the 0.5 engines and the flags are
    // removed outright; passing them must fail as unknown arguments.
    for flag in ["--cache", "--clear-cache"] {
        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
                flag,
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{flag} must be rejected as unknown"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unexpected argument"),
            "{flag} not rejected:\n{stderr}"
        );
    }
    // Nothing was synced by the rejected invocations.
    assert!(!dest.path().join("file.txt").exists());
}

// Trailing slash behavior tests
// These test rsync-compatible trailing slash semantics

fn compute_test_destination(
    source: &sy::path::SyncPath,
    dest: &sy::path::SyncPath,
) -> std::path::PathBuf {
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
    assert_eq!(
        effective_dest,
        std::path::PathBuf::from("/target/myproject")
    );
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
    assert_eq!(
        effective_dest,
        std::path::PathBuf::from("/target/myproject")
    );
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
    assert!(
        stdout.contains("f"),
        "Expected itemize output in stdout: {}",
        stdout
    );
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
    assert!(
        stdout.contains("Files scanned:"),
        "Expected stats in stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("Files created:"),
        "Expected stats in stdout: {}",
        stdout
    );
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
    assert!(
        dest.path().join("file.txt~").exists(),
        "Backup file should exist"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "new content"
    );
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
    assert!(
        !source.path().join("file.txt").exists(),
        "Source file should be removed"
    );

    // Check dest file exists
    assert!(
        dest.path().join("file.txt").exists(),
        "Dest file should exist"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "content"
    );
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
    assert!(
        dest.path().join("existing.txt").exists(),
        "Existing file should be updated"
    );
    assert!(
        !dest.path().join("new.txt").exists(),
        "New file should not be created"
    );
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
    assert_eq!(
        fs::read_to_string(dest.path().join("existing.txt")).unwrap(),
        "old content"
    );
    assert!(
        dest.path().join("new.txt").exists(),
        "New file should be created"
    );
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
    assert!(
        dest.path().join("link.txt").exists(),
        "Symlink should exist"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("link.txt")).unwrap(),
        "content"
    );
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
    assert!(
        dest.path().join("link.txt").exists(),
        "Link file should exist"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("link.txt")).unwrap(),
        "content"
    );
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
    assert!(
        !dest.path().join("small.txt").exists(),
        "Small file should not be synced"
    );
    assert!(
        dest.path().join("large.txt").exists(),
        "Large file should be synced"
    );
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
    assert!(
        dest.path().join("small.txt").exists(),
        "Small file should be synced"
    );
    assert!(
        !dest.path().join("large.txt").exists(),
        "Large file should not be synced"
    );
}

#[test]
fn test_bwlimit_flag() {
    let (source, dest) = setup_test_dir("bwlimit");

    // 30 KiB at a 60 KiB/s limit: the one-second burst covers the first
    // 60 KiB, so the transfer completes without a measurable pause while
    // still exercising the paced streaming path (native kernel copies are
    // bypassed when a limit is set).
    fs::write(source.path().join("large.txt"), "a".repeat(30 * 1024)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--bwlimit=60KB",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Check file is synced
    assert!(
        dest.path().join("large.txt").exists(),
        "File should be synced"
    );
}

#[test]
fn test_special_characters_in_filenames() {
    let (source, dest) = setup_test_dir("special_chars");

    // Create files with special characters
    fs::write(source.path().join("file with spaces.txt"), "content").unwrap();
    fs::write(source.path().join("file\twith\ttabs.txt"), "content").unwrap();
    fs::write(source.path().join("file'with'quotes.txt"), "content").unwrap();
    fs::write(
        source.path().join("file\"with\"doublequotes.txt"),
        "content",
    )
    .unwrap();

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
    assert!(
        dest.path().join("file with spaces.txt").exists(),
        "File with spaces should exist"
    );
    assert!(
        dest.path().join("file\twith\ttabs.txt").exists(),
        "File with tabs should exist"
    );
    assert!(
        dest.path().join("file'with'quotes.txt").exists(),
        "File with quotes should exist"
    );
    assert!(
        dest.path().join("file\"with\"doublequotes.txt").exists(),
        "File with double quotes should exist"
    );
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
    assert!(
        dest.path().join("empty.txt").exists(),
        "Empty file should exist"
    );
    assert!(
        dest.path().join("empty_dir").exists(),
        "Empty directory should exist"
    );
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
    assert!(
        dest.path().join(&long_name).exists(),
        "Long filename file should exist"
    );
}

#[test]
fn test_concurrent_sync_safety() {
    let (source, dest) = setup_test_dir("concurrent");

    // Create files
    fs::write(source.path().join("file1.txt"), "content1").unwrap();
    fs::write(source.path().join("file2.txt"), "content2").unwrap();

    // Start two syncs concurrently
    let dest_path = dest.path().to_path_buf();
    let source_path = format!("{}/", source.path().display());

    let handle1 = std::thread::spawn({
        let dest_path = dest_path.clone();
        let source_path = source_path.clone();
        move || {
            Command::new(sy_bin())
                .args([&source_path, dest_path.to_str().unwrap(), "--exclude-vcs"])
                .output()
                .unwrap()
        }
    });

    let handle2 = std::thread::spawn({
        let dest_path = dest_path.clone();
        let source_path = source_path.clone();
        move || {
            Command::new(sy_bin())
                .args([&source_path, dest_path.to_str().unwrap(), "--exclude-vcs"])
                .output()
                .unwrap()
        }
    });

    let output1 = handle1.join().unwrap();
    let output2 = handle2.join().unwrap();

    // At least one should succeed
    assert!(
        output1.status.success() || output2.status.success(),
        "At least one sync should succeed"
    );

    // Check files exist
    assert!(dest.path().join("file1.txt").exists(), "File1 should exist");
    assert!(dest.path().join("file2.txt").exists(), "File2 should exist");
}

#[test]
fn test_backup_dir_flag() {
    let (source, dest) = setup_test_dir("backup_dir");

    // Create initial file in dest
    fs::write(dest.path().join("file.txt"), "old content").unwrap();

    // Wait to ensure source is newer
    thread::sleep(Duration::from_secs(2));

    // Create updated file in source
    fs::write(source.path().join("file.txt"), "new content").unwrap();

    let backup_dir = dest.path().join("backups");
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--backup",
            &format!("--backup-dir={}", backup_dir.display()),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Check backup file exists in backup dir
    assert!(
        backup_dir.join("file.txt~").exists(),
        "Backup file should exist in backup dir"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "new content"
    );
}

#[test]
fn test_times_flag() {
    let (source, dest) = setup_test_dir("times");

    // Create a file
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--preserve-times",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Check file exists
    assert!(dest.path().join("file.txt").exists(), "File should exist");

    // Check mtime is preserved (within tolerance)
    let src_meta = fs::metadata(source.path().join("file.txt")).unwrap();
    let dst_meta = fs::metadata(dest.path().join("file.txt")).unwrap();
    let src_mtime = src_meta.modified().unwrap();
    let dst_mtime = dst_meta.modified().unwrap();
    let diff = src_mtime
        .duration_since(dst_mtime)
        .unwrap_or_else(|_| dst_mtime.duration_since(src_mtime).unwrap());
    assert!(
        diff.as_secs() <= 1,
        "Mtime should be preserved (diff: {}s)",
        diff.as_secs()
    );
}

#[test]
fn test_perms_flag() {
    let (source, dest) = setup_test_dir("perms");

    // Create a file
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--preserve-permissions",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Check file exists
    assert!(dest.path().join("file.txt").exists(), "File should exist");

    // Check permissions are preserved
    let src_meta = fs::metadata(source.path().join("file.txt")).unwrap();
    let dst_meta = fs::metadata(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        src_meta.permissions().mode(),
        dst_meta.permissions().mode(),
        "Permissions should be preserved"
    );
}

#[test]
fn test_default_includes_git() {
    // By default, .git directories are included (rsync-compatible)
    let (source, dest) = setup_test_dir("default_git");

    // setup_test_dir already does git init, so .git exists
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Run without --exclude-vcs
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // .git SHOULD be included by default (rsync-compatible)
    assert!(
        dest.path().join(".git").exists(),
        ".git should be included by default"
    );
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_backup_readonly_dir() {
    let (source, dest) = setup_test_dir("backup_readonly");

    // Create initial file in dest
    fs::write(dest.path().join("file.txt"), "old content").unwrap();

    // Make dest read-only
    let mut perms = fs::metadata(dest.path()).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(dest.path(), perms).unwrap();

    // Create updated file in source
    thread::sleep(Duration::from_secs(2));
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

    // Should fail because dest is read-only
    assert!(
        !output.status.success(),
        "Should fail when dest is read-only"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Permission denied")
            || stderr.contains("permission")
            || stderr.contains("denied"),
        "Error should mention permission: {}",
        stderr
    );

    // Restore permissions for cleanup
    let perms = fs::Permissions::from_mode(0o644);
    fs::set_permissions(dest.path(), perms).unwrap();
}

#[test]
fn test_backup_dir_nonexistent() {
    let (source, dest) = setup_test_dir("backup_dir_nonexistent");

    // Create initial file in dest
    fs::write(dest.path().join("file.txt"), "old content").unwrap();

    // Wait to ensure source is newer
    thread::sleep(Duration::from_secs(2));

    // Create updated file in source
    fs::write(source.path().join("file.txt"), "new content").unwrap();

    // Use a non-existent backup dir (should be created automatically)
    let backup_dir = dest.path().join("nonexistent").join("backups");

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--backup",
            &format!("--backup-dir={}", backup_dir.display()),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Check backup file exists in backup dir
    assert!(
        backup_dir.join("file.txt~").exists(),
        "Backup file should exist in backup dir"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "new content"
    );
}

#[test]
fn test_backup_preserves_original() {
    let (source, dest) = setup_test_dir("backup_preserves");

    // Create initial file in dest with specific content
    fs::write(dest.path().join("file.txt"), "original content").unwrap();

    // Wait to ensure source is newer
    thread::sleep(Duration::from_secs(2));

    // Create updated file in source
    fs::write(source.path().join("file.txt"), "updated content").unwrap();

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

    // Check backup file contains original content
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt~")).unwrap(),
        "original content"
    );
    // Check main file contains updated content
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "updated content"
    );
}

/// --diff details every planned operation with byte sizes in dry-run mode.
#[test]
fn test_diff_mode_details_planned_changes() {
    let (source, dest) = setup_test_dir("diff_mode");

    fs::write(source.path().join("small.txt"), "tiny").unwrap();
    fs::write(source.path().join("large.txt"), "x".repeat(4096)).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--dry-run",
            "--diff",
            "-v",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("small.txt").exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Would create: "),
        "diff mode must detail per-file plans, got:\n{stdout}"
    );
    assert!(
        stdout.contains("(4.00 KB)"),
        "diff mode must include byte sizes, got:\n{stdout}"
    );
}

/// --diff without --dry-run is rejected: it would silently do nothing.
#[test]
fn test_diff_without_dry_run_fails() {
    let (source, dest) = setup_test_dir("diff_reject");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--diff",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--diff requires --dry-run"),
        "got:\n{stderr}"
    );
    // Nothing was mutated: the command must not have synced before failing.
    assert!(!dest.path().join("file.txt").exists());
}

/// --trash was never implemented in any engine and was removed; passing it
/// must fail as an unknown argument rather than silently doing nothing.
#[test]
fn test_trash_flag_removed() {
    let (source, dest) = setup_test_dir("trash_removed");

    fs::write(source.path().join("keep.txt"), "keep").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--trash",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument") && stderr.contains("--trash"),
        "got:\n{stderr}"
    );
    assert!(!dest.path().join("keep.txt").exists());
}

/// --bwlimit actually paces local transfers: bytes above the one-second burst
/// must take token-deficit time. This pins the regression where the local
/// engine silently ignored the limit and transferred at full disk speed.
#[test]
fn test_bwlimit_paces_local_transfer() {
    let (source, dest) = setup_test_dir("bwlimit_pace");

    // 40 KiB payload at a 16 KiB/s limit: burst covers 16 KiB, the remaining
    // 24 KiB need ~1.5 s of token refill.
    fs::write(source.path().join("paced.bin"), vec![0_u8; 40 * 1024]).unwrap();

    let start = std::time::Instant::now();
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--bwlimit=16KB",
        ])
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(output.status.success());
    assert_eq!(
        fs::read(dest.path().join("paced.bin")).unwrap().len(),
        40 * 1024
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(1300),
        "transfer must be paced: 40 KiB at 16 KiB/s needs ~1.5 s, took {elapsed:?}"
    );
}

/// --verify enables post-write verification on the local engine: committed
/// files are hashed against the source and counted. The 0.4 wiring computed
/// verification from a helper hard-coded to false, silently ignoring the flag.
#[test]
fn test_verify_counts_verified_files() {
    let (source, dest) = setup_test_dir("verify_counts");

    fs::write(source.path().join("a.txt"), "alpha").unwrap();
    fs::write(source.path().join("b.txt"), "beta").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--verify=after",
            "--stats",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The source is a fresh git repository, so the exact count includes git
    // internals; what matters is that every created file was verified.
    let created = stdout
        .lines()
        .find(|line| line.contains("Files created:"))
        .and_then(|line| line.split_whitespace().last())
        .and_then(|v| v.parse::<usize>().ok())
        .expect("created count");
    // "Verification:    N files (xxHash3)" - N is the token before "files".
    let verified = stdout
        .lines()
        .find(|line| line.contains("Verification:"))
        .and_then(|line| {
            line.split_whitespace()
                .zip(line.split_whitespace().skip(1))
                .find(|(_, next)| *next == "files" || next.starts_with("files"))
                .and_then(|(token, _)| token.parse::<usize>().ok())
        })
        .expect("verified count");
    assert!(
        stdout.contains("Verification:"),
        "--verify=after must report verification, got:\n{stdout}"
    );
    assert_eq!(
        created, verified,
        "every created file must be verified under --verify=after"
    );
    assert!(
        verified >= 2,
        "the two data files must be among the verified"
    );
}

/// The checksum database was consumed only by the unreachable legacy engine
/// and is removed; passing its flags must fail as unknown arguments.
#[test]
fn test_checksum_db_flags_removed() {
    let (source, dest) = setup_test_dir("checksum_db_removed");

    fs::write(source.path().join("file.txt"), "content").unwrap();

    for flag in [
        "--checksum-db",
        "--clear-checksum-db",
        "--prune-checksum-db",
    ] {
        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
                flag,
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{flag} must be rejected as unknown"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unexpected argument"),
            "{flag} not rejected:\n{stderr}"
        );
    }
    assert!(!dest.path().join("file.txt").exists());
}

/// --remove-source-files locally: transferred and planner-verified unchanged
/// files move (destination bytes intact); skips without verified destination
/// parity keep their source; directories are never removed.
#[test]
fn test_remove_source_files_moves_committed_and_verified_entries() {
    let (source, dest) = setup_test_dir("remove_source_files");

    fs::write(source.path().join("created.txt"), "created").unwrap();
    // Different lengths force an Update under the quick check even on
    // filesystems whose mtime granularity would otherwise match the pair.
    fs::write(dest.path().join("updated.txt"), "stale-bytes").unwrap();
    fs::write(source.path().join("updated.txt"), "new").unwrap();
    fs::create_dir(source.path().join("sub")).unwrap();
    fs::write(source.path().join("sub/inner.txt"), "inner").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--remove-source-files",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Moved bytes all present at the destination.
    assert_eq!(
        fs::read_to_string(dest.path().join("created.txt")).unwrap(),
        "created"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("updated.txt")).unwrap(),
        "new"
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("sub/inner.txt")).unwrap(),
        "inner"
    );

    // Sources removed; empty directory kept.
    assert!(!source.path().join("created.txt").exists());
    assert!(!source.path().join("updated.txt").exists());
    assert!(!source.path().join("sub/inner.txt").exists());
    assert!(source.path().join("sub").is_dir());
}

/// Under --remove-source-files, a file that exists only in the source is
/// never removed when --existing skips its transfer: the destination has no
/// verified copy, so removal would destroy the only remaining bytes.
#[test]
fn test_remove_source_files_keeps_untransferred_source_under_existing() {
    let (source, dest) = setup_test_dir("remove_source_existing");

    fs::write(source.path().join("only-here.txt"), "precious").unwrap();
    fs::write(dest.path().join("other.txt"), "else").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--remove-source-files",
            "--existing",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(source.path().join("only-here.txt")).is_ok(),
        "untransferred source must be kept under --existing"
    );
    assert!(!dest.path().join("only-here.txt").exists());
}
