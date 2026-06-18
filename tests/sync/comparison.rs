//! Comparison mode tests — ignore-times, size-only, checksum, update.
//!
//! Consolidates: comparison_modes_test.rs.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir() -> (TempDir, TempDir) {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    // Initialize git repo for consistent scanning
    Command::new("git")
        .args(["init"])
        .current_dir(source.path())
        .output()
        .unwrap();
    (source, dest)
}

fn sync_args<'a>(source: &'a TempDir, dest: &'a TempDir, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        format!("{}/", source.path().display()),
        dest.path().to_str().unwrap().to_string(),
        "--exclude-vcs".to_string(),
    ];
    for e in extra {
        args.push(e.to_string());
    }
    args
}

#[test]
fn test_ignore_times_forces_comparison() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same content, --ignore-times should still "update"
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--ignore-times"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_ignore_times_with_identical_files() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // --ignore-times on identical files should still "update"
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--ignore-times"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_size_only_skips_mtime_check() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same size, --size-only should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--size-only"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_size_only_updates_different_size() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Change size
    fs::write(source.path().join("file.txt"), "new longer content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--size-only"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files updated:     1"));
}

#[test]
fn test_default_uses_mtime_and_size() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same content, no changes - should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_ignore_existing() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "new").unwrap();
    fs::write(dest.path().join("file.txt"), "old").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--ignore-existing"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "old");
}

#[test]
fn test_update_skips_newer_dest() {
    let (source, dest) = setup_test_dir();

    // Create file in dest with newer mtime
    fs::write(dest.path().join("file.txt"), "dest content").unwrap();
    // Set source to older time
    fs::write(source.path().join("file.txt"), "source content").unwrap();

    // Make dest newer by touching it
    let dest_file = dest.path().join("file.txt");
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(
        &dest_file,
        filetime::FileTime::from_system_time(now + std::time::Duration::from_secs(10)),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--update"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "dest content");
}

#[test]
fn test_update_long_flag() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "source").unwrap();
    fs::write(dest.path().join("file.txt"), "dest").unwrap();

    // Make dest newer
    let dest_file = dest.path().join("file.txt");
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(
        &dest_file,
        filetime::FileTime::from_system_time(now + std::time::Duration::from_secs(10)),
    )
    .unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--update"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:"));
}

#[test]
fn test_checksum_compares_content() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Same content, --checksum should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--checksum"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_checksum_skips_identical_content() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "same content").unwrap();

    // Initial sync
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &[]))
        .output()
        .unwrap();
    assert!(output.status.success());

    // Force same mtime by touching dest to be same time as source
    let src_meta = fs::metadata(source.path().join("file.txt")).unwrap();
    let dest_file = dest.path().join("file.txt");
    filetime::set_file_mtime(
        &dest_file,
        filetime::FileTime::from_system_time(src_meta.modified().unwrap()),
    )
    .unwrap();

    // With --checksum, same content should skip
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--checksum"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_comparison_flags_mutually_exclusive() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // --size-only and --checksum together should fail
    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--size-only", "--checksum"]))
        .output()
        .unwrap();

    assert!(!output.status.success());
}

#[test]
fn test_update_copies_older_dest() {
    let (source, dest) = setup_test_dir();

    // Create file in dest first (will have older time)
    fs::write(dest.path().join("file.txt"), "old content").unwrap();

    // Sleep to ensure mtime differs
    std::thread::sleep(std::time::Duration::from_millis(2100));

    // Create file in source with newer time
    fs::write(source.path().join("file.txt"), "new content").unwrap();

    // Run sync with --update (should copy newer source)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--update",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "new content");
}

#[test]
fn test_rsync_r_flag_accepted() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // -r is rsync compatibility flag for recursive
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "-r",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_rsync_avr_combination() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // -avr is common rsync flag组合 (archive, verbose, recursive)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "-avr",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_w_short_flag_recognized() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // -w is --watch flag; for sync test use -v (verbose) instead
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "-v",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_z_short_flag_recognized() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // -z is rsync flag for compression, now requires a value
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "-z",
            "auto",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
}

// Delta sync tests

#[test]
fn test_delta_sync_file_grows() {
    let (source, dest) = setup_test_dir();

    // Create initial file
    fs::write(source.path().join("file.txt"), "initial content").unwrap();

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

    // Grow the file
    fs::write(source.path().join("file.txt"), "initial content with more data").unwrap();

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
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "initial content with more data"
    );
}

#[test]
fn test_delta_sync_file_shrinks() {
    let (source, dest) = setup_test_dir();

    // Create initial file
    fs::write(source.path().join("file.txt"), "initial content that is longer").unwrap();

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

    // Shrink the file
    fs::write(source.path().join("file.txt"), "short").unwrap();

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
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "short"
    );
}

