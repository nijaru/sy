use std::io::{self, Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Lz4,
    Zstd,
}

impl Compression {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "none" => Some(Self::None),
            "lz4" => Some(Self::Lz4),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        }
    }
}

/// Compress data using the specified algorithm
pub fn compress(data: &[u8], compression: Compression) -> io::Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => compress_lz4(data),
        Compression::Zstd => compress_zstd(data),
    }
}

/// Decompress data using the specified algorithm
pub fn decompress(data: &[u8], compression: Compression) -> io::Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => decompress_lz4(data),
        Compression::Zstd => decompress_zstd(data),
    }
}

fn compress_lz4(data: &[u8]) -> io::Result<Vec<u8>> {
    Ok(lz4_flex::compress_prepend_size(data))
}

fn decompress_lz4(data: &[u8]) -> io::Result<Vec<u8>> {
    lz4_flex::decompress_size_prepended(data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn compress_zstd(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = zstd::Encoder::new(Vec::new(), 3)?; // Level 3 (balanced)
    encoder.write_all(data)?;
    encoder.finish()
}

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
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif",
    // Video
    "mp4", "mkv", "avi", "mov", "webm", "m4v", "flv", "wmv",
    // Audio
    "mp3", "m4a", "aac", "ogg", "opus", "flac", "wma",
    // Archives
    "zip", "gz", "bz2", "xz", "7z", "rar", "tar.gz", "tgz", "tar.bz2",
    // Documents
    "pdf", "docx", "xlsx", "pptx",
    // Other
    "wasm", "br", "zst",
];

/// Check if file extension indicates already-compressed data
pub fn is_compressed_extension(filename: &str) -> bool {
    if let Some(ext) = filename.rsplit('.').next() {
        COMPRESSED_EXTENSIONS.contains(&ext.to_lowercase().as_str())
    } else {
        false
    }
}

/// Determine if we should compress based on file size and extension
pub fn should_compress(filename: &str, file_size: u64) -> Compression {
    // Skip small files (overhead > benefit)
    if file_size < 1024 * 1024 {
        return Compression::None;
    }

    // Skip already-compressed formats
    if is_compressed_extension(filename) {
        return Compression::None;
    }

    // Default to LZ4 for now (fast, good compression ratio)
    // TODO: Add network speed detection and adaptive compression
    Compression::Lz4
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
    fn test_lz4_better_ratio_for_repetitive_data() {
        let repetitive = b"AAAA".repeat(1000);
        let compressed = compress(&repetitive, Compression::Lz4).unwrap();

        // Should compress very well (repetitive data)
        let ratio = compressed.len() as f64 / repetitive.len() as f64;
        assert!(ratio < 0.1); // Less than 10% of original
    }

    #[test]
    fn test_zstd_better_compression_than_lz4() {
        let data = b"The quick brown fox jumps over the lazy dog. ".repeat(100);
        let lz4_compressed = compress(&data, Compression::Lz4).unwrap();
        let zstd_compressed = compress(&data, Compression::Zstd).unwrap();

        // Zstd should achieve better compression (trades speed for ratio)
        assert!(zstd_compressed.len() <= lz4_compressed.len());
    }

    #[test]
    fn test_is_compressed_extension() {
        assert!(is_compressed_extension("file.jpg"));
        assert!(is_compressed_extension("video.mp4"));
        assert!(is_compressed_extension("archive.zip"));
        assert!(is_compressed_extension("document.pdf"));

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
        // Large text files should be compressed
        assert_eq!(should_compress("data.txt", 10_000_000), Compression::Lz4);
        assert_eq!(should_compress("log.log", 50_000_000), Compression::Lz4);
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
}
