//! Integration tests for file comparison modes
//!
//! These tests verify --ignore-times, --size-only, --checksum, -u/--update,
//! --ignore-existing, and rsync compatibility flags.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_git_repo(dir: &TempDir) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .unwrap();
}

// =============================================================================
// --ignore-times Tests
// =============================================================================

#[test]
fn test_ignore_times_forces_comparison() {
    // --ignore-times should compare files even if mtime matches
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create identical files
    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(dest.path().join("file.txt"), "content").unwrap();

    // Make mtimes match
    let source_meta = fs::metadata(source.path().join("file.txt")).unwrap();
    filetime::set_file_mtime(
        dest.path().join("file.txt"),
        filetime::FileTime::from_last_modification_time(&source_meta),
    )
    .unwrap();

    // First sync without --ignore-times should skip (mtime and size match)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("skipped")
            || stdout.contains("Skipped")
            || stdout.contains("Files skipped:     1"),
        "File should be skipped when mtime matches"
    );

    // Now change dest content but keep mtime the same
    fs::write(dest.path().join("file.txt"), "different").unwrap();
    filetime::set_file_mtime(
        dest.path().join("file.txt"),
        filetime::FileTime::from_last_modification_time(&source_meta),
    )
    .unwrap();

    // With --ignore-times, should detect the difference and update
    let output = Command::new(sy_bin())
        .args([
            "--ignore-times",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --ignore-times failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // File should have been updated
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        dest_content, "content",
        "--ignore-times should update file even when mtime matches"
    );
}

#[test]
fn test_ignore_times_with_identical_files() {
    // --ignore-times with truly identical files should still skip
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "identical").unwrap();
    fs::write(dest.path().join("file.txt"), "identical").unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--ignore-times",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Content should still be identical (no unnecessary overwrite)
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(dest_content, "identical");
}

// =============================================================================
// --size-only Tests
// =============================================================================

#[test]
fn test_size_only_skips_mtime_check() {
    // --size-only should only compare file sizes, not mtime
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create files with same size but different content
    fs::write(source.path().join("file.txt"), "AAAA").unwrap(); // 4 bytes
    fs::write(dest.path().join("file.txt"), "BBBB").unwrap(); // 4 bytes

    // Wait to ensure different mtime
    thread::sleep(Duration::from_millis(100));

    // Touch source to make it newer
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(
        source.path().join("file.txt"),
        filetime::FileTime::from_system_time(now),
    )
    .unwrap();

    // With --size-only, should NOT update (same size)
    let output = Command::new(sy_bin())
        .args([
            "--size-only",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --size-only failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Content should NOT have changed (size was same)
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        dest_content, "BBBB",
        "--size-only should skip files with same size"
    );
}

#[test]
fn test_size_only_updates_different_size() {
    // --size-only should update files with different sizes
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "longer content").unwrap();
    fs::write(dest.path().join("file.txt"), "short").unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--size-only",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Content should have been updated (different size)
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        dest_content, "longer content",
        "--size-only should update files with different sizes"
    );
}

// =============================================================================
// --checksum Tests
// =============================================================================

