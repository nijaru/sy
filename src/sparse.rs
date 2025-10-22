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
}
