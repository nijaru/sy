use std::fs;
use std::process::Command;
use sy::compress::{compress, decompress, should_compress_adaptive, Compression};
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir() -> (TempDir, TempDir) {
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
fn test_compression_end_to_end() {
    // Simulate file transfer with compression
    let source_dir = TempDir::new().unwrap();
    let dest_dir = TempDir::new().unwrap();

    // Create test file with compressible data (>1MB to trigger compression)
    let source_file = source_dir.path().join("test.txt");
    let test_data = "This is test data that should compress well. ".repeat(25_000); // ~1.2 MB
    fs::write(&source_file, &test_data).unwrap();

    let file_size = test_data.len() as u64;

    // 1. Decide if we should compress
    let compression = should_compress_adaptive(
        source_file.to_str().unwrap(),
        file_size,
        false, // Not local (simulate network)
        None,  // No speed info
    );

    assert_eq!(
        compression,
        Compression::Zstd,
        "Should use Zstd for network transfers"
    );

    // 2. Read source file
    let source_data = fs::read(&source_file).unwrap();

    // 3. Compress data
    let compressed = compress(&source_data, compression).unwrap();

    // Verify compression actually reduces size
    let compression_ratio = compressed.len() as f64 / source_data.len() as f64;
    assert!(
        compression_ratio < 0.5,
        "Should compress to less than 50% for repetitive data"
    );

    // 4. Simulate transfer (just write compressed data)
    let dest_file = dest_dir.path().join("test.txt.compressed");
    fs::write(&dest_file, &compressed).unwrap();

    // 5. Read transferred data
    let transferred = fs::read(&dest_file).unwrap();

    // 6. Decompress
    let decompressed = decompress(&transferred, compression).unwrap();

    // 7. Verify correctness
    assert_eq!(
        decompressed, source_data,
        "Decompressed data should match original"
    );
}

#[test]
fn test_compression_skip_precompressed() {
    // Verify we don't compress already-compressed files
    let compression = should_compress_adaptive(
        "video.mp4",
        10_000_000, // 10 MB
        false,      // Network transfer
        None,
    );

    assert_eq!(
        compression,
        Compression::None,
        "Should not compress mp4 files"
    );
}

#[test]
fn test_compression_skip_small_files() {
    // Verify we don't compress small files
    let compression = should_compress_adaptive(
        "small.txt",
        512_000, // 500 KB
        false,   // Network transfer
        None,
    );

    assert_eq!(
        compression,
        Compression::None,
        "Should not compress files < 1MB"
    );
}

#[test]
fn test_compression_skip_local() {
    // Verify we don't compress local transfers
    let compression = should_compress_adaptive(
        "large.txt",
        10_000_000, // 10 MB
        true,       // Local transfer
        None,
    );

    assert_eq!(
        compression,
        Compression::None,
        "Should not compress local transfers"
    );
}

#[test]
fn test_cli_compression_end_to_end() {
    let (source, dest) = setup_test_dir();

    // Create compressible content
    let content = "Hello World\n".repeat(10000);
    fs::write(source.path().join("compressible.txt"), &content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--compress=auto",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("compressible.txt")).unwrap(),
        content
    );
}

#[test]
fn test_cli_compression_skip_small_files() {
    let (source, dest) = setup_test_dir();

    // Create small file (below compression threshold)
    fs::write(source.path().join("small.txt"), "tiny").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--compress=auto",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("small.txt")).unwrap(),
        "tiny"
    );
}

#[test]
fn test_cli_compression_skip_local() {
    let (source, dest) = setup_test_dir();

    // Create file
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Compression should be skipped for local sync
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--exclude-vcs",
            "--compress=auto",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dest.path().join("file.txt")).unwrap(),
        "content"
    );
}
