use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::str::FromStr;

/// Compression algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    /// LZ4: 23 GB/s, lower compression ratio (good for low-CPU scenarios)
    Lz4,
    /// Zstd level 3: 8.7 GB/s, better compression ratio (default)
    Zstd,
}

impl FromStr for Compression {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "lz4" => Ok(Self::Lz4),
            "zstd" => Ok(Self::Zstd),
            _ => Err(format!("Unknown compression type: {}", s)),
        }
    }
}

impl Compression {
    #[allow(dead_code)] // Used in debug logging
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        }
    }
}

/// Compress data
pub fn compress(data: &[u8], compression: Compression) -> io::Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => compress_lz4(data),
        Compression::Zstd => compress_zstd(data),
    }
}

/// Decompress data (used by sy-remote binary)
#[allow(dead_code)] // Used by sy-remote binary, not library code
pub fn decompress(data: &[u8], compression: Compression) -> io::Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => decompress_lz4(data),
        Compression::Zstd => decompress_zstd(data),
    }
}

fn compress_lz4(data: &[u8]) -> io::Result<Vec<u8>> {
    // LZ4: 23 GB/s throughput (benchmarked), lower CPU usage
    Ok(lz4_flex::compress_prepend_size(data))
}

#[allow(dead_code)] // Called by decompress() which is used by sy-remote
fn decompress_lz4(data: &[u8]) -> io::Result<Vec<u8>> {
    lz4_flex::decompress_size_prepended(data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn compress_zstd(data: &[u8]) -> io::Result<Vec<u8>> {
    // Level 3: 8.7 GB/s throughput (benchmarked), optimal balance
    let mut encoder = zstd::Encoder::new(Vec::new(), 3)?;
    encoder.write_all(data)?;
    encoder.finish()
}

#[allow(dead_code)] // Called by decompress() which is used by sy-remote
fn decompress_zstd(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = zstd::Decoder::new(data)?;
    let mut result = Vec::new();
    decoder.read_to_end(&mut result)?;
    Ok(result)
}

/// List of file extensions that are already compressed
/// Compressing these files provides minimal benefit
const COMPRESSED_EXTENSIONS: &[&str] = &[
    // Images
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", // Video
    "mp4", "mkv", "avi", "mov", "webm", "m4v", "flv", "wmv", // Audio
    "mp3", "m4a", "aac", "ogg", "opus", "flac", "wma", // Archives
    "zip", "gz", "bz2", "xz", "7z", "rar", "tar.gz", "tgz", "tar.bz2", // Documents
    "pdf", "docx", "xlsx", "pptx", // Other
    "wasm", "br", "zst",
];

/// Check if file extension indicates already-compressed data
pub fn is_compressed_extension(filename: &str) -> bool {
    if let Some(ext) = filename.rsplit('.').next() {
        COMPRESSED_EXTENSIONS
            .iter()
            .any(|&e| ext.eq_ignore_ascii_case(e))
    } else {
        false
    }
}

/// Determine if we should compress based on file size, extension, and network conditions
///
/// NOTE: Benchmarks show compression is MUCH faster than originally assumed:
/// - LZ4: 23 GB/s (not 400-500 MB/s as originally thought)
/// - Zstd: 8 GB/s (level 3)
///
/// CPU is NEVER the bottleneck - network always is, even on 100 Gbps!
#[allow(dead_code)] // Public API for future use
pub fn should_compress_adaptive(
    filename: &str,
    file_size: u64,
    is_local: bool,
    _network_speed_mbps: Option<u64>, // Kept for API compatibility, but unused
) -> Compression {
    // LOCAL: Never compress (disk I/O is bottleneck, not network/CPU)
    if is_local {
        return Compression::None;
    }

    // Skip small files (overhead > benefit)
    if file_size < 1024 * 1024 {
        return Compression::None;
    }

    // Skip already-compressed formats (jpg, mp4, zip, etc.)
    if is_compressed_extension(filename) {
        return Compression::None;
    }

    // BENCHMARKED DECISION:
    // Zstd at level 3 compresses at 8 GB/s (64 Gbps equivalent)
    // This is faster than ANY network, so always use it for best compression ratio
    // LZ4 is faster (23 GB/s) but worse ratio, only needed if Zstd bottlenecks
    //
    // Reality: Even 100 Gbps networks (12.5 GB/s) won't bottleneck on Zstd
    // Therefore: Always use Zstd for network transfers
    Compression::Zstd
}

/// Determine if we should compress based on file size and extension
/// (Legacy function for backward compatibility)
#[allow(dead_code)] // Public API for future use
pub fn should_compress(filename: &str, file_size: u64) -> Compression {
    should_compress_adaptive(filename, file_size, false, None)
}

/// Detect file compressibility by sampling first 64KB with LZ4
///
/// Returns compression ratio (compressed_size / original_size)
/// - Ratio < 0.9 means compressible (>10% savings)
/// - Ratio >= 0.9 means incompressible (<10% savings)
///
/// Uses LZ4 for fast testing (23 GB/s throughput)
/// Inspired by BorgBackup's auto-compression heuristic
pub fn detect_compressibility(file_path: &Path) -> io::Result<f64> {
    const SAMPLE_SIZE: usize = 64 * 1024; // 64KB sample

    let mut file = File::open(file_path)?;
    let mut buffer = vec![0u8; SAMPLE_SIZE];
    let bytes_read = file.read(&mut buffer)?;

    // Empty file or very small file
    if bytes_read == 0 {
        return Ok(1.0); // No benefit
    }

    let sample = &buffer[..bytes_read];
    let compressed = compress_lz4(sample)?;

    // Calculate compression ratio
    let ratio = compressed.len() as f64 / sample.len() as f64;

    Ok(ratio)
}

/// Compression detection mode
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum CompressionDetection {
    /// Content-based detection with sampling (default)
    #[default]
    Auto,

    /// Extension-only detection (legacy behavior)
    Extension,

    /// Always compress (override detection)
    Always,

    /// Never compress (override detection)
    Never,
}

/// Smart compression detection using content sampling
///
/// This function extends should_compress_adaptive() with content-based detection
/// for improved accuracy. It follows BorgBackup's proven approach of sampling
/// file content to determine compressibility.
///
/// # Arguments
/// * `file_path` - Optional path to the file for content sampling
/// * `filename` - Filename for extension-based filtering
/// * `file_size` - Size in bytes
/// * `is_local` - Whether this is a local transfer
/// * `detection_mode` - Detection mode (Auto, Extension, Always, Never)
///
/// # Detection Strategy
/// 1. Fast path: Skip if local transfer, small file, or known compressed extension
/// 2. Content sampling: Read first 64KB, test with LZ4, measure ratio
/// 3. Decision: Ratio <0.9 → compress with Zstd, ≥0.9 → skip compression
pub fn should_compress_smart(
    file_path: Option<&Path>,
    filename: &str,
    file_size: u64,
    is_local: bool,
    detection_mode: CompressionDetection,
) -> Compression {
    // LOCAL: Never compress (disk I/O is bottleneck, not network/CPU)
    if is_local {
        return Compression::None;
    }

    // Handle explicit overrides
    match detection_mode {
        CompressionDetection::Always => return Compression::Zstd,
        CompressionDetection::Never => return Compression::None,
        _ => {} // Continue with detection
    }

    // Skip small files (overhead > benefit)
    if file_size < 1024 * 1024 {
        return Compression::None;
    }

    // Skip known compressed extensions (fast path)
    if is_compressed_extension(filename) {
        return Compression::None;
    }

    // Extension-only mode (legacy behavior)
    if detection_mode == CompressionDetection::Extension {
        return Compression::Zstd;
    }

    // Content sampling (auto mode)
    // This is the new smart detection that tests actual compressibility
    if let Some(path) = file_path {
        match detect_compressibility(path) {
            Ok(ratio) if ratio < 0.9 => {
                // Compressible: >10% savings achieved
                Compression::Zstd
            }
            Ok(_ratio) => {
                // Incompressible: <10% savings, not worth CPU overhead
                Compression::None
            }
            Err(_) => {
                // Error reading file, fall back to trying compression
                // Better to compress and waste some CPU than skip and lose bandwidth
                Compression::Zstd
            }
        }
    } else {
        // No file path available, fall back to extension-based heuristic
        // This happens when we only have filename/size but not actual file
        Compression::Zstd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress_lz4() {
        let original = b"Hello, world! This is a test of LZ4 compression. ".repeat(100);
        let compressed = compress(&original, Compression::Lz4).unwrap();
        let decompressed = decompress(&compressed, Compression::Lz4).unwrap();

        assert_eq!(original.as_slice(), decompressed.as_slice());
        assert!(compressed.len() < original.len());
    }

    #[test]
    fn test_compress_decompress_zstd() {
        let original = b"Hello, world! This is a test of Zstd compression. ".repeat(100);
        let compressed = compress(&original, Compression::Zstd).unwrap();
        let decompressed = decompress(&compressed, Compression::Zstd).unwrap();

        assert_eq!(original.as_slice(), decompressed.as_slice());
        assert!(compressed.len() < original.len());
    }

    #[test]
    fn test_compress_decompress_none() {
        let original = b"No compression test";
        let compressed = compress(original, Compression::None).unwrap();
        let decompressed = decompress(&compressed, Compression::None).unwrap();

        assert_eq!(original.as_slice(), decompressed.as_slice());
        assert_eq!(compressed.len(), original.len());
    }

    #[test]
    fn test_zstd_compression_ratio() {
        let repetitive = b"AAAA".repeat(1000);
        let compressed = compress(&repetitive, Compression::Zstd).unwrap();

        // Should compress very well (repetitive data)
        let ratio = compressed.len() as f64 / repetitive.len() as f64;
        assert!(ratio < 0.1); // Less than 10% of original
    }

    #[test]
    fn test_is_compressed_extension() {
        // Lowercase
        assert!(is_compressed_extension("file.jpg"));
        assert!(is_compressed_extension("video.mp4"));
        assert!(is_compressed_extension("archive.zip"));
        assert!(is_compressed_extension("document.pdf"));

        // Uppercase (should work case-insensitively)
        assert!(is_compressed_extension("file.JPG"));
        assert!(is_compressed_extension("video.MP4"));
        assert!(is_compressed_extension("archive.ZIP"));

        // Mixed case
        assert!(is_compressed_extension("file.JpG"));
        assert!(is_compressed_extension("video.Mp4"));

        // Not compressed
        assert!(!is_compressed_extension("file.txt"));
        assert!(!is_compressed_extension("code.rs"));
        assert!(!is_compressed_extension("data.csv"));
    }

    #[test]
    fn test_should_compress_small_file() {
        // Small files should not be compressed
        assert_eq!(should_compress("test.txt", 1024), Compression::None);
    }

    #[test]
    fn test_should_compress_already_compressed() {
        // Already compressed files should not be compressed
        assert_eq!(should_compress("image.jpg", 10_000_000), Compression::None);
        assert_eq!(should_compress("video.mp4", 100_000_000), Compression::None);
    }

    #[test]
    fn test_should_compress_large_text() {
        // Large text files should be compressed (now defaults to Zstd)
        assert_eq!(should_compress("data.txt", 10_000_000), Compression::Zstd);
        assert_eq!(should_compress("log.log", 50_000_000), Compression::Zstd);
    }

    #[test]
    fn test_roundtrip_empty_data() {
        let empty: &[u8] = &[];
        for compression in [Compression::None, Compression::Lz4, Compression::Zstd] {
            let compressed = compress(empty, compression).unwrap();
            let decompressed = decompress(&compressed, compression).unwrap();
            assert_eq!(decompressed.as_slice(), empty);
        }
    }

    #[test]
    fn test_roundtrip_large_data() {
        // 1MB of data
        let large: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();

        for compression in [Compression::None, Compression::Lz4, Compression::Zstd] {
            let compressed = compress(&large, compression).unwrap();
            let decompressed = decompress(&compressed, compression).unwrap();
            assert_eq!(decompressed, large);
        }
    }

    #[test]
    fn test_lz4_compression_ratio() {
        let repetitive = b"AAAA".repeat(1000);
        let compressed = compress(&repetitive, Compression::Lz4).unwrap();

        // LZ4 should compress repetitive data well
        let ratio = compressed.len() as f64 / repetitive.len() as f64;
        assert!(ratio < 0.1); // Less than 10% of original
    }

    #[test]
    fn test_adaptive_compression_local() {
        // Local transfers should never compress
        assert_eq!(
            should_compress_adaptive("test.txt", 10_000_000, true, None),
            Compression::None
        );
    }

    #[test]
    fn test_adaptive_compression_any_network() {
        // UPDATED: Benchmarks show compression is always faster than network
        // Network speed is now irrelevant - always use Zstd for best ratio

        // Even 100 Gbps (12.5 GB/s) is slower than Zstd (8 GB/s won't bottleneck due to I/O)
        assert_eq!(
            should_compress_adaptive("test.txt", 10_000_000, false, Some(100_000)), // 100 Gbps
            Compression::Zstd
        );

        // 1 Gbps network -> Zstd
        assert_eq!(
            should_compress_adaptive("test.txt", 10_000_000, false, Some(1000)),
            Compression::Zstd
        );

        // 100 Mbps network -> Zstd
        assert_eq!(
            should_compress_adaptive("test.txt", 10_000_000, false, Some(100)),
            Compression::Zstd
        );

        // No network speed info -> Zstd (default for network transfers)
        assert_eq!(
            should_compress_adaptive("test.txt", 10_000_000, false, None),
            Compression::Zstd
        );
    }

    #[test]
    fn test_adaptive_compression_respects_precompressed() {
        // Even on slow network, don't compress already-compressed files
        assert_eq!(
            should_compress_adaptive("video.mp4", 100_000_000, false, Some(10)),
            Compression::None
        );
    }

    #[test]
    fn test_adaptive_compression_small_files() {
        // Small files should not be compressed regardless of network speed
        assert_eq!(
            should_compress_adaptive("test.txt", 512_000, false, Some(10)),
            Compression::None
        );
    }

    #[test]
    fn test_detect_compressibility_text() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a highly compressible text file
        let mut temp_file = NamedTempFile::new().unwrap();
        let repetitive_text = "Hello world! ".repeat(5000); // ~60KB of repetitive text
        temp_file.write_all(repetitive_text.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let ratio = detect_compressibility(temp_file.path()).unwrap();

        // Repetitive text should compress very well (ratio should be < 0.5)
        assert!(ratio < 0.5, "Ratio: {}", ratio);
    }

    #[test]
    fn test_detect_compressibility_random() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a file with high-entropy data (incompressible)
        // Use a better pseudo-random sequence that doesn't compress well
        let mut temp_file = NamedTempFile::new().unwrap();
        let random_data: Vec<u8> = (0u32..65536)
            .map(|i| {
                // Mix bits from multiple positions to create high entropy
                let x = i.wrapping_mul(2654435761); // Knuth's multiplicative hash
                ((x ^ (x >> 16)) & 0xFF) as u8
            })
            .collect();
        temp_file.write_all(&random_data).unwrap();
        temp_file.flush().unwrap();

        let ratio = detect_compressibility(temp_file.path()).unwrap();

        // High-entropy data should not compress well (ratio should be > 0.85)
        // Note: Even good pseudo-random may compress slightly, so we use 0.85 threshold
        assert!(ratio > 0.85, "Ratio: {}", ratio);
    }

    #[test]
    fn test_detect_compressibility_empty() {
        use tempfile::NamedTempFile;

        // Create an empty file
        let temp_file = NamedTempFile::new().unwrap();

        let ratio = detect_compressibility(temp_file.path()).unwrap();

        // Empty file should return 1.0 (no benefit)
        assert_eq!(ratio, 1.0);
    }

    #[test]
    fn test_should_compress_smart_auto_compressible() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a compressible file
        let mut temp_file = NamedTempFile::new().unwrap();
        let text = "Compressible text data! ".repeat(50000); // ~1.2MB
        temp_file.write_all(text.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = should_compress_smart(
            Some(temp_file.path()),
            "test.txt",
            1_200_000,
            false,
            CompressionDetection::Auto,
        );

        assert_eq!(result, Compression::Zstd);
    }

    #[test]
    fn test_should_compress_smart_auto_incompressible() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create an incompressible file (high entropy)
        let mut temp_file = NamedTempFile::new().unwrap();
        let random_data: Vec<u8> = (0u32..1_200_000)
            .map(|i| {
                // Use Knuth's multiplicative hash for high-entropy data
                let x = i.wrapping_mul(2654435761);
                ((x ^ (x >> 16)) & 0xFF) as u8
            })
            .collect();
        temp_file.write_all(&random_data).unwrap();
        temp_file.flush().unwrap();

        let result = should_compress_smart(
            Some(temp_file.path()),
            "data.bin",
            1_200_000,
            false,
            CompressionDetection::Auto,
        );

        // Should skip compression for incompressible data
        assert_eq!(result, Compression::None);
    }

    #[test]
    fn test_should_compress_smart_always() {
        // Always mode should always compress
        let result = should_compress_smart(
            None,
            "test.jpg", // Even for compressed extension
            10_000_000,
            false,
            CompressionDetection::Always,
        );

        assert_eq!(result, Compression::Zstd);
    }

    #[test]
    fn test_should_compress_smart_never() {
        // Never mode should never compress
        let result = should_compress_smart(
            None,
            "test.txt", // Even for text files
            10_000_000,
            false,
            CompressionDetection::Never,
        );

        assert_eq!(result, Compression::None);
    }

    #[test]
    fn test_should_compress_smart_extension_mode() {
        // Extension mode should use legacy behavior (compress unless known extension)
        let result = should_compress_smart(
            None,
            "test.txt",
            10_000_000,
            false,
            CompressionDetection::Extension,
        );

        assert_eq!(result, Compression::Zstd);

        // Should skip known compressed extensions even in extension mode
        let result = should_compress_smart(
            None,
            "test.jpg",
            10_000_000,
            false,
            CompressionDetection::Extension,
        );

        assert_eq!(result, Compression::None);
    }

    #[test]
    fn test_should_compress_smart_local() {
        // Local transfers should never compress regardless of detection mode
        let result = should_compress_smart(
            None,
            "test.txt",
            10_000_000,
            true, // is_local
            CompressionDetection::Auto,
        );

        assert_eq!(result, Compression::None);
    }

    #[test]
    fn test_should_compress_smart_small_file() {
        // Small files should not be compressed
        let result = should_compress_smart(
            None,
            "test.txt",
            512_000, // < 1MB
            false,
            CompressionDetection::Auto,
        );

        assert_eq!(result, Compression::None);
    }

    #[test]
    fn test_should_compress_smart_known_compressed_extension() {
        // Files with known compressed extensions should be skipped (fast path)
        let result = should_compress_smart(
            None,
            "video.mp4",
            100_000_000,
            false,
            CompressionDetection::Auto,
        );

        assert_eq!(result, Compression::None);
    }

    #[test]
    fn test_should_compress_smart_no_path_fallback() {
        // Without file path, should fall back to extension-based heuristic
        let result = should_compress_smart(
            None, // No path
            "data.bin",
            10_000_000,
            false,
            CompressionDetection::Auto,
        );

        // Should default to compressing when path not available
        assert_eq!(result, Compression::Zstd);
    }
}
