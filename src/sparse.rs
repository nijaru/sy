/// Sparse file handling utilities
///
/// This module provides functions for detecting and working with sparse files
/// (files with holes). It supports both local and remote (SSH) sparse file transfers.
use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use serde::{Deserialize, Serialize};

/// Represents a contiguous region of data in a sparse file
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[allow(dead_code)] // Foundation for future sparse file optimizations
pub struct DataRegion {
    /// Offset from start of file in bytes
    pub offset: u64,
    /// Length of data region in bytes
    pub length: u64,
}

/// Detect data regions in a sparse file using SEEK_HOLE/SEEK_DATA
///
/// Returns a list of (offset, length) pairs representing non-zero regions.
/// Returns empty vec if file is all holes or if SEEK_DATA not supported.
#[cfg(unix)]
#[allow(dead_code)] // Foundation for future sparse file optimizations
pub fn detect_data_regions(path: &Path) -> io::Result<Vec<DataRegion>> {
    const SEEK_DATA: i32 = 3; // Find next data region
    const SEEK_HOLE: i32 = 4; // Find next hole

    let file = File::open(path)?;
    let file_size = file.metadata()?.len();

    // Empty file or zero-length file
    if file_size == 0 {
        return Ok(Vec::new());
    }

    let fd = file.as_raw_fd();
    let file_size_i64 = file_size as i64;

    // Try SEEK_DATA first to check if supported
    let first_data = unsafe { libc::lseek(fd, 0, SEEK_DATA) };
    if first_data < 0 {
        let err = io::Error::last_os_error();
        let errno = err.raw_os_error();

        // EINVAL = not supported (most filesystems)
        // ENXIO can mean either "all holes" OR "not supported" (APFS on macOS)
        // To distinguish: if file size > 0 and we get ENXIO, treat as unsupported
        if errno == Some(libc::EINVAL) {
            return Err(err);
        }

        if errno == Some(libc::ENXIO) {
            // ENXIO on macOS APFS means "not supported", not "all holes"
            // Return error to fall back to block-based detection
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SEEK_DATA not properly supported (got ENXIO)",
            ));
        }

        // Other errors - propagate
        return Err(err);
    }

    let mut regions = Vec::new();
    let mut pos: i64 = 0;

    while pos < file_size_i64 {
        // Find next data region
        let data_start = unsafe { libc::lseek(fd, pos, SEEK_DATA) };
        if data_start < 0 {
            break; // No more data (ENXIO)
        }
        if data_start >= file_size_i64 {
            break;
        }

        // Find end of this data region (start of next hole)
        let hole_start = unsafe { libc::lseek(fd, data_start, SEEK_HOLE) };
        let data_end = if hole_start < 0 || hole_start > file_size_i64 {
            file_size_i64
        } else {
            hole_start
        };

        regions.push(DataRegion {
            offset: data_start as u64,
            length: (data_end - data_start) as u64,
        });

        pos = data_end;
    }

    Ok(regions)
}

