use std::path::Path;
/// Filesystem utility functions for detecting COW support, hard links, and cross-filesystem operations.
///
/// This module provides platform-specific filesystem detection to enable intelligent
/// strategy selection in delta sync operations.
use sy::engine::namespace::NamespaceSemantics;

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
///
/// # Implementation Details
///
/// - **macOS**: Uses `statfs` to check filesystem type name against "apfs"
/// - **Linux**: Uses `statfs` to check magic number (BTRFS=0x9123683E, XFS=0x58465342)
/// - **Other platforms**: Returns false (conservative approach)
///
/// # Example
///
/// ```rust,ignore
/// use sy::fs_util::supports_cow_reflinks;
/// use std::path::Path;
///
/// let path = Path::new("/tmp/test.txt");
/// if supports_cow_reflinks(path) {
///     println!("Can use COW optimization");
/// } else {
///     println!("Using in-place strategy");
/// }
/// ```
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
///
/// # Implementation
///
/// Compares the `dev_t` device ID from `stat()` metadata.
/// Files on different devices (different partitions, mount points, etc.)
/// will have different device IDs.
///
/// # Returns
///
/// - `true` if both paths are on the same filesystem device
/// - `false` if on different devices or if metadata cannot be read
///
/// # Example
///
/// ```rust,ignore
/// ```
/// use std::path::Path;
///
/// let src = Path::new("/home/user/file1.txt");
/// let dst = Path::new("/home/user/file2.txt");
/// if same_filesystem(src, dst) {
///     println!("Same filesystem - can use COW");
/// }
/// ```
#[cfg(unix)]
#[allow(dead_code)]
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
///
/// # How Hard Links Work
///
/// Multiple directory entries can point to the same inode. When `nlink > 1`,
/// the file has multiple names (paths) pointing to the same data.
///
/// **Critical for correctness**: If we COW clone a hard-linked file, we create
/// a new inode, breaking the link. Changes to one "link" won't appear in others.
///
/// # Implementation
///
/// Reads the `nlink` field from file metadata. Returns `true` if `nlink > 1`.
///
/// # Returns
///
/// - `true` if file has hard links (nlink > 1)
/// - `false` if file is standalone (nlink == 1) or metadata cannot be read
///
/// # Example
///
/// ```rust,ignore
/// ```
/// use std::path::Path;
///
/// let path = Path::new("/tmp/file.txt");
/// if has_hard_links(path) {
///     println!("File has hard links - must use in-place strategy");
/// }
/// ```
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

/// Probe the name-comparison semantics of the filesystem hosting `root`.
///
/// Destination namespace preflight needs the *filesystem's* rules, not the
/// OS's: APFS volumes can be case-sensitive, ext4 directories can opt into
/// case folding, and removable FAT/exFAT volumes fold case on every OS.
/// The probe is read-only and may return `Unspecified` axes; callers stay
/// conservative for those (see `engine::namespace`).
#[cfg(target_os = "macos")]
pub fn namespace_semantics(root: &Path) -> NamespaceSemantics {
    use sy::engine::namespace::Folding;

    let case = macos_case_folding(root);
    // HFS+ and APFS fold Unicode canonical decomposition by format design;
    // no API exposes this property, so foreign volumes (smbfs, msdos, ntfs
    // mounts) stay Unspecified rather than borrowing APFS behavior.
    let normalization = match macos_fs_type_name(root).as_deref() {
        Some("apfs") | Some("hfs") | Some("hfsx") => Folding::Folded,
        _ => Folding::Unspecified,
    };
    NamespaceSemantics {
        case,
        normalization,
    }
}

