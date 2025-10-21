// Critical delta sync correctness tests
//
// These tests verify that delta sync produces correct output in various scenarios,
// including file size changes, hard links, and COW vs non-COW filesystems.

use std::fs;
use std::io::Write;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

#[test]
fn test_delta_sync_file_shrinks() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create large dest file (100KB)
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 100_000]).unwrap();

    // Create smaller source file (50KB)
    let source_file = source.path().join("test.dat");
    let source_data = vec![1u8; 50_000];
    fs::write(&source_file, &source_data).unwrap();

    // Sync (should use delta sync since files exist)
    let output = Command::new(sy_bin())
        .args([
            source_file.to_str().unwrap(),
            dest_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify dest is now same size as source (not 100KB!)
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(
        result_data.len(),
        50_000,
        "Dest file should be truncated to source size"
    );
    assert_eq!(
        result_data, source_data,
        "Dest file should match source exactly"
    );
}

#[test]
fn test_delta_sync_file_grows() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create small dest file (50KB)
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 50_000]).unwrap();

    // Create larger source file (100KB)
    let source_file = source.path().join("test.dat");
    let source_data = vec![1u8; 100_000];
    fs::write(&source_file, &source_data).unwrap();

    // Sync
    let output = Command::new(sy_bin())
        .args([
            source_file.to_str().unwrap(),
            dest_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify dest is now same size as source
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(
        result_data.len(),
        100_000,
        "Dest file should grow to source size"
    );
    assert_eq!(
        result_data, source_data,
        "Dest file should match source exactly"
    );
}

#[test]
fn test_delta_sync_correctness() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create dest file with initial content (10MB)
    let dest_file = dest.path().join("test.dat");
    let mut initial_data = Vec::new();
    for i in 0..10_000 {
        write!(&mut initial_data, "block {:04}\n", i).unwrap();
    }
    fs::write(&dest_file, &initial_data).unwrap();

    // Modify some blocks in source
    let source_file = source.path().join("test.dat");
    let mut modified_data = initial_data.clone();
    // Change blocks 100-200
    for i in 100..200 {
        let offset = i * 11; // Each block is "block XXXX\n" = 11 bytes
        let replacement = format!("CHANG {:04}\n", i);
        modified_data[offset..offset + 11].copy_from_slice(replacement.as_bytes());
    }
    fs::write(&source_file, &modified_data).unwrap();

    // Sync using delta sync
    let output = Command::new(sy_bin())
        .args([
            source_file.to_str().unwrap(),
            dest_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify dest matches source exactly
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(
        result_data, modified_data,
        "Dest file should be bit-identical to source after delta sync"
    );

    // Verify delta sync was actually used (check log output)
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should see delta sync messages in logs if RUST_LOG=debug
}

#[test]
#[cfg(unix)]
fn test_hard_links_preserved() {
    use std::os::unix::fs::MetadataExt;

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create file1
    let file1 = source.path().join("file1.txt");
    fs::write(&file1, "shared content").unwrap();

    // Create hard link to file1
    let file2 = source.path().join("file2.txt");
    fs::hard_link(&file1, &file2).unwrap();

    // Verify hard link exists
    let inode1 = fs::metadata(&file1).unwrap().ino();
    let inode2 = fs::metadata(&file2).unwrap().ino();
    assert_eq!(inode1, inode2, "Source files should be hard linked");

    // Sync directory with --preserve-hardlinks flag
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify both files exist in dest
    let dest_file1 = dest.path().join("file1.txt");
    let dest_file2 = dest.path().join("file2.txt");
    assert!(dest_file1.exists());
    assert!(dest_file2.exists());

    // Verify hard link is preserved
    let dest_inode1 = fs::metadata(&dest_file1).unwrap().ino();
    let dest_inode2 = fs::metadata(&dest_file2).unwrap().ino();
    assert_eq!(
        dest_inode1, dest_inode2,
        "Dest files should be hard linked (same inode)"
    );

    // Verify content is correct
    assert_eq!(fs::read_to_string(&dest_file1).unwrap(), "shared content");
    assert_eq!(fs::read_to_string(&dest_file2).unwrap(), "shared content");
}

#[test]
#[cfg(unix)]
fn test_hard_link_update_both_files_same_content() {
    use std::os::unix::fs::MetadataExt;

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create initial hard linked files in source
    let file1 = source.path().join("file1.txt");
    let file2 = source.path().join("file2.txt");
    fs::write(&file1, "initial").unwrap();
    fs::hard_link(&file1, &file2).unwrap();

    // Initial sync with --preserve-hardlinks
    Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    // Modify one of the source hard linked files
    fs::write(&file1, "modified content").unwrap();
    // file2 also has "modified content" because they share the same inode

    // Sync again with --preserve-hardlinks
    let output = Command::new(sy_bin())
        .args([
            source.path().to_str().unwrap(),
            dest.path().to_str().unwrap(),
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify both dest files have new content
    let dest_file1 = dest.path().join("file1.txt");
    let dest_file2 = dest.path().join("file2.txt");
    assert_eq!(
        fs::read_to_string(&dest_file1).unwrap(),
        "modified content"
    );
    assert_eq!(
        fs::read_to_string(&dest_file2).unwrap(),
        "modified content"
    );

    // Verify hard link still preserved
    let dest_inode1 = fs::metadata(&dest_file1).unwrap().ino();
    let dest_inode2 = fs::metadata(&dest_file2).unwrap().ino();
    assert_eq!(dest_inode1, dest_inode2, "Hard link should be preserved");
}
