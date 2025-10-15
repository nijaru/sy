#[allow(dead_code)] // Public API and hasher infrastructure
use crate::error::Result;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use xxhash_rust::xxh3::Xxh3;

/// Wrapper around xxHash3 hasher
pub struct XxHash3Hasher;

#[allow(dead_code)] // Public API with tested methods
impl XxHash3Hasher {
    /// Compute xxHash3 hash of a file
    ///
    /// This reads the entire file and computes its hash.
    /// xxHash3 is much faster than cryptographic hashes but not suitable for security.
    pub fn hash_file(path: &Path) -> Result<u64> {
        let mut file = File::open(path)?;
        let mut hasher = Xxh3::new();

        // Read and hash in chunks to avoid loading entire file into memory
        let mut buffer = vec![0u8; 1024 * 1024]; // 1MB chunks
        loop {
            let bytes_read = file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        Ok(hasher.digest())
    }

    /// Compute xxHash3 hash of data in memory
    pub fn hash_data(data: &[u8]) -> u64 {
        xxhash_rust::xxh3::xxh3_64(data)
    }

    /// Create a new incremental hasher (for streaming)
    pub fn new_hasher() -> Xxh3 {
        Xxh3::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_hash_data() {
        let data = b"Hello, xxHash3!";
        let hash = XxHash3Hasher::hash_data(data);
        assert_ne!(hash, 0); // xxHash3 produces non-zero hash for non-empty data
    }

    #[test]
    fn test_hash_data_deterministic() {
        let data = b"Test data";
        let hash1 = XxHash3Hasher::hash_data(data);
        let hash2 = XxHash3Hasher::hash_data(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_data_different() {
        let hash1 = XxHash3Hasher::hash_data(b"Data 1");
        let hash2 = XxHash3Hasher::hash_data(b"Data 2");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let content = b"File content for xxHash3";
        fs::write(&file_path, content).unwrap();

        let hash = XxHash3Hasher::hash_file(&file_path).unwrap();
        assert_ne!(hash, 0);

        // Should match hash of raw data
        let data_hash = XxHash3Hasher::hash_data(content);
        assert_eq!(hash, data_hash);
    }

    #[test]
    fn test_hash_large_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("large.txt");

        // Create 10MB file
        let chunk = vec![0x42u8; 1024 * 1024]; // 1MB
        let mut file = File::create(&file_path).unwrap();
        use std::io::Write;
        for _ in 0..10 {
            file.write_all(&chunk).unwrap();
        }
        drop(file);

        // Hash it
        let hash = XxHash3Hasher::hash_file(&file_path).unwrap();
        assert_ne!(hash, 0);
    }

    #[test]
    fn test_incremental_hasher() {
        let mut hasher = XxHash3Hasher::new_hasher();
        hasher.update(b"Hello, ");
        hasher.update(b"world!");
        let hash1 = hasher.digest();

        let hash2 = XxHash3Hasher::hash_data(b"Hello, world!");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_empty_data() {
        let hash = XxHash3Hasher::hash_data(b"");
        // xxHash3 has a specific hash for empty input
        assert_eq!(hash, 0x2d06800538d394c2);
    }

    #[test]
    fn test_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("empty.txt");
        fs::write(&file_path, b"").unwrap();

        let file_hash = XxHash3Hasher::hash_file(&file_path).unwrap();
        let data_hash = XxHash3Hasher::hash_data(b"");
        assert_eq!(file_hash, data_hash);
    }

    #[test]
    fn test_known_hash() {
        // Test with known input/output for regression testing
        let data = b"The quick brown fox jumps over the lazy dog";
        let hash = XxHash3Hasher::hash_data(data);
        // This is the known xxHash3 value for this input
        assert_eq!(hash, 0xce7d19a5418fb365);
    }
}