#[cfg(target_os = "macos")]
fn macos_case_folding(root: &Path) -> sy::engine::namespace::Folding {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use sy::engine::namespace::Folding;

    // `vol_capabilities_attr_t` layout: `<sys/attr.h>` VOL_CAPABILITIES_FORMAT
    // index holds the VOL_CAP_FMT_* bits. Verified against a live APFS root:
    // the fixed buffer is `length: u32` followed by the 32-byte capabilities
    // struct (total 36 bytes).
    #[repr(C)]
    struct VolCapabilitiesBuffer {
        length: u32,
        capabilities: libc::vol_capabilities_attr_t,
    }

    const VOL_CAPABILITIES_FORMAT: usize = 0;

    let Ok(path) = CString::new(root.as_os_str().as_bytes()) else {
        return Folding::Unspecified;
    };
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buffer = VolCapabilitiesBuffer {
        length: 0,
        capabilities: libc::vol_capabilities_attr_t {
            capabilities: [0; 4],
            valid: [0; 4],
        },
    };
    // SAFETY: `path` is a valid NUL-terminated string, `attributes` is fully
    // initialized, and `buffer`/its size match the fixed `attrlist` request
    // (ATTR_VOL_INFO | ATTR_VOL_CAPABILITIES returns exactly the capabilities
    // struct after the returned length word). getattrlist only writes within
    // `attrbufsize` bytes of `buffer`.
    let status = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast::<libc::c_void>(),
            (&mut buffer as *mut VolCapabilitiesBuffer).cast::<libc::c_void>(),
            std::mem::size_of::<VolCapabilitiesBuffer>(),
            0,
        )
    };
    if status != 0 {
        return Folding::Unspecified;
    }
    let capabilities = buffer.capabilities.capabilities[VOL_CAPABILITIES_FORMAT];
    let valid = buffer.capabilities.valid[VOL_CAPABILITIES_FORMAT];
    if (valid & libc::VOL_CAP_FMT_CASE_SENSITIVE) == 0 {
        return Folding::Unspecified;
    }
    if (capabilities & libc::VOL_CAP_FMT_CASE_SENSITIVE) != 0 {
        Folding::Exact
    } else {
        Folding::Folded
    }
}

#[cfg(target_os = "macos")]
fn macos_fs_type_name(root: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(root.as_os_str().as_bytes()).ok()?;
    let mut stat: std::mem::MaybeUninit<libc::statfs> = std::mem::MaybeUninit::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a valid
    // output pointer of the struct type the callee fills on success.
    let status = unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) };
    if status != 0 {
        return None;
    }
    // SAFETY: success guarantees the callee initialized the whole struct.
    let stat = unsafe { stat.assume_init() };
    let name = stat
        .f_fstypename
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8 as char)
        .collect::<String>();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(target_os = "linux")]
pub fn namespace_semantics(root: &Path) -> NamespaceSemantics {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use sy::engine::namespace::Folding;

    let Ok(path) = CString::new(root.as_os_str().as_bytes()) else {
        return NamespaceSemantics::UNSPECIFIED;
    };
    let mut stat: std::mem::MaybeUninit<libc::statfs> = std::mem::MaybeUninit::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a valid
    // output pointer of the struct type the callee fills on success.
    let status = unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) };
    if status != 0 {
        return NamespaceSemantics::UNSPECIFIED;
    }
    // SAFETY: success guarantees the callee initialized the whole struct.
    let stat = unsafe { stat.assume_init() };

    match stat.f_type {
        // Byte-exact name comparison: ext2/3/4 (unless a directory enables
        // casefold), XFS, Btrfs, tmpfs, ramfs, F2FS (same caveat), EROFS,
        // NILFS2, squashfs.
        0xEF53 | 0x584_65342 | 0x9123_683E | 0x0102_1994 | 0x8584_58f6 | 0xF2F5_2010
        | 0xE0F5_E1E2 | 0x3434 | 0x7371_7368 => {
            if linux_root_casefolded(root) {
                // ext4/f2fs casefold directories fold case and Unicode
                // normalization together (encoding-based folding).
                NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED
            } else {
                NamespaceSemantics::BYTE_EXACT
            }
        }
        // FAT/exFAT/NTFS family: case folding is certain; their normalization
        // models differ per driver, so the second axis stays Unspecified.
        0x4D44 | 0x2011_BAB0 | 0x5346_544E => NamespaceSemantics {
            case: Folding::Folded,
            normalization: Folding::Unspecified,
        },
        // FUSE, NFS, ZFS, CIFS, overlayfs and unknowns can implement any
        // policy (ZFS datasets and overlayfs upper layers vary per mount).
        _ => NamespaceSemantics::UNSPECIFIED,
    }
}

