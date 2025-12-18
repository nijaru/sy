use crate::error::Result;
use std::path::Path;

mod blake3;
mod xxhash3;

pub use self::blake3::Blake3Hasher;
pub use self::xxhash3::XxHash3Hasher;

/// Type of checksum to compute
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumType {
    /// No checksum verification (trust TCP)
    None,

    /// Fast non-cryptographic checksum (xxHash3)
    /// Good for detecting corruption but not malicious tampering
    Fast,

    /// Cryptographic checksum (BLAKE3)
    /// Slower but provides cryptographic guarantees
    #[allow(dead_code)]
    Cryptographic,
}

/// A computed checksum value
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    None,
    Fast(Vec<u8>),
    Cryptographic(Vec<u8>),
}

#[allow(dead_code)] // Public API for checksum operations
impl Checksum {
    /// Create a None checksum
    pub fn none() -> Self {
        Self::None
    }

    /// Create a Fast (xxHash3) checksum
    pub fn fast(hash: Vec<u8>) -> Self {
        Self::Fast(hash)
    }

    /// Create a Cryptographic (BLAKE3) checksum
    pub fn cryptographic(hash: Vec<u8>) -> Self {
        Self::Cryptographic(hash)
    }

    /// Check if this is a None checksum
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Get the checksum bytes, or None if this is a None checksum
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::None => None,
            Self::Fast(bytes) | Self::Cryptographic(bytes) => Some(bytes),
        }
    }

    /// Convert to hex string for display
    pub fn to_hex(&self) -> String {
        match self.bytes() {
            Some(bytes) => hex::encode(bytes),
            None => "none".to_string(),
        }
    }
}

/// Integrity verifier for file transfers
#[derive(Clone)]
pub struct IntegrityVerifier {
    checksum_type: ChecksumType,
    verify_on_write: bool,
}

#[allow(dead_code)] // Public API for integrity verification
impl IntegrityVerifier {
    /// Create a new integrity verifier
    pub fn new(checksum_type: ChecksumType, verify_on_write: bool) -> Self {
        Self {
            checksum_type,
            verify_on_write,
        }
    }

    /// Get the checksum type
    pub fn checksum_type(&self) -> ChecksumType {
        self.checksum_type
    }

    /// Check if paranoid mode is enabled (verify on write)
    pub fn verify_on_write(&self) -> bool {
        self.verify_on_write
    }

    /// Compute checksum for a file
    pub fn compute_file_checksum(&self, path: &Path) -> Result<Checksum> {
        match self.checksum_type {
            ChecksumType::None => Ok(Checksum::None),
            ChecksumType::Fast => {
                let hash = XxHash3Hasher::hash_file(path)?;
                Ok(Checksum::Fast(hash.to_le_bytes().to_vec()))
            }
            ChecksumType::Cryptographic => {
                let hash = Blake3Hasher::hash_file(path)?;
                Ok(Checksum::Cryptographic(hash.as_bytes().to_vec()))
            }
        }
    }

    /// Compute checksum for data in memory
    pub fn compute_data_checksum(&self, data: &[u8]) -> Result<Checksum> {
        match self.checksum_type {
            ChecksumType::None => Ok(Checksum::None),
            ChecksumType::Fast => {
                let hash = XxHash3Hasher::hash_data(data);
                Ok(Checksum::Fast(hash.to_le_bytes().to_vec()))
            }
            ChecksumType::Cryptographic => {
                let hash = Blake3Hasher::hash_data(data);
                Ok(Checksum::Cryptographic(hash.as_bytes().to_vec()))
            }
        }
    }

    /// Verify that source and destination files match
    pub fn verify_transfer(&self, source: &Path, dest: &Path) -> Result<bool> {
        let source_sum = self.compute_file_checksum(source)?;
        let dest_sum = self.compute_file_checksum(dest)?;
        Ok(source_sum == dest_sum)
    }

