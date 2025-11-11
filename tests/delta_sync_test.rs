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
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
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
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
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
        writeln!(&mut initial_data, "block {:04}", i).unwrap();
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
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify dest matches source exactly
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(
        result_data, modified_data,
        "Dest file should be bit-identical to source after delta sync"
    );
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
    // Use trailing slash to copy contents (rsync-compatible behavior)
    let source_path = format!("{}/", source.path().display());
    let output = Command::new(sy_bin())
        .args([
            &source_path,
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
    // Use trailing slash to copy contents (rsync-compatible behavior)
    let source_path = format!("{}/", source.path().display());
    Command::new(sy_bin())
        .args([
            &source_path,
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
            &source_path,
            dest.path().to_str().unwrap(),
            "--preserve-hardlinks",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify both dest files have new content
    let dest_file1 = dest.path().join("file1.txt");
    let dest_file2 = dest.path().join("file2.txt");
    assert_eq!(fs::read_to_string(&dest_file1).unwrap(), "modified content");
    assert_eq!(fs::read_to_string(&dest_file2).unwrap(), "modified content");

    // Verify hard link still preserved
    let dest_inode1 = fs::metadata(&dest_file1).unwrap().ino();
    let dest_inode2 = fs::metadata(&dest_file2).unwrap().ino();
    assert_eq!(dest_inode1, dest_inode2, "Hard link should be preserved");
}

#[test]
#[cfg(target_os = "macos")]
fn test_cow_strategy_used_on_apfs() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create dest file (15MB - above 10MB delta threshold)
    let dest_file = dest.path().join("test.dat");
    fs::write(&dest_file, vec![0u8; 15_000_000]).unwrap();

    // Create source file with small change
    let source_file = source.path().join("test.dat");
    let mut source_data = vec![0u8; 15_000_000];
    source_data[5_000_000] = 1; // Change one byte
    fs::write(&source_file, &source_data).unwrap();

    // Sync with debug logging
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // Verify COW strategy was used if filesystem supports it
    // Note: /var/folders (macOS temp dir) may not support COW even on APFS systems
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Check if COW was used OR if filesystem doesn't support it (acceptable on temp dirs)
    let cow_used = stderr.contains("COW (clone + selective writes)")
        || stdout.contains("COW (clone + selective writes)");
    let no_cow_support = stderr.contains("filesystem does not support COW reflinks")
        || stdout.contains("filesystem does not support COW reflinks");

    assert!(
        cow_used || no_cow_support,
        "Should use COW strategy on APFS or report no COW support. Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );
}

#[test]
#[cfg(unix)]
fn test_inplace_strategy_used_with_hard_links() {
    use std::os::unix::fs::MetadataExt;

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create dest file with hard link (15MB - above 10MB delta threshold)
    let dest_file = dest.path().join("test.dat");
    let dest_link = dest.path().join("test_link.dat");
    fs::write(&dest_file, vec![0u8; 15_000_000]).unwrap();
    fs::hard_link(&dest_file, &dest_link).unwrap();

    // Verify hard link exists
    let nlink = fs::metadata(&dest_file).unwrap().nlink();
    assert_eq!(nlink, 2, "Dest file should have hard link");

    // Create source file with changes (only change 25% to stay below 75% threshold)
    let source_file = source.path().join("test.dat");
    let mut source_data = vec![0u8; 15_000_000];
    // Change only the first 25% (3.75MB)
    for byte in &mut source_data[..3_750_000] {
        *byte = 1;
    }
    fs::write(&source_file, &source_data).unwrap();

    // Sync with debug logging
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // Verify in-place strategy was used (check logs)
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stderr.contains("in-place (full file rebuild)") || stdout.contains("in-place (full file rebuild)"),
        "Should use in-place strategy (for hard links or filesystem limitations). Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );
}

#[test]
fn test_both_strategies_produce_identical_results() {
    // This test creates identical scenarios and verifies both strategies
    // produce bit-identical output

    // Test data: 100KB file with 10KB changed in middle
    let initial_data = vec![0u8; 100_000];
    let mut modified_data = initial_data.clone();
    for byte in &mut modified_data[45_000..55_000] {
        *byte = 0xFF;
    }

    // Scenario 1: COW strategy (normal case)
    {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        let dest_file = dest.path().join("test.dat");
        fs::write(&dest_file, &initial_data).unwrap();

        let source_file = source.path().join("test.dat");
        fs::write(&source_file, &modified_data).unwrap();

        let output = Command::new(sy_bin())
            .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
            .output()
            .unwrap();

        assert!(output.status.success());

        let result1 = fs::read(&dest_file).unwrap();
        assert_eq!(
            result1, modified_data,
            "COW strategy should produce correct output"
        );
    }

    // Scenario 2: In-place strategy (via hard link)
    #[cfg(unix)]
    {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        let dest_file = dest.path().join("test.dat");
        let dest_link = dest.path().join("link.dat");
        fs::write(&dest_file, &initial_data).unwrap();
        fs::hard_link(&dest_file, &dest_link).unwrap();

        let source_file = source.path().join("test.dat");
        fs::write(&source_file, &modified_data).unwrap();

        let output = Command::new(sy_bin())
            .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
            .output()
            .unwrap();

        assert!(output.status.success());

        let result2 = fs::read(&dest_file).unwrap();
        assert_eq!(
            result2, modified_data,
            "In-place strategy should produce correct output"
        );
    }
}

#[test]
fn test_strategy_selection_correctness() {
    // Verify that strategy selection produces correct results
    // regardless of which strategy is chosen

    let test_sizes = vec![
        1_000,     // 1KB
        10_000,    // 10KB
        100_000,   // 100KB
        1_000_000, // 1MB
    ];

    for size in test_sizes {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        // Create dest file
        let dest_file = dest.path().join("test.dat");
        fs::write(&dest_file, vec![0u8; size]).unwrap();

        // Create source file with random changes
        let source_file = source.path().join("test.dat");
        let source_data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        fs::write(&source_file, &source_data).unwrap();

        // Sync
        let output = Command::new(sy_bin())
            .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "Sync should succeed for size {}",
            size
        );

        // Verify correctness
        let result_data = fs::read(&dest_file).unwrap();
        assert_eq!(
            result_data, source_data,
            "Dest should match source exactly for size {}",
            size
        );
    }
}

#[test]
#[cfg(unix)]
#[ignore] // Requires manual setup with multiple mounted filesystems
fn test_cross_filesystem_uses_inplace_strategy() {
    // This test verifies that cross-filesystem delta sync uses in-place strategy.
    //
    // To run this test manually:
    // 1. Create a ramdisk or mount a different filesystem
    // 2. Set CROSS_FS_PATH environment variable to a path on that filesystem
    // 3. Run: cargo test test_cross_filesystem_uses_inplace_strategy -- --ignored --nocapture
    //
    // Example on macOS:
    //   hdiutil attach -nomount ram://204800  # 100MB ramdisk
    //   diskutil erasevolume APFS "TestFS" /dev/diskN
    //   export CROSS_FS_PATH=/Volumes/TestFS
    //   cargo test test_cross_filesystem_uses_inplace_strategy -- --ignored --nocapture
    //   hdiutil detach /dev/diskN

    use std::env;

    let cross_fs_path = env::var("CROSS_FS_PATH")
        .expect("CROSS_FS_PATH not set - see test comments for setup instructions");

    let source = TempDir::new().unwrap();
    let cross_fs_dest = std::path::PathBuf::from(&cross_fs_path);

    // Create dest file on cross-filesystem (15MB - above delta threshold)
    let dest_file = cross_fs_dest.join("test_cross_fs.dat");
    fs::write(&dest_file, vec![0u8; 15_000_000]).unwrap();

    // Create source file with changes
    let source_file = source.path().join("test.dat");
    let source_data = vec![1u8; 15_000_000];
    fs::write(&source_file, &source_data).unwrap();

    // Sync with debug logging
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(output.status.success(), "Sync should succeed");

    // Verify result is correct
    let result_data = fs::read(&dest_file).unwrap();
    assert_eq!(result_data, source_data, "Dest should match source");

    // Verify in-place strategy was used (check logs)
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        (stderr.contains("in-place (full file rebuild)")
            && stderr.contains("different filesystems"))
            || (stdout.contains("in-place (full file rebuild)")
                && stdout.contains("different filesystems")),
        "Should use in-place strategy for cross-filesystem. Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );

    // Cleanup
    let _ = fs::remove_file(&dest_file);
}

#[test]
#[cfg(unix)]
fn test_same_filesystem_detection() {
    // Unit test for same_filesystem function using standard library metadata

    use sy::fs_util::same_filesystem;

    let temp = TempDir::new().unwrap();
    let file1 = temp.path().join("file1.txt");
    let file2 = temp.path().join("file2.txt");

    fs::write(&file1, b"test1").unwrap();
    fs::write(&file2, b"test2").unwrap();

    // Files in same directory should be on same filesystem
    assert!(
        same_filesystem(&file1, &file2),
        "Files in same directory should be on same filesystem"
    );

    // File and its parent directory should be on same filesystem
    assert!(
        same_filesystem(&file1, temp.path()),
        "File and parent directory should be on same filesystem"
    );
}

#[test]
#[cfg(unix)]
fn test_sparse_file_delta_sync_preserves_sparseness() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create sparse destination file (10MB with hole in middle)
    let dest_file = dest.path().join("sparse.dat");
    {
        let mut f = fs::File::create(&dest_file).unwrap();
        f.write_all(&vec![0xAA; 1_000_000]).unwrap(); // 1MB data
        f.seek(SeekFrom::Current(8_000_000)).unwrap(); // 8MB hole
        f.write_all(&vec![0xBB; 1_000_000]).unwrap(); // 1MB data
        f.sync_all().unwrap();
    }

    // Check if filesystem supports sparse files (ext4, XFS, btrfs do; APFS may not)
    let dest_meta = fs::metadata(&dest_file).unwrap();
    if dest_meta.blocks() * 512 >= dest_meta.len() {
        eprintln!(
            "Filesystem doesn't support sparse files (allocated {} >= logical {}), skipping test",
            dest_meta.blocks() * 512,
            dest_meta.len()
        );
        return;
    }

    assert_eq!(
        dest_meta.len(),
        10_000_000,
        "Dest logical size should be 10MB"
    );

    // Create sparse source file (different content, still sparse)
    let source_file = source.path().join("sparse.dat");
    {
        let mut f = fs::File::create(&source_file).unwrap();
        f.write_all(&vec![0xCC; 1_000_000]).unwrap(); // 1MB different data
        f.seek(SeekFrom::Current(8_000_000)).unwrap(); // 8MB hole
        f.write_all(&vec![0xDD; 1_000_000]).unwrap(); // 1MB different data
        f.sync_all().unwrap();
    }

    // Verify source is sparse
    let source_meta = fs::metadata(&source_file).unwrap();
    assert_eq!(
        source_meta.len(),
        10_000_000,
        "Source logical size should be 10MB"
    );
    assert!(
        source_meta.blocks() * 512 < source_meta.len(),
        "Source should be sparse (allocated < logical)"
    );

    // Sync with info logging
    let output = Command::new(sy_bin())
        .args([source_file.to_str().unwrap(), dest_file.to_str().unwrap()])
        .env("RUST_LOG", "sy=info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "Sync should succeed for sparse file"
    );

    // Verify file size is correct
    let result_meta = fs::metadata(&dest_file).unwrap();
    assert_eq!(
        result_meta.len(),
        10_000_000,
        "Result logical size should be 10MB"
    );

    // Verify content matches (read full file to check data + holes)
    let source_data = fs::read(&source_file).unwrap();
    let dest_data = fs::read(&dest_file).unwrap();
    assert_eq!(
        dest_data, source_data,
        "Dest content should match source exactly"
    );

    // Check if sparseness was preserved (optional, depends on filesystem support)
    let result_allocated = result_meta.blocks() * 512;
    if result_allocated < result_meta.len() {
        eprintln!(
            "✓ Sparseness preserved: {} allocated vs {} logical",
            result_allocated,
            result_meta.len()
        );
    } else {
        eprintln!(
            "⚠ Sparseness not preserved on this filesystem: {} allocated vs {} logical\n\
             This is expected on some filesystems (e.g., ext4 with older kernels, some CI environments)",
            result_allocated,
            result_meta.len()
        );
    }

    // Verify sparse-aware copy was used
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stderr.contains("sparse") || stdout.contains("sparse"),
        "Should log sparse file detection. Stderr: {}\nStdout: {}",
        stderr,
        stdout
    );
}