/// True when `root` is an ext4/f2fs directory with `FS_CASEFOLD_FL` enabled.
///
/// The flag is inherited by subdirectories, so the root's setting covers the
/// whole synchronized subtree.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn linux_root_casefolded(root: &Path) -> bool {
    use std::os::unix::io::AsRawFd;

    /// `_IOR('f', 1, long)` from `<linux/fs.h>` on LP64. `libc::Ioctl` differs
    /// between glibc (c_ulong) and musl (c_int); the kernel compares only the
    /// low 32 bits of the request word.
    const FS_IOC_GETFLAGS: libc::Ioctl = 0x8008_6601_u32 as libc::Ioctl;
    /// `FS_CASEFOLD_FL` from `<linux/fs.h>`.
    const FS_CASEFOLD_FL: libc::c_long = 0x4000_0000;

    let Ok(file) = std::fs::File::open(root) else {
        return false;
    };
    let mut flags: libc::c_long = 0;
    // SAFETY: `file` holds an open descriptor and `flags` is a valid `long`
    // output pointer for the FS_IOC_GETFLAGS request. The ioctl only writes
    // the requested value and reports failure through its return code.
    let status = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            FS_IOC_GETFLAGS,
            (&mut flags as *mut libc::c_long).cast::<libc::c_void>(),
        )
    };
    status == 0 && (flags & FS_CASEFOLD_FL) != 0
}

