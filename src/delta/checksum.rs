use super::Adler32;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Block checksum containing both weak and strong hashes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockChecksum {
    /// Block index (0-based)
    pub index: u64,
    /// Byte offset in file
    pub offset: u64,
    /// Block size in bytes
    pub size: usize,
    /// Weak rolling checksum (Adler-32)
    pub weak: u32,
    /// Strong checksum (xxHash3)
    pub strong: u64,
}

/// Compute checksums for all blocks in a file
///
/// This is called on the destination file to create a checksum map
/// that the source can use to find matching blocks.
pub fn compute_checksums(path: &Path, block_size: usize) -> io::Result<Vec<BlockChecksum>> {
    let mut file = File::open(path)?;
    let mut checksums = Vec::new();
    let mut buffer = vec![0u8; block_size];
    let mut offset = 0u64;
    let mut index = 0u64;

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        let block = &buffer[..bytes_read];

        // Compute weak checksum (Adler-32)
        let weak = Adler32::hash(block);

        // Compute strong checksum (xxHash3)
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(block);
        let strong = hasher.digest();

        checksums.push(BlockChecksum {
            index,
            offset,
            size: bytes_read,
            weak,
            strong,
        });

        offset += bytes_read as u64;
        index += 1;
    }

    Ok(checksums)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_compute_checksums() {
        // Create test file
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"Hello, World! This is a test file for checksumming.").unwrap();
        temp_file.flush().unwrap();

        // Compute checksums
        let checksums = compute_checksums(temp_file.path(), 16).unwrap();

        // Should have ceil(52 / 16) = 4 blocks
        assert_eq!(checksums.len(), 4);

        // Check first block
        assert_eq!(checksums[0].index, 0);
        assert_eq!(checksums[0].offset, 0);
        assert_eq!(checksums[0].size, 16);

        // Check last block (partial)
        let last = &checksums[3];
        assert_eq!(last.index, 3);
        assert_eq!(last.offset, 48);
        assert_eq!(last.size, 4); // "ing."
    }

    #[test]
    fn test_empty_file() {
        let temp_file = NamedTempFile::new().unwrap();
        let checksums = compute_checksums(temp_file.path(), 1024).unwrap();
        assert_eq!(checksums.len(), 0);
    }

    #[test]
    fn test_checksums_deterministic() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"test data").unwrap();
        temp_file.flush().unwrap();

        let checksums1 = compute_checksums(temp_file.path(), 4).unwrap();
        let checksums2 = compute_checksums(temp_file.path(), 4).unwrap();

        assert_eq!(checksums1, checksums2);
    }

    #[test]
    fn test_different_block_sizes() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let data = b"a".repeat(100);
        temp_file.write_all(&data).unwrap();
        temp_file.flush().unwrap();

        let checksums_small = compute_checksums(temp_file.path(), 10).unwrap();
        let checksums_large = compute_checksums(temp_file.path(), 50).unwrap();

        assert_eq!(checksums_small.len(), 10); // 100 / 10 = 10 blocks
        assert_eq!(checksums_large.len(), 2); // 100 / 50 = 2 blocks
    }
}
