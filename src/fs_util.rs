use std::path::Path;

/// Check if a filesystem supports copy-on-write (COW) reflinks
///
/// COW reflinks allow instant file cloning by sharing blocks until they're modified.
/// This is much faster than copying, especially for large files.
///
/// Supported filesystems:
/// - macOS: APFS (default on modern macOS)
/// - Linux: BTRFS, XFS (with reflink support)
/// - Windows: ReFS (rare)
///
/// NOT supported:
/// - Linux: ext4, ext3 (most common)
/// - Windows: NTFS (most common)
/// - macOS: HFS+ (legacy)
#[cfg(target_os = "macos")]
pub fn supports_cow_reflinks(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // On macOS, check if filesystem is APFS using statfs
    #[repr(C)]
    struct statfs {
        f_bsize: u32,
        f_iosize: i32,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_fsid: [i32; 2],
        f_owner: u32,
        f_type: u32,
        f_flags: u32,
        f_fssubtype: u32,
        f_fstypename: [u8; 16],
        f_mntonname: [u8; 1024],
        f_mntfromname: [u8; 1024],
        f_reserved: [u32; 8],
    }

    extern "C" {
        fn statfs(path: *const libc::c_char, buf: *mut statfs) -> libc::c_int;
    }

    let path_bytes = path.as_os_str().as_bytes();
    let path_c = match CString::new(path_bytes) {
        Ok(p) => p,
        Err(_) => return false,
    };

    unsafe {
        let mut stat: std::mem::MaybeUninit<statfs> = std::mem::MaybeUninit::uninit();
        if statfs(path_c.as_ptr(), stat.as_mut_ptr()) == 0 {
            let stat = stat.assume_init();
            // APFS type name is "apfs"
            let fs_type = std::str::from_utf8(&stat.f_fstypename)
                .ok()
                .and_then(|s| s.split('\0').next())
                .unwrap_or("");

            fs_type == "apfs"
        } else {
            false
        }
    }
}

#[cfg(target_os = "linux")]
pub fn supports_cow_reflinks(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // On Linux, check if filesystem is BTRFS or XFS using statfs
    let path_bytes = path.as_os_str().as_bytes();
    let path_c = match CString::new(path_bytes) {
        Ok(p) => p,
        Err(_) => return false,
    };

    unsafe {
        let mut stat: std::mem::MaybeUninit<libc::statfs> = std::mem::MaybeUninit::uninit();
        if libc::statfs(path_c.as_ptr(), stat.as_mut_ptr()) == 0 {
            let stat = stat.assume_init();
            // BTRFS_SUPER_MAGIC = 0x9123683E
            // XFS_SUPER_MAGIC = 0x58465342
            matches!(stat.f_type, 0x9123683E | 0x58465342)
        } else {
            false
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn supports_cow_reflinks(_path: &Path) -> bool {
    // Windows ReFS supports reflinks via FSCTL_DUPLICATE_EXTENTS_TO_FILE,
    // but it's rare. For now, assume no COW on Windows/other platforms.
    false
}

/// Check if two paths are on the same filesystem
///
/// COW reflinks only work within the same filesystem.
/// This checks if source and dest are on the same device.
#[cfg(unix)]
pub fn same_filesystem(path1: &Path, path2: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let meta1 = match std::fs::metadata(path1) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let meta2 = match std::fs::metadata(path2) {
        Ok(m) => m,
        Err(_) => return false,
    };

    meta1.dev() == meta2.dev()
}

#[cfg(not(unix))]
pub fn same_filesystem(_path1: &Path, _path2: &Path) -> bool {
    // Conservative: assume different filesystems on non-Unix
    false
}

/// Check if a file has hard links (nlink > 1)
///
/// If a file has hard links, COW cloning would break the link relationship.
/// We need to use in-place updates to preserve hard links.
#[cfg(unix)]
pub fn has_hard_links(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path)
        .map(|m| m.nlink() > 1)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn has_hard_links(_path: &Path) -> bool {
    // Windows has hard links but less common, and we don't use COW there anyway
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_cow_detection() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, b"test").unwrap();

        let supports_cow = supports_cow_reflinks(&test_file);

        #[cfg(target_os = "macos")]
        {
            // Most modern macOS systems use APFS
            println!("macOS COW support: {}", supports_cow);
        }

        #[cfg(target_os = "linux")]
        {
            // Depends on filesystem (BTRFS/XFS yes, ext4 no)
            println!("Linux COW support: {}", supports_cow);
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_same_filesystem() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");

        fs::write(&file1, b"test1").unwrap();
        fs::write(&file2, b"test2").unwrap();

        // Same directory = same filesystem
        assert!(same_filesystem(&file1, &file2));

        // File and its parent directory = same filesystem
        assert!(same_filesystem(&file1, temp.path()));
    }

    #[test]
    #[cfg(unix)]
    fn test_hard_link_detection() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");

        fs::write(&file1, b"test").unwrap();

        // Initially no hard links
        assert!(!has_hard_links(&file1));

        // Create hard link
        #[cfg(unix)]
        {
            std::fs::hard_link(&file1, &file2).unwrap();

            // Now both files have nlink = 2
            assert!(has_hard_links(&file1));
            assert!(has_hard_links(&file2));
        }
    }
}