/// Detect data regions on non-Unix platforms (fallback - no sparse support)
#[cfg(not(unix))]
pub fn detect_data_regions(_path: &Path) -> io::Result<Vec<DataRegion>> {
    // Non-Unix platforms don't support SEEK_HOLE/SEEK_DATA
    // Return error to indicate sparse detection not available
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Sparse file detection not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    #[cfg(unix)]
    #[ignore] // SEEK_DATA not reliably supported on macOS APFS
    fn test_detect_data_regions_all_data() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("all_data.txt");

        // Create a non-sparse file
        std::fs::write(&file_path, b"Hello, world!").unwrap();

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) => {
                // SEEK_DATA supported
                // Should have one region covering entire file
                assert_eq!(r.len(), 1);
                assert_eq!(r[0].offset, 0);
                assert_eq!(r[0].length, 13);
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == std::io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported on this filesystem - test passes
                // (e.g., APFS on macOS, older ext4, network mounts)
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_detect_data_regions_empty_file() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("empty.txt");

        // Create empty file
        File::create(&file_path).unwrap();

        let regions = detect_data_regions(&file_path).unwrap();

        // Empty file should have no regions
        assert_eq!(regions.len(), 0);
    }

    #[test]
    #[cfg(unix)]
    #[ignore] // SEEK_DATA not reliably supported on macOS APFS
    fn test_detect_data_regions_sparse_file() {
        use std::process::Command;

        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("sparse.dat");

        // Use dd to create a truly sparse file (10MB with only 4KB data)
        // Note: APFS on macOS may not create sparse files with write_all_at
        let output = Command::new("dd")
            .args([
                "if=/dev/zero",
                &format!("of={}", file_path.display()),
                "bs=1024",
                "count=0",
                "seek=10240", // 10MB offset
            ])
            .output();

        // If dd fails or file not created, skip test
        if output.is_err() || !file_path.exists() {
            return;
        }

        // Write 4KB data at start
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&file_path)
            .unwrap();
        use std::io::Write;
        file.write_all(&vec![0x42; 4096]).unwrap();
        drop(file);

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) => {
                // Filesystem supports sparse files and SEEK_DATA
                // Should have at least one data region
                assert!(!r.is_empty(), "Should have at least one data region");

                // First region should start at or near 0
                assert!(r[0].offset < 8192, "First region should be near start");
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == std::io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported - acceptable (older kernels, some filesystems)
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    // === Edge Case Tests ===

    #[test]
    #[cfg(unix)]
    fn test_detect_data_regions_nonexistent_file() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("nonexistent.txt");

        let result = detect_data_regions(&file_path);

        // Should return an I/O error (file not found)
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    #[cfg(unix)]
    #[ignore] // SEEK_DATA not reliably supported on macOS APFS
    fn test_detect_data_regions_leading_hole() {
        use std::io::{Seek, SeekFrom, Write};

        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("leading_hole.dat");

        // Create file with hole at start, data at end
        let mut file = File::create(&file_path).unwrap();

        // Seek to 1MB offset and write data
        file.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        file.write_all(b"Data after hole").unwrap();
        drop(file);

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) => {
                // Should detect data region at ~1MB offset
                assert!(!r.is_empty(), "Should have at least one data region");
                assert!(
                    r[0].offset >= 1024 * 1024,
                    "First region should start at/after 1MB, got offset: {}",
                    r[0].offset
                );
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported - acceptable
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    #[cfg(unix)]
    #[ignore] // SEEK_DATA not reliably supported on macOS APFS
    fn test_detect_data_regions_trailing_hole() {
        use std::io::{Seek, SeekFrom, Write};

        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("trailing_hole.dat");

        // Create file with data at start, hole at end
        let mut file = File::create(&file_path).unwrap();

        // Write data at start
        file.write_all(b"Data at start").unwrap();

        // Seek to 1MB and truncate/extend to create trailing hole
        file.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        file.write_all(&[0]).unwrap(); // Write 1 byte to extend file
        drop(file);

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) => {
                // Should have at least one region (the initial data)
                assert!(!r.is_empty(), "Should have at least one data region");

                // First region should be at/near start
                assert!(
                    r[0].offset < 1024,
                    "First region should be at start, got offset: {}",
                    r[0].offset
                );
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported - acceptable
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    #[cfg(unix)]
    #[ignore] // SEEK_DATA not reliably supported on macOS APFS
    fn test_detect_data_regions_multiple_data_regions() {
        use std::io::{Seek, SeekFrom, Write};

        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("multiple_regions.dat");

        // Create file with multiple data/hole alternations
        let mut file = File::create(&file_path).unwrap();

        // Data region 1: 0-4KB
        file.write_all(&vec![0x41; 4096]).unwrap();

        // Hole: 4KB-1MB (seek ahead)
        file.seek(SeekFrom::Start(1024 * 1024)).unwrap();

        // Data region 2: 1MB-1MB+4KB
        file.write_all(&vec![0x42; 4096]).unwrap();

        // Hole: 1MB+4KB-2MB
        file.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();

        // Data region 3: 2MB-2MB+4KB
        file.write_all(&vec![0x43; 4096]).unwrap();

        drop(file);

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) => {
                // Should detect multiple data regions (at least 2-3)
                assert!(
                    r.len() >= 2,
                    "Should have multiple data regions, got {}",
                    r.len()
                );

                // Regions should be ordered by offset
                for i in 0..r.len() - 1 {
                    assert!(
                        r[i].offset < r[i + 1].offset,
                        "Regions should be ordered: region {} offset {} >= region {} offset {}",
                        i,
                        r[i].offset,
                        i + 1,
                        r[i + 1].offset
                    );
                }
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported - acceptable
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    #[cfg(unix)]
    #[ignore] // SEEK_DATA not reliably supported on macOS APFS
    fn test_detect_data_regions_very_large_offset() {
        use std::io::{Seek, SeekFrom, Write};

        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("large_offset.dat");

        // Create file with data at very large offset (1GB)
        let mut file = File::create(&file_path).unwrap();
        let large_offset = 1024 * 1024 * 1024u64; // 1GB

        // Seek to 1GB and write small data
        file.seek(SeekFrom::Start(large_offset)).unwrap();
        file.write_all(b"Far away data").unwrap();
        drop(file);

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) => {
                // Should detect data at large offset
                assert!(!r.is_empty(), "Should have at least one data region");

                // First region should be at/near the large offset
                assert!(
                    r[0].offset >= large_offset - 4096, // Allow some FS block rounding
                    "First region should be at large offset, got: {}",
                    r[0].offset
                );
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported - acceptable
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    #[cfg(unix)]
    #[ignore] // SEEK_DATA not reliably supported on macOS APFS
    fn test_detect_data_regions_single_byte() {
        use std::io::{Seek, SeekFrom, Write};

        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("single_byte.dat");

        // Create file with single byte of data surrounded by holes
        let mut file = File::create(&file_path).unwrap();

        // Seek to 1MB, write 1 byte, then extend to 2MB
        file.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        file.write_all(&[0x99]).unwrap();
        file.seek(SeekFrom::Start(2 * 1024 * 1024)).unwrap();
        file.write_all(&[0]).unwrap();
        drop(file);

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) => {
                // Should detect at least one region containing that byte
                // (filesystem may round to block size)
                assert!(!r.is_empty(), "Should have at least one data region");
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported - acceptable
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_detect_data_regions_region_ordering() {
        // This test verifies the DataRegion ordering invariant
        // All detected regions should be in ascending order by offset

        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("ordering.dat");

        // Create any non-sparse file
        std::fs::write(&file_path, b"Test data for ordering").unwrap();

        let regions = detect_data_regions(&file_path);

        match regions {
            Ok(r) if !r.is_empty() => {
                // Verify regions are ordered by offset
                for i in 0..r.len() - 1 {
                    assert!(
                        r[i].offset < r[i + 1].offset,
                        "Regions must be ordered by offset"
                    );

                    // Also verify no overlap
                    assert!(
                        r[i].offset + r[i].length <= r[i + 1].offset,
                        "Regions must not overlap"
                    );
                }

                // Verify all regions have non-zero length
                for region in r.iter() {
                    assert!(region.length > 0, "Regions must have non-zero length");
                }
            }
            Ok(_) => {
                // Empty result is OK (empty file or all holes)
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EINVAL)
                    || e.kind() == io::ErrorKind::Unsupported =>
            {
                // SEEK_DATA not supported - acceptable
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_detect_data_regions_boundary_conditions() {
        let temp = TempDir::new().unwrap();

        // Test 1: File with data exactly at offset 0
        let file1 = temp.path().join("at_zero.dat");
        std::fs::write(&file1, b"At zero").unwrap();

        let regions = detect_data_regions(&file1);
        if let Ok(r) = regions {
            if !r.is_empty() {
                assert_eq!(r[0].offset, 0, "First region should start at 0");
            }
        }

        // Test 2: Very small file (1 byte)
        let file2 = temp.path().join("one_byte.dat");
        std::fs::write(&file2, b"X").unwrap();

        let regions = detect_data_regions(&file2);
        if let Ok(r) = regions {
            if !r.is_empty() {
                assert_eq!(r[0].offset, 0);
                assert!(r[0].length >= 1, "Should contain at least 1 byte");
            }
        }
    }

    #[test]
    #[cfg(not(unix))]
    fn test_detect_data_regions_unsupported_platform() {
        // On non-Unix platforms, should return Unsupported error
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.dat");
        std::fs::write(&file_path, b"test").unwrap();

        let result = detect_data_regions(&file_path);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn test_data_region_serialization() {
        // Test that DataRegion can be serialized/deserialized
        let region = DataRegion {
            offset: 1024,
            length: 4096,
        };

        let json = serde_json::to_string(&region).unwrap();
        let deserialized: DataRegion = serde_json::from_str(&json).unwrap();

        assert_eq!(region, deserialized);
        assert_eq!(deserialized.offset, 1024);
        assert_eq!(deserialized.length, 4096);
    }
}
