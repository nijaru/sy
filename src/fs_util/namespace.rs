//! Native name rules from a held directory, never a descendant-wide profile.
use crate::engine::namespace::{Folding, NamespaceSemantics};
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd};

/// Only qualified ordinary-mkdir inheritance permits a precise prediction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirectoryNamespace {
    pub semantics: NamespaceSemantics,
    pub filesystem: [u64; 3],
    pub inherits: bool,
}

#[cfg(unix)]
pub(crate) fn directory_semantics(fd: BorrowedFd<'_>) -> std::io::Result<NamespaceSemantics> {
    Ok(qualify_directory(fd)?.semantics)
}

#[cfg(target_os = "linux")]
pub(crate) fn qualify_directory(fd: BorrowedFd<'_>) -> std::io::Result<DirectoryNamespace> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: live FD and writable native output buffer.
    if unsafe { libc::fstatfs(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful fstatfs initialized stat.
    let stat = unsafe { stat.assume_init() };
    let mut fsid = [0u8; 8];
    // SAFETY: fsid is an initialized native fixed-size value; copy only its bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&stat.f_fsid as *const libc::fsid_t).cast::<u8>(),
            fsid.as_mut_ptr(),
            8,
        )
    };
    let (semantics, inherits) = match stat.f_type {
        // ext4, F2FS and tmpfs have per-directory casefold flags inherited by mkdir.
        0xEF53 | 0xF2F5_2010 | 0x0102_1994 => match linux_casefolded(fd)? {
            Some(false) => (NamespaceSemantics::BYTE_EXACT, true),
            Some(true) => (NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED, true),
            None => (NamespaceSemantics::UNSPECIFIED, false),
        },
        // Native Btrfs dir-item comparison is bytewise. These other ordinary
        // directory formats also have no per-directory folding switch.
        0x9123_683E | 0x8584_58f6 | 0xE0F5_E1E2 | 0x3434 | 0x7371_7368 => {
            (NamespaceSemantics::BYTE_EXACT, true)
        }
        0x4D44 | 0x2011_BAB0 => (
            NamespaceSemantics {
                case: Folding::Folded,
                normalization: Folding::Unspecified,
            },
            true,
        ),
        // XFS ascii-ci geometry is not queried here. Magic is not proof.
        _ => (NamespaceSemantics::UNSPECIFIED, false),
    };
    Ok(DirectoryNamespace {
        semantics,
        filesystem: [stat.f_type as u64, u64::from_ne_bytes(fsid), 0],
        inherits,
    })
}

#[cfg(target_os = "linux")]
fn linux_casefolded(fd: BorrowedFd<'_>) -> std::io::Result<Option<bool>> {
    let mut flags: libc::c_uint = 0;
    // SAFETY: live FD and the unsigned-int output used by FS_IOC_GETFLAGS.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } == 0 {
        return Ok(Some(flags & 0x4000_0000 != 0));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::ENOSYS) => Ok(None),
        _ => Err(error),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn qualify_directory(fd: BorrowedFd<'_>) -> std::io::Result<DirectoryNamespace> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: live FD and writable native output buffer.
    if unsafe { libc::fstatfs(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful fstatfs initialized stat.
    let stat = unsafe { stat.assume_init() };
    #[repr(C)]
    struct Buffer {
        length: u32,
        capabilities: libc::vol_capabilities_attr_t,
    }
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buffer = Buffer {
        length: 0,
        capabilities: libc::vol_capabilities_attr_t {
            capabilities: [0; 4],
            valid: [0; 4],
        },
    };
    // SAFETY: fixed native attribute request and bounded correctly laid-out buffer.
    let status = unsafe {
        libc::fgetattrlist(
            fd.as_raw_fd(),
            (&mut attrs as *mut libc::attrlist).cast(),
            (&mut buffer as *mut Buffer).cast(),
            std::mem::size_of::<Buffer>(),
            0,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::ENOSYS)) {
            return Err(error);
        }
    }
    if status == 0 && buffer.length as usize != std::mem::size_of::<Buffer>() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid native volume-capabilities response",
        ));
    }
    let case =
        if status != 0 || buffer.capabilities.valid[0] & libc::VOL_CAP_FMT_CASE_SENSITIVE == 0 {
            Folding::Unspecified
        } else if buffer.capabilities.capabilities[0] & libc::VOL_CAP_FMT_CASE_SENSITIVE != 0 {
            Folding::Exact
        } else {
            Folding::Folded
        };
    let hfs = stat
        .f_fstypename
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .eq(b"hfs".iter().copied())
        || stat
            .f_fstypename
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as u8)
            .eq(b"hfsx".iter().copied());
    let mut fsid = [0u8; 8];
    // SAFETY: initialized fixed-size native fsid, copied as bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&stat.f_fsid as *const libc::fsid_t).cast::<u8>(),
            fsid.as_mut_ptr(),
            8,
        )
    };
    Ok(DirectoryNamespace {
        semantics: NamespaceSemantics {
            case,
            normalization: if hfs {
                Folding::Folded
            } else {
                Folding::Unspecified
            },
        },
        filesystem: [
            stat.f_type as u64,
            u64::from_ne_bytes(fsid),
            stat.f_fssubtype as u64,
        ],
        // APFS logical parent does not establish a new child's physical volume
        // or its independent normalization features. Predict conservatively.
        inherits: hfs,
    })
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) fn qualify_directory(_fd: BorrowedFd<'_>) -> std::io::Result<DirectoryNamespace> {
    Ok(DirectoryNamespace {
        semantics: NamespaceSemantics::UNSPECIFIED,
        filesystem: [0; 3],
        inherits: false,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    #[test]
    fn unsupported_flags_do_not_certify_exact_names() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(linux_casefolded(file.as_fd()).unwrap(), None);
    }
}