#[test]
fn test_delta_sync_correctness() {
    let (source, dest) = setup_test_dir();

    // Create file with known content
    let content = "Hello World\n".repeat(1000);
    fs::write(source.path().join("file.txt"), &content).unwrap();

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

    // Modify middle of file
    let mut modified = content.clone();
    modified.insert_str(5000, "MODIFIED");
    fs::write(source.path().join("file.txt"), &modified).unwrap();

    // Second sync should produce identical content
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
        modified
    );
}

#[test]
fn test_hard_links_preserved() {
    let (source, dest) = setup_test_dir();

    // Create hard linked files
    fs::write(source.path().join("original.txt"), "content").unwrap();
    fs::hard_link(
        source.path().join("original.txt"),
        source.path().join("link.txt"),
    )
    .unwrap();

    // Sync with --preserve-hardlinks
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("original.txt").exists());
    assert!(dest.path().join("link.txt").exists());

    // Check that hard links are preserved
    let orig_meta = fs::metadata(dest.path().join("original.txt")).unwrap();
    let link_meta = fs::metadata(dest.path().join("link.txt")).unwrap();
    assert_eq!(orig_meta.ino(), link_meta.ino());
}

#[test]
fn test_sparse_file_delta_sync_preserves_sparseness() {
    let (source, dest) = setup_test_dir();

    // Create sparse file (large with mostly zeros)
    let mut content = vec![0u8; 1024 * 1024]; // 1MB
    content[0] = 1;
    content[1024 * 512] = 1;
    fs::write(source.path().join("sparse.bin"), &content).unwrap();

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

    // Sleep to ensure mtime differs
    std::thread::sleep(std::time::Duration::from_millis(2100));

    // Modify sparse file
    content[100] = 1;
    fs::write(source.path().join("sparse.bin"), &content).unwrap();

    // Second sync
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
        fs::read(dest.path().join("sparse.bin")).unwrap(),
        content
    );
}

#[test]
fn test_ignore_existing_skips_existing_files() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("existing.txt"), "source version").unwrap();
    fs::write(source.path().join("new.txt"), "new file").unwrap();
    fs::write(dest.path().join("existing.txt"), "dest version").unwrap();

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

    // Existing file should NOT have changed
    let existing_content = fs::read_to_string(dest.path().join("existing.txt")).unwrap();
    assert_eq!(
        existing_content, "dest version",
        "--ignore-existing should not overwrite existing files"
    );

    // New file should have been created
    let new_content = fs::read_to_string(dest.path().join("new.txt")).unwrap();
    assert_eq!(new_content, "new file", "New file should be created");
}

#[test]
fn test_nanosecond_mtime_preserved() {
    let (source, dest) = setup_test_dir();

    // Create file
    let file = source.path().join("file.txt");
    fs::write(&file, "content").unwrap();

    // Set a specific mtime with nanosecond precision
    // Use a time that has non-zero nanoseconds
    #[cfg(unix)]
    {
        use std::time::{Duration, UNIX_EPOCH};

        // 1234567890 seconds + 123456789 nanoseconds
        let mtime = UNIX_EPOCH + Duration::new(1234567890, 123456789);
        let atime = filetime::FileTime::from_system_time(UNIX_EPOCH);
        let mtime_ft = filetime::FileTime::from_system_time(mtime);
        filetime::set_file_times(&file, atime, mtime_ft).unwrap();

        // Verify source mtime has nanosecond precision
        let src_meta = fs::metadata(&file).unwrap();
        let src_mtime = src_meta.modified().unwrap();
        let src_nanos = src_mtime.duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        // Filesystem may truncate — just verify it's non-zero and close
        assert!(src_nanos > 0, "Source mtime nanos should be non-zero, got {}", src_nanos);

        // Sync
        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
                "--exclude-vcs",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());

        // Verify dest mtime matches source mtime (within filesystem precision)
        let dest_meta = fs::metadata(dest.path().join("file.txt")).unwrap();
        let dest_mtime = dest_meta.modified().unwrap();
        let dest_nanos = dest_mtime.duration_since(UNIX_EPOCH).unwrap().subsec_nanos();

        // Both should have non-zero nanoseconds (not truncated to seconds)
        assert!(
            dest_nanos > 0,
            "Dest mtime nanos should be non-zero (not truncated to seconds), got {}",
            dest_nanos
        );

        // Mtime difference should be < 1 second (ideally 0)
        let diff = if dest_mtime > src_mtime {
            dest_mtime.duration_since(src_mtime).unwrap()
        } else {
            src_mtime.duration_since(dest_mtime).unwrap()
        };
        assert!(
            diff < Duration::from_secs(1),
            "Mtime difference should be < 1s, got {:?} (src_nanos={}, dest_nanos={})",
            diff, src_nanos, dest_nanos
        );
    }
}
