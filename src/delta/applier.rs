use super::{Delta, DeltaOp};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Apply delta operations to reconstruct a file
///
/// Reads from old_file (using Copy ops) and delta data (using Data ops)
/// to create new_file.
pub fn apply_delta(old_file: &Path, delta: &Delta, new_file: &Path) -> io::Result<()> {
    let mut old = File::open(old_file)?;
    let mut new = File::create(new_file)?;

    for op in &delta.ops {
        match op {
            DeltaOp::Copy { offset, size } => {
                // Seek to position in old file
                old.seek(SeekFrom::Start(*offset))?;

                // Copy block
                let mut buffer = vec![0u8; *size];
                old.read_exact(&mut buffer)?;
                new.write_all(&buffer)?;
            }
            DeltaOp::Data(data) => {
                // Write literal data
                new.write_all(data)?;
            }
        }
    }

    new.flush()?;
    Ok(())
}

/// Apply delta when there's no old file (full reconstruction from literals)
pub fn apply_delta_no_base(delta: &Delta, new_file: &Path) -> io::Result<()> {
    let mut new = File::create(new_file)?;

    for op in &delta.ops {
        match op {
            DeltaOp::Copy { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Cannot apply Copy operation without base file",
                ));
            }
            DeltaOp::Data(data) => {
                new.write_all(data)?;
            }
        }
    }

    new.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{compute_checksums, generate_delta};
    use std::io::Write as _;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_apply_delta_identical() {
        // Create original file
        let mut original = NamedTempFile::new().unwrap();
        let data = b"Hello, World! This is a test of delta sync.";
        original.write_all(data).unwrap();
        original.flush().unwrap();

        // Create "modified" file (actually identical)
        let mut modified = NamedTempFile::new().unwrap();
        modified.write_all(data).unwrap();
        modified.flush().unwrap();

        // Generate delta
        let checksums = compute_checksums(original.path(), 8).unwrap();
        let delta = generate_delta(modified.path(), &checksums, 8).unwrap();

        // Apply delta
        let temp_dir = TempDir::new().unwrap();
        let reconstructed = temp_dir.path().join("reconstructed");
        apply_delta(original.path(), &delta, &reconstructed).unwrap();

        // Verify reconstructed file matches modified
        let original_data = std::fs::read(modified.path()).unwrap();
        let reconstructed_data = std::fs::read(&reconstructed).unwrap();
        assert_eq!(original_data, reconstructed_data);
    }

    #[test]
    fn test_apply_delta_modified() {
        // Create original file
        let mut original = NamedTempFile::new().unwrap();
        original.write_all(b"AAAABBBBCCCCDDDD").unwrap();
        original.flush().unwrap();

        // Create modified file (change middle blocks)
        let mut modified = NamedTempFile::new().unwrap();
        modified.write_all(b"AAAAXXXXYYYYDDDD").unwrap();
        modified.flush().unwrap();

        // Generate delta
        let block_size = 4;
        let checksums = compute_checksums(original.path(), block_size).unwrap();
        let delta = generate_delta(modified.path(), &checksums, block_size).unwrap();

        // Apply delta
        let temp_dir = TempDir::new().unwrap();
        let reconstructed = temp_dir.path().join("reconstructed");
        apply_delta(original.path(), &delta, &reconstructed).unwrap();

        // Verify
        let expected = std::fs::read(modified.path()).unwrap();
        let actual = std::fs::read(&reconstructed).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn test_apply_delta_completely_new() {
        // Create original file
        let mut original = NamedTempFile::new().unwrap();
        original.write_all(b"old data here").unwrap();
        original.flush().unwrap();

        // Create completely different file
        let mut modified = NamedTempFile::new().unwrap();
        modified.write_all(b"completely new content!").unwrap();
        modified.flush().unwrap();

        // Generate delta
        let checksums = compute_checksums(original.path(), 4).unwrap();
        let delta = generate_delta(modified.path(), &checksums, 4).unwrap();

        // Apply delta
        let temp_dir = TempDir::new().unwrap();
        let reconstructed = temp_dir.path().join("reconstructed");
        apply_delta(original.path(), &delta, &reconstructed).unwrap();

        // Verify
        let expected = std::fs::read(modified.path()).unwrap();
        let actual = std::fs::read(&reconstructed).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn test_apply_delta_no_base() {
        // Create file with only literal data
        let mut source = NamedTempFile::new().unwrap();
        let data = b"new file content";
        source.write_all(data).unwrap();
        source.flush().unwrap();

        // Generate delta with no base checksums
        let delta = generate_delta(source.path(), &[], 4).unwrap();

        // Apply without base file
        let temp_dir = TempDir::new().unwrap();
        let reconstructed = temp_dir.path().join("reconstructed");
        apply_delta_no_base(&delta, &reconstructed).unwrap();

        // Verify
        let reconstructed_data = std::fs::read(&reconstructed).unwrap();
        assert_eq!(reconstructed_data, data);
    }

    #[test]
    fn test_roundtrip_large_file() {
        // Create larger test file
        let mut original = NamedTempFile::new().unwrap();
        let original_data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        original.write_all(&original_data).unwrap();
        original.flush().unwrap();

        // Modify some blocks
        let mut modified_data = original_data.clone();
        for i in 2000..3000 {
            modified_data[i] = 0xFF;
        }
        let mut modified = NamedTempFile::new().unwrap();
        modified.write_all(&modified_data).unwrap();
        modified.flush().unwrap();

        // Generate and apply delta
        let block_size = 512;
        let checksums = compute_checksums(original.path(), block_size).unwrap();
        let delta = generate_delta(modified.path(), &checksums, block_size).unwrap();

        let temp_dir = TempDir::new().unwrap();
        let reconstructed = temp_dir.path().join("reconstructed");
        apply_delta(original.path(), &delta, &reconstructed).unwrap();

        // Verify
        let reconstructed_data = std::fs::read(&reconstructed).unwrap();
        assert_eq!(reconstructed_data, modified_data);

        // Check delta efficiency
        let ratio = delta.compression_ratio();
        println!("Compression ratio: {:.2}%", ratio * 100.0);
        assert!(ratio < 0.2); // Should transfer less than 20% (1000 bytes out of 10000)
    }
}
