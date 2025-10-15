#[allow(dead_code)] // Public API and hasher infrastructure
use crate::error::Result;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Wrapper around BLAKE3 hasher
pub struct Blake3Hasher;

#[allow(dead_code)] // Public API with tested methods
impl Blake3Hasher {
    /// Compute BLAKE3 hash of a file
    ///
    /// This reads the entire file and computes its hash.
    /// For large files, this may use significant memory.
    pub fn hash_file(path: &Path) -> Result<blake3::Hash> {
        let mut file = File::open(path)?;
        let mut hasher = blake3::Hasher::new();

        // Read and hash in chunks to avoid loading entire file into memory
        let mut buffer = vec![0u8; 1024 * 1024]; // 1MB chunks
        loop {
            let bytes_read = file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        Ok(hasher.finalize())
    }

    /// Compute BLAKE3 hash of data in memory
    pub fn hash_data(data: &[u8]) -> blake3::Hash {
        blake3::hash(data)
    }

    /// Compute BLAKE3 hash incrementally (for streaming)
    pub fn new_hasher() -> blake3::Hasher {
        blake3::Hasher::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_hash_data() {
        let data = b"Hello, BLAKE3!";
        let hash = Blake3Hasher::hash_data(data);
        assert_eq!(hash.as_bytes().len(), 32); // BLAKE3 produces 32-byte hashes
    }

    #[test]
    fn test_hash_data_deterministic() {
        let data = b"Test data";
        let hash1 = Blake3Hasher::hash_data(data);
        let hash2 = Blake3Hasher::hash_data(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_data_different() {
        let hash1 = Blake3Hasher::hash_data(b"Data 1");
        let hash2 = Blake3Hasher::hash_data(b"Data 2");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let content = b"File content for BLAKE3";
        fs::write(&file_path, content).unwrap();

        let hash = Blake3Hasher::hash_file(&file_path).unwrap();
        assert_eq!(hash.as_bytes().len(), 32);

        // Should match hash of raw data
        let data_hash = Blake3Hasher::hash_data(content);
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
        let hash = Blake3Hasher::hash_file(&file_path).unwrap();
        assert_eq!(hash.as_bytes().len(), 32);
    }

    #[test]
    fn test_incremental_hasher() {
        let mut hasher = Blake3Hasher::new_hasher();
        hasher.update(b"Hello, ");
        hasher.update(b"world!");
        let hash1 = hasher.finalize();

        let hash2 = Blake3Hasher::hash_data(b"Hello, world!");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_empty_data() {
        let hash = Blake3Hasher::hash_data(b"");
        assert_eq!(hash.as_bytes().len(), 32);
    }

    #[test]
    fn test_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("empty.txt");
        fs::write(&file_path, b"").unwrap();

        let file_hash = Blake3Hasher::hash_file(&file_path).unwrap();
        let data_hash = Blake3Hasher::hash_data(b"");
        assert_eq!(file_hash, data_hash);
    }
}
