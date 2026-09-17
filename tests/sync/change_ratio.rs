//! Integration coverage for large local file updates.
//!
//! In v0.5 the local transfer path no longer applies rsync-style rolling delta
//! policy to ordinary local copies. Transfer selection is capability-driven:
//! sparse files may use sparse copy, low-change updates on COW filesystems may
//! use reflink + patch, and other updates use an atomic native whole-file copy.
//!
//! These tests intentionally assert observable sync semantics rather than a
//! particular filesystem-dependent strategy.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    std::env::var("CARGO_BIN_EXE_sy").unwrap_or_else(|_| "target/debug/sy".to_string())
}

fn sync_update(size: usize, changed_prefix: usize) {
    assert!(changed_prefix <= size);

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let source_file = source.path().join("test.dat");
    let dest_file = dest.path().join("test.dat");

    fs::write(&dest_file, vec![0_u8; size]).unwrap();

    let mut source_data = vec![0_u8; size];
    source_data[..changed_prefix].fill(1);
    fs::write(&source_file, &source_data).unwrap();

    let output = Command::new(sy_bin())
        .args([source_file.as_os_str(), dest_file.as_os_str()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sync failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&dest_file).unwrap(), source_data);
}

#[test]
fn small_change_large_file_updates_correctly() {
    sync_update(15_000_000, 150_000);
}

#[test]
fn medium_change_large_file_updates_correctly() {
    sync_update(15_000_000, 7_500_000);
}

#[test]
fn complete_change_large_file_updates_correctly() {
    sync_update(15_000_000, 15_000_000);
}

#[test]
fn large_reflink_candidate_updates_correctly() {
    // Above the 16 MiB reflink-patch eligibility threshold. Whether reflink
    // patching is actually selected depends on the runner filesystem.
    sync_update(20_000_000, 2_000_000);
}

#[test]
fn high_change_reflink_candidate_falls_back_safely() {
    // Also above the reflink threshold, but with enough changed data that the
    // strategy selector should normally prefer native whole-copy.
    sync_update(20_000_000, 18_000_000);
}