#[cfg(all(target_os = "linux", not(target_pointer_width = "64")))]
fn linux_root_casefolded(_root: &Path) -> bool {
    // FS_IOC_GETFLAGS request encoding is pointer-width dependent; without a
    // verified LP64 constant the casefold refinement is simply not applied.
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn namespace_semantics(_root: &Path) -> NamespaceSemantics {
    // No verified probe on this platform; unknown semantics stay conservative.
    NamespaceSemantics::UNSPECIFIED
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use sy::engine::namespace::Folding;
    use tempfile::TempDir;

    /// Behavioral oracle: create a case-variant name beside an existing one and
    /// compare the filesystem's real aliasing with the probed claim.
    #[test]
    fn namespace_semantics_match_case_alias_behavior() {
        let temp = TempDir::new().unwrap();
        let semantics = namespace_semantics(temp.path());

        fs::create_dir(temp.path().join("Probe")).unwrap();
        let aliases = fs::create_dir(temp.path().join("probe")).is_err();

        match semantics.case {
            Folding::Exact => assert!(!aliases, "probe claims byte-exact but names alias"),
            Folding::Folded => assert!(aliases, "probe claims case folding but names do not alias"),
            Folding::Unspecified => {}
        }
    }

    /// Behavioral oracle for Unicode normalization aliasing (NFC vs NFD).
    #[test]
    fn namespace_semantics_match_normalization_alias_behavior() {
        use std::ffi::OsString;

        let temp = TempDir::new().unwrap();
        let semantics = namespace_semantics(temp.path());

        let nfc = OsString::from("caf\u{e9}");
        let nfd = OsString::from("cafe\u{301}");
        assert_ne!(nfc, nfd);
        fs::create_dir(temp.path().join(&nfc)).unwrap();
        let aliases = fs::create_dir(temp.path().join(&nfd)).is_err();

        match semantics.normalization {
            Folding::Exact => assert!(
                !aliases,
                "probe claims byte-exact normalization but NFC/NFD alias"
            ),
            Folding::Folded => assert!(
                aliases,
                "probe claims normalization folding but NFC/NFD do not alias"
            ),
            Folding::Unspecified => {}
        }
    }

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

    #[test]
    #[cfg(windows)]
    fn test_windows_no_cow_support() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, b"test").unwrap();

        // Windows NTFS should return false for COW support
        // (ReFS is not commonly used)
        assert!(!supports_cow_reflinks(&test_file));
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_same_filesystem_conservative() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");

        fs::write(&file1, b"test1").unwrap();
        fs::write(&file2, b"test2").unwrap();

        // Windows implementation is conservative - returns false
        assert!(!same_filesystem(&file1, &file2));
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_no_hard_link_detection() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");

        fs::write(&file1, b"test").unwrap();

        // Windows implementation always returns false
        assert!(!has_hard_links(&file1));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_filesystem_detection() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, b"test").unwrap();

        // TempDir is usually on tmpfs or ext4
        let supports_cow = supports_cow_reflinks(&test_file);

        // This test documents behavior rather than asserting specific values
        // since /tmp might be on different filesystems:
        // - ext4: false
        // - btrfs: true
        // - xfs: true (if reflink enabled)
        // - tmpfs: false
        println!("Linux temp directory COW support: {}", supports_cow);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_btrfs_detection() {
        // This test is informational - BTRFS magic number is 0x9123683E
        // We can't create a BTRFS filesystem in tests, but we document the value
        const BTRFS_SUPER_MAGIC: i64 = 0x9123683E;
        assert_eq!(BTRFS_SUPER_MAGIC, 0x9123683E);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_xfs_detection() {
        // This test is informational - XFS magic number is 0x58465342
        // We can't create a XFS filesystem in tests, but we document the value
        const XFS_SUPER_MAGIC: i64 = 0x58465342;
        assert_eq!(XFS_SUPER_MAGIC, 0x58465342);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_same_filesystem() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");

        fs::write(&file1, b"test1").unwrap();
        fs::write(&file2, b"test2").unwrap();

        // Files in same temp directory should be on same filesystem
        assert!(same_filesystem(&file1, &file2));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_apfs_detection() {
        let temp = TempDir::new().unwrap();
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, b"test").unwrap();

        // Modern macOS (10.13+) uses APFS by default
        let supports_cow = supports_cow_reflinks(&test_file);

        // Log the result for CI visibility
        println!("macOS COW support (APFS detection): {}", supports_cow);

        // Most modern Macs use APFS, but we don't assert to handle legacy HFS+
        // This test documents behavior and helps CI validation
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_architecture_info() {
        // This test documents the architecture we're running on
        // Useful for CI to verify Apple Silicon vs Intel

        #[cfg(target_arch = "aarch64")]
        {
            println!("Running on Apple Silicon (ARM64/aarch64)");
            assert_eq!(std::env::consts::ARCH, "aarch64");
        }

        #[cfg(target_arch = "x86_64")]
        {
            println!("Running on Intel (x86_64)");
            assert_eq!(std::env::consts::ARCH, "x86_64");
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_same_filesystem() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");

        fs::write(&file1, b"test1").unwrap();
        fs::write(&file2, b"test2").unwrap();

        // Files in same temp directory should be on same filesystem
        assert!(same_filesystem(&file1, &file2));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_hard_links() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");

        fs::write(&file1, b"test").unwrap();

        // Initially no hard links
        assert!(!has_hard_links(&file1));

        // Create hard link
        std::fs::hard_link(&file1, &file2).unwrap();

        // Now both files have nlink = 2
        assert!(has_hard_links(&file1));
        assert!(has_hard_links(&file2));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_apfs_magic_string() {
        // APFS filesystem type name is "apfs" (case-sensitive)
        // This test documents the expected string
        const APFS_TYPE_NAME: &str = "apfs";
        assert_eq!(APFS_TYPE_NAME, "apfs");
        assert_eq!(APFS_TYPE_NAME.len(), 4);
    }

    // Edge case tests for filesystem detection

    #[test]
    fn test_cow_detection_nonexistent_path() {
        // Non-existent path should return false (conservative approach)
        let nonexistent = Path::new("/nonexistent/path/that/does/not/exist.txt");
        assert!(
            !supports_cow_reflinks(nonexistent),
            "Non-existent path should return false for COW support"
        );
    }

    #[test]
    fn test_cow_detection_nonexistent_parent() {
        // Path with non-existent parent should return false
        let nonexistent = Path::new("/nonexistent/parent/dir/file.txt");
        assert!(
            !supports_cow_reflinks(nonexistent),
            "Path with non-existent parent should return false"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_same_filesystem_nonexistent_paths() {
        // Both paths non-existent should return false
        let path1 = Path::new("/nonexistent/path1.txt");
        let path2 = Path::new("/nonexistent/path2.txt");
        assert!(
            !same_filesystem(path1, path2),
            "Non-existent paths should return false"
        );

        // One exists, one doesn't - should return false
        let temp = TempDir::new().unwrap();
        let existing = temp.path().join("exists.txt");
        fs::write(&existing, b"test").unwrap();

        let nonexistent = temp.path().join("nonexistent.txt");
        assert!(
            !same_filesystem(&existing, &nonexistent),
            "Mixed existent/non-existent should return false"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_same_filesystem_parent_and_child() {
        // File and its parent directory should be on same filesystem
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("test.txt");
        fs::write(&file, b"test").unwrap();

        assert!(
            same_filesystem(&file, temp.path()),
            "File and parent directory should be on same filesystem"
        );

        // Nested file and grandparent directory
        let subdir = temp.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        let nested_file = subdir.join("nested.txt");
        fs::write(&nested_file, b"test").unwrap();

        assert!(
            same_filesystem(&nested_file, temp.path()),
            "Nested file and grandparent should be on same filesystem"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_has_hard_links_nonexistent() {
        // Non-existent file should return false
        let nonexistent = Path::new("/nonexistent/file.txt");
        assert!(
            !has_hard_links(nonexistent),
            "Non-existent file should return false for hard links"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_has_hard_links_directory() {
        // Directories have nlink >= 2 by default (., .., and parent's entry)
        // but we care about regular files
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("testdir");
        fs::create_dir(&dir).unwrap();

        // Directory nlink detection is implementation-dependent
        // Just verify it doesn't crash
        let _ = has_hard_links(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn test_hard_links_three_way() {
        // Test file with 3 hard links (nlink=3)
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");
        let file3 = temp.path().join("file3.txt");

        fs::write(&file1, b"shared").unwrap();
        fs::hard_link(&file1, &file2).unwrap();
        fs::hard_link(&file1, &file3).unwrap();

        // All three should report has_hard_links=true
        assert!(has_hard_links(&file1));
        assert!(has_hard_links(&file2));
        assert!(has_hard_links(&file3));
    }

    #[test]
    #[cfg(unix)]
    fn test_same_filesystem_symlink() {
        // Symlinks: same_filesystem should follow the symlink
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("file.txt");
        fs::write(&file, b"test").unwrap();

        let symlink = temp.path().join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&file, &symlink).unwrap();

        // Symlink and target should be on same filesystem (metadata follows symlink)
        assert!(
            same_filesystem(&file, &symlink),
            "Symlink and target should be on same filesystem"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_linux_filesystem_magic_numbers() {
        // Document the filesystem magic numbers we check for
        const BTRFS_SUPER_MAGIC: i64 = 0x9123683E;
        const XFS_SUPER_MAGIC: i64 = 0x58465342;

        // Verify these are the values we expect
        assert_eq!(BTRFS_SUPER_MAGIC, 0x9123683E);
        assert_eq!(XFS_SUPER_MAGIC, 0x58465342);

        // Verify they're different (no collision)
        assert_ne!(BTRFS_SUPER_MAGIC, XFS_SUPER_MAGIC);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_hfs_plus_not_cow() {
        // HFS+ (legacy macOS filesystem) does not support COW
        // APFS replaced it in macOS 10.13+
        // This test documents the filesystem type name for HFS+
        const HFS_PLUS_TYPE_NAME: &str = "hfs";
        assert_eq!(HFS_PLUS_TYPE_NAME, "hfs");
        assert_ne!(HFS_PLUS_TYPE_NAME, "apfs");
    }
}