    /// Verify that a written block matches expected data
    ///
    /// This is used in paranoid mode to verify each block immediately after writing.
    /// Returns true if block matches, false if corrupted.
    pub fn verify_block(&self, expected_data: &[u8], actual_data: &[u8]) -> Result<bool> {
        if !self.verify_on_write {
            // Not in paranoid mode, skip verification
            return Ok(true);
        }

        let expected_sum = self.compute_data_checksum(expected_data)?;
        let actual_sum = self.compute_data_checksum(actual_data)?;
        Ok(expected_sum == actual_sum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_checksum_none() {
        let checksum = Checksum::none();
        assert!(checksum.is_none());
        assert_eq!(checksum.bytes(), None);
        assert_eq!(checksum.to_hex(), "none");
    }

    #[test]
    fn test_checksum_fast() {
        let checksum = Checksum::fast(vec![1, 2, 3, 4]);
        assert!(!checksum.is_none());
        assert_eq!(checksum.bytes(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(checksum.to_hex(), "01020304");
    }

    #[test]
    fn test_checksum_cryptographic() {
        let checksum = Checksum::cryptographic(vec![0xde, 0xad, 0xbe, 0xef]);
        assert!(!checksum.is_none());
        assert_eq!(checksum.bytes(), Some(&[0xde, 0xad, 0xbe, 0xef][..]));
        assert_eq!(checksum.to_hex(), "deadbeef");
    }

    #[test]
    fn test_checksum_equality() {
        let cs1 = Checksum::fast(vec![1, 2, 3]);
        let cs2 = Checksum::fast(vec![1, 2, 3]);
        let cs3 = Checksum::fast(vec![4, 5, 6]);

        assert_eq!(cs1, cs2);
        assert_ne!(cs1, cs3);
    }

    #[test]
    fn test_verifier_none() {
        let verifier = IntegrityVerifier::new(ChecksumType::None, false);
        assert_eq!(verifier.checksum_type(), ChecksumType::None);
        assert!(!verifier.verify_on_write());

        let checksum = verifier.compute_data_checksum(b"test data").unwrap();
        assert!(checksum.is_none());
    }

    #[test]
    fn test_verifier_cryptographic() {
        let verifier = IntegrityVerifier::new(ChecksumType::Cryptographic, false);
        assert_eq!(verifier.checksum_type(), ChecksumType::Cryptographic);

        let data = b"Hello, world!";
        let checksum = verifier.compute_data_checksum(data).unwrap();
        assert!(!checksum.is_none());

        // Same data should produce same checksum
        let checksum2 = verifier.compute_data_checksum(data).unwrap();
        assert_eq!(checksum, checksum2);

        // Different data should produce different checksum
        let checksum3 = verifier.compute_data_checksum(b"Different data").unwrap();
        assert_ne!(checksum, checksum3);
    }

    #[test]
    fn test_verifier_file_checksum() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, b"Test file content").unwrap();

        let verifier = IntegrityVerifier::new(ChecksumType::Cryptographic, false);
        let checksum = verifier.compute_file_checksum(&file_path).unwrap();
        assert!(!checksum.is_none());
    }

    #[test]
    fn test_verify_transfer() {
        let temp_dir = TempDir::new().unwrap();
        let source_path = temp_dir.path().join("source.txt");
        let dest_path = temp_dir.path().join("dest.txt");

        fs::write(&source_path, b"File content").unwrap();
        fs::write(&dest_path, b"File content").unwrap();

        let verifier = IntegrityVerifier::new(ChecksumType::Cryptographic, false);
        assert!(verifier.verify_transfer(&source_path, &dest_path).unwrap());

        // Change destination content
        fs::write(&dest_path, b"Different content").unwrap();
        assert!(!verifier.verify_transfer(&source_path, &dest_path).unwrap());
    }

    #[test]
    fn test_verifier_fast() {
        let verifier = IntegrityVerifier::new(ChecksumType::Fast, false);
        assert_eq!(verifier.checksum_type(), ChecksumType::Fast);

        let data = b"Hello, xxHash3!";
        let checksum = verifier.compute_data_checksum(data).unwrap();
        assert!(!checksum.is_none());

        // Same data should produce same checksum
        let checksum2 = verifier.compute_data_checksum(data).unwrap();
        assert_eq!(checksum, checksum2);

        // Different data should produce different checksum
        let checksum3 = verifier.compute_data_checksum(b"Different data").unwrap();
        assert_ne!(checksum, checksum3);
    }

    #[test]
    fn test_verifier_fast_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, b"Test file content").unwrap();

        let verifier = IntegrityVerifier::new(ChecksumType::Fast, false);
        let checksum = verifier.compute_file_checksum(&file_path).unwrap();
        assert!(!checksum.is_none());

        // Same file should produce same checksum
        let checksum2 = verifier.compute_file_checksum(&file_path).unwrap();
        assert_eq!(checksum, checksum2);
    }

    #[test]
    fn test_verify_transfer_fast() {
        let temp_dir = TempDir::new().unwrap();
        let source_path = temp_dir.path().join("source.txt");
        let dest_path = temp_dir.path().join("dest.txt");

        fs::write(&source_path, b"File content").unwrap();
        fs::write(&dest_path, b"File content").unwrap();

        let verifier = IntegrityVerifier::new(ChecksumType::Fast, false);
        assert!(verifier.verify_transfer(&source_path, &dest_path).unwrap());

        // Change destination content
        fs::write(&dest_path, b"Different content").unwrap();
        assert!(!verifier.verify_transfer(&source_path, &dest_path).unwrap());
    }

    #[test]
    fn test_verify_block_disabled() {
        let verifier = IntegrityVerifier::new(ChecksumType::Cryptographic, false);
        let data1 = b"Hello, world!";
        let data2 = b"Goodbye, world!";

        // With verify_on_write = false, should always return true
        assert!(verifier.verify_block(data1, data2).unwrap());
    }

    #[test]
    fn test_verify_block_matching() {
        let verifier = IntegrityVerifier::new(ChecksumType::Cryptographic, true);
        let data = b"Test data for block verification";

        // Same data should verify successfully
        assert!(verifier.verify_block(data, data).unwrap());
    }

    #[test]
    fn test_verify_block_mismatched() {
        let verifier = IntegrityVerifier::new(ChecksumType::Cryptographic, true);
        let expected = b"Expected data";
        let actual = b"Different data";

        // Different data should fail verification
        assert!(!verifier.verify_block(expected, actual).unwrap());
    }

    #[test]
    fn test_verify_block_fast_checksum() {
        let verifier = IntegrityVerifier::new(ChecksumType::Fast, true);
        let data = b"Fast checksum test data";

        // Should work with fast checksums too
        assert!(verifier.verify_block(data, data).unwrap());

        let corrupted = b"Corrupted checksum data";
        assert!(!verifier.verify_block(data, corrupted).unwrap());
    }
}
