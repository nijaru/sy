//! Tests for symlink sync behavior
//!
//! These tests verify that syncing symlinks works correctly, including
//! overwriting existing symlinks and regular files.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Helper to run sy command
fn run_sy(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(args)
        .output()
        .expect("Failed to execute sy")
}

/// Test syncing a symlink to a destination that already has a symlink
#[test]
fn test_sync_overwrites_existing_symlink() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let dest = temp.path().join("dest");

    // Create source directory with a file and symlink
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("target.txt"), b"target content").unwrap();
    symlink(Path::new("target.txt"), source.join("link.txt")).unwrap();

    // Create dest directory with a DIFFERENT symlink at same path
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("other.txt"), b"other content").unwrap();
    symlink(Path::new("other.txt"), dest.join("link.txt")).unwrap();

    // Verify setup: dest symlink points to other.txt
    let dest_link = fs::read_link(dest.join("link.txt")).unwrap();
    assert_eq!(dest_link.to_str().unwrap(), "other.txt");

    // Run sync (trailing slash on source to sync contents, not the dir itself)
    let source_str = format!("{}/", source.display());
    let output = run_sy(&[&source_str, dest.to_str().unwrap(), "--verbose"]);

    // Should succeed
    assert!(
        output.status.success(),
        "Sync should succeed when overwriting symlinks. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify: dest symlink now points to target.txt (same as source)
    let dest_link_after = fs::read_link(dest.join("link.txt")).unwrap();
    assert_eq!(
        dest_link_after.to_str().unwrap(),
        "target.txt",
        "Symlink should be overwritten to point to target.txt"
    );
}

/// Test syncing a symlink to a destination that has a regular file at that path
#[test]
fn test_sync_symlink_over_regular_file() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let dest = temp.path().join("dest");

    // Create source with a symlink
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("target.txt"), b"target").unwrap();
    symlink(Path::new("target.txt"), source.join("link.txt")).unwrap();

    // Create dest with a regular file at same path
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("link.txt"), b"regular file content").unwrap();

    // Verify setup: dest/link.txt is a regular file
    assert!(dest.join("link.txt").is_file());
    assert!(!dest.join("link.txt").is_symlink());

    // Run sync (trailing slash on source to sync contents, not the dir itself)
    let source_str = format!("{}/", source.display());
    let output = run_sy(&[&source_str, dest.to_str().unwrap()]);

    // Should succeed (sy should handle this case)
    assert!(
        output.status.success(),
        "Sync should handle symlink over regular file. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // After sync, link.txt should be a symlink
    assert!(
        dest.join("link.txt").is_symlink(),
        "After sync, link.txt should be a symlink"
    );
}

/// Test that multiple symlinks pointing to same target are handled
#[test]
fn test_sync_multiple_symlinks_same_target() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let dest = temp.path().join("dest");

    // Create source with multiple symlinks to same target
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("target.txt"), b"shared target").unwrap();
    symlink(Path::new("target.txt"), source.join("link1.txt")).unwrap();
    symlink(Path::new("target.txt"), source.join("link2.txt")).unwrap();
    symlink(Path::new("target.txt"), source.join("link3.txt")).unwrap();

    // Create empty dest
    fs::create_dir_all(&dest).unwrap();

    // Run sync (trailing slash on source to sync contents, not the dir itself)
    let source_str = format!("{}/", source.display());
    let output = run_sy(&[&source_str, dest.to_str().unwrap()]);

    assert!(
        output.status.success(),
        "Sync should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify all symlinks exist and point correctly
    for link in &["link1.txt", "link2.txt", "link3.txt"] {
        let link_path = dest.join(link);
        assert!(link_path.is_symlink(), "{} should be a symlink", link);
        let target = fs::read_link(&link_path).unwrap();
        assert_eq!(target.to_str().unwrap(), "target.txt");
    }
}

/// Test syncing symlink to empty destination
#[test]
fn test_sync_symlink_to_empty_dest() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let dest = temp.path().join("dest");

    // Create source with a symlink
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("target.txt"), b"target content").unwrap();
    symlink(Path::new("target.txt"), source.join("link.txt")).unwrap();

    // Create empty dest
    fs::create_dir_all(&dest).unwrap();

    // Run sync
    let source_str = format!("{}/", source.display());
    let output = run_sy(&[&source_str, dest.to_str().unwrap()]);

    assert!(
        output.status.success(),
        "Sync should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify symlink was created
    assert!(
        dest.join("link.txt").is_symlink(),
        "link.txt should be a symlink"
    );
    let target = fs::read_link(dest.join("link.txt")).unwrap();
    assert_eq!(target.to_str().unwrap(), "target.txt");
}

/// Test that identical symlinks are skipped
#[test]
fn test_sync_skips_identical_symlink() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let dest = temp.path().join("dest");

    // Create source with a symlink
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("target.txt"), b"target content").unwrap();
    symlink(Path::new("target.txt"), source.join("link.txt")).unwrap();

    // Create dest with IDENTICAL symlink
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("target.txt"), b"target content").unwrap();
    symlink(Path::new("target.txt"), dest.join("link.txt")).unwrap();

    // Run sync
    let source_str = format!("{}/", source.display());
    let output = run_sy(&[&source_str, dest.to_str().unwrap(), "--verbose"]);

    assert!(
        output.status.success(),
        "Sync should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Symlink should still point to target.txt
    let target = fs::read_link(dest.join("link.txt")).unwrap();
    assert_eq!(target.to_str().unwrap(), "target.txt");

    // Check that it was skipped (not updated)
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The sync should show 0 updated since symlinks are identical
    assert!(
        stdout.contains("updated:") && stdout.contains("0")
            || stdout.contains("Files updated:     0"),
        "Identical symlinks should be skipped, got: {}",
        stdout
    );
}