#[test]
fn test_checksum_compares_content() {
    // --checksum should compare file content, not mtime
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create files with same size but different content
    fs::write(source.path().join("file.txt"), "AAAA").unwrap();
    fs::write(dest.path().join("file.txt"), "BBBB").unwrap();

    // Make dest newer (normally would not be updated)
    let future = std::time::SystemTime::now() + Duration::from_secs(3600);
    filetime::set_file_mtime(
        dest.path().join("file.txt"),
        filetime::FileTime::from_system_time(future),
    )
    .unwrap();

    // With --checksum, should update based on content difference
    let output = Command::new(sy_bin())
        .args([
            "--checksum",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --checksum failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Content should have been updated (checksum different)
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        dest_content, "AAAA",
        "--checksum should update files with different content"
    );
}

#[test]
fn test_checksum_skips_identical_content() {
    // --checksum should skip files with identical content
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "identical").unwrap();
    fs::write(dest.path().join("file.txt"), "identical").unwrap();

    // Make mtimes different
    let past = std::time::SystemTime::now() - Duration::from_secs(3600);
    filetime::set_file_mtime(
        dest.path().join("file.txt"),
        filetime::FileTime::from_system_time(past),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--checksum",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Content should still be identical
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(dest_content, "identical");
}

// =============================================================================
// Mutual Exclusivity Tests
// =============================================================================

#[test]
fn test_comparison_flags_mutually_exclusive() {
    // --ignore-times, --size-only, and --checksum should be mutually exclusive
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Try --ignore-times with --size-only
    let output = Command::new(sy_bin())
        .args([
            "--ignore-times",
            "--size-only",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "Should fail with mutually exclusive flags"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mutually exclusive") || stderr.contains("cannot be used with"),
        "Error message should mention mutual exclusivity: {}",
        stderr
    );
}

// =============================================================================
// Default Behavior Tests
// =============================================================================

#[test]
fn test_default_uses_mtime_and_size() {
    // Default behavior: compare mtime and size
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());

    // Second sync should skip (same mtime and size)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show file was skipped
    assert!(
        stdout.contains("skipped")
            || stdout.contains("Skipped")
            || stdout.contains("Files skipped:     1")
            || stdout.contains("Files updated:     0"),
        "File should be skipped on second sync with default mode: {}",
        stdout
    );
}

// =============================================================================
// -u/--update Tests (skip if dest is newer)
// =============================================================================

#[test]
fn test_update_skips_newer_dest() {
    // -u/--update should skip files where destination is newer than source
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "source content").unwrap();
    fs::write(dest.path().join("file.txt"), "newer dest content").unwrap();

    // Make dest file newer than source
    let future = std::time::SystemTime::now() + Duration::from_secs(3600);
    filetime::set_file_mtime(
        dest.path().join("file.txt"),
        filetime::FileTime::from_system_time(future),
    )
    .unwrap();

    // With -u, should skip the file (dest is newer)
    let output = Command::new(sy_bin())
        .args([
            "-u",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Content should NOT have changed (dest was newer)
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        dest_content, "newer dest content",
        "-u should skip files where dest is newer"
    );
}

#[test]
fn test_update_long_flag() {
    // --update should work the same as -u
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "source").unwrap();
    fs::write(dest.path().join("file.txt"), "dest").unwrap();

    // Make dest newer
    let future = std::time::SystemTime::now() + Duration::from_secs(3600);
    filetime::set_file_mtime(
        dest.path().join("file.txt"),
        filetime::FileTime::from_system_time(future),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--update",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        dest_content, "dest",
        "--update should skip newer dest files"
    );
}

#[test]
fn test_update_copies_older_dest() {
    // -u should still copy when source is newer than dest
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "new source").unwrap();
    fs::write(dest.path().join("file.txt"), "old dest").unwrap();

    // Make source newer than dest
    thread::sleep(Duration::from_millis(100));
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(
        source.path().join("file.txt"),
        filetime::FileTime::from_system_time(now),
    )
    .unwrap();

    let past = now - Duration::from_secs(3600);
    filetime::set_file_mtime(
        dest.path().join("file.txt"),
        filetime::FileTime::from_system_time(past),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args([
            "-u",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Content SHOULD have changed (source was newer)
    let dest_content = fs::read_to_string(dest.path().join("file.txt")).unwrap();
    assert_eq!(
        dest_content, "new source",
        "-u should update when source is newer"
    );
}

// =============================================================================
// --ignore-existing Tests
// =============================================================================

#[test]
fn test_ignore_existing_skips_existing_files() {
    // --ignore-existing should skip files that already exist in dest
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("existing.txt"), "source version").unwrap();
    fs::write(source.path().join("new.txt"), "new file").unwrap();
    fs::write(dest.path().join("existing.txt"), "dest version").unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--ignore-existing",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Existing file should NOT have changed
    let existing_content = fs::read_to_string(dest.path().join("existing.txt")).unwrap();
    assert_eq!(
        existing_content, "dest version",
        "--ignore-existing should not overwrite existing files"
    );

    // New file should have been created
    let new_content = fs::read_to_string(dest.path().join("new.txt")).unwrap();
    assert_eq!(
        new_content, "new file",
        "--ignore-existing should still create new files"
    );
}

// =============================================================================
// rsync Compatibility Tests
// =============================================================================

#[test]
fn test_rsync_r_flag_accepted() {
    // -r should be silently accepted (sy is always recursive)
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::create_dir_all(source.path().join("subdir")).unwrap();
    fs::write(source.path().join("subdir/file.txt"), "nested").unwrap();

    // -r should work without error
    let output = Command::new(sy_bin())
        .args([
            "-r",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "-r flag should be accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Nested file should exist (sy is always recursive)
    assert!(dest.path().join("subdir/file.txt").exists());
}

#[test]
fn test_rsync_avr_combination() {
    // -avr is common rsync muscle memory, should work
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            "-avr",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "-avr should work: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(dest.path().join("file.txt").exists());
}

// =============================================================================
// Short Flag Tests
// =============================================================================

#[test]
fn test_w_short_flag_recognized() {
    // -w should be recognized as --watch (we can't easily test watch behavior,
    // but we can verify the flag is parsed)
    let output = Command::new(sy_bin()).args(["--help"]).output().unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("-w, --watch"),
        "-w should be short for --watch"
    );
}

#[test]
fn test_z_short_flag_recognized() {
    // -z should be recognized as --compress
    let output = Command::new(sy_bin()).args(["--help"]).output().unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("-z, --compress"),
        "-z should be short for --compress"
    );
}
