use std::fs;
use sy::compress::{compress, decompress, should_compress_adaptive, Compression};
use tempfile::TempDir;

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
    println!(
        "Compression ratio: {:.1}% ({}KB -> {}KB)",
        compression_ratio * 100.0,
        source_data.len() / 1024,
        compressed.len() / 1024
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
    println!("✓ Compression integration test passed");
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
fn test_compression_performance() {
    use std::time::Instant;

    // Create 10MB of compressible data
    let data: Vec<u8> = "ABCDEFGH".repeat(1_250_000).into_bytes();

    // Benchmark Zstd compression
    let start = Instant::now();
    let compressed = compress(&data, Compression::Zstd).unwrap();
    let duration = start.elapsed();

    let throughput = (data.len() as f64 / duration.as_secs_f64()) / 1_000_000_000.0; // GB/s
    let ratio = compressed.len() as f64 / data.len() as f64;

    println!("Zstd Performance:");
    println!("  Throughput: {:.2} GB/s", throughput);
    println!("  Ratio: {:.1}%", ratio * 100.0);
    println!(
        "  Size: {} KB -> {} KB",
        data.len() / 1024,
        compressed.len() / 1024
    );

    // Verify it's reasonably fast (relaxed threshold for CI environments)
    // Local benchmarks show 8+ GB/s, but CI runners vary (0.5-2 GB/s)
    assert!(
        throughput > 0.3,
        "Zstd should compress at >0.3 GB/s, got {:.2} GB/s",
        throughput
    );
}
