//! Read-only namespace observations of an already held directory.
//!
//! A directory observation is not a uniform profile of its descendants:
//! casefold flags and nested mounts can introduce different rules.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::engine::namespace::Folding;
#[cfg(unix)]
use crate::engine::namespace::NamespaceSemantics;
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd};

#[cfg(target_os = "macos")]
pub(crate) fn directory_semantics(directory: BorrowedFd<'_>) -> NamespaceSemantics {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the borrowed descriptor remains live and stat is a writable
    // output buffer of the type fstatfs initializes on success.
    if unsafe { libc::fstatfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return NamespaceSemantics::UNSPECIFIED;
    }
    // SAFETY: successful fstatfs initialized the structure.
    let stat = unsafe { stat.assume_init() };
    let name = stat.f_fstypename;
    let normalization = if [b"apfs".as_slice(), b"hfs", b"hfsx"]
        .iter()
        .any(|expected| {
            name.iter()
                .take_while(|&&byte| byte != 0)
                .map(|&byte| byte as u8)
                .eq(expected.iter().copied())
        }) {
        Folding::Folded
    } else {
        Folding::Unspecified
    };
    NamespaceSemantics {
        case: macos_case_folding(directory),
        normalization,
    }
}

#[cfg(target_os = "macos")]
fn macos_case_folding(directory: BorrowedFd<'_>) -> Folding {
    #[repr(C)]
    struct VolCapabilitiesBuffer {
        length: u32,
        capabilities: libc::vol_capabilities_attr_t,
    }
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
    // SAFETY: directory remains live; attributes and buffer have the native
    // layouts for this fixed volume-capabilities request. fgetattrlist is
    // bounded by the supplied buffer size and follows the held vnode, not a
    // fresh lookup of the original pathname.
    let status = unsafe {
        libc::fgetattrlist(
            directory.as_raw_fd(),
            (&mut attributes as *mut libc::attrlist).cast(),
            (&mut buffer as *mut VolCapabilitiesBuffer).cast(),
            std::mem::size_of::<VolCapabilitiesBuffer>(),
            0,
        )
    };
    if status != 0 || buffer.length as usize != std::mem::size_of::<VolCapabilitiesBuffer>() {
        return Folding::Unspecified;
    }
    const VOL_CAPABILITIES_FORMAT: usize = 0;
    let capabilities = buffer.capabilities.capabilities[VOL_CAPABILITIES_FORMAT];
    let valid = buffer.capabilities.valid[VOL_CAPABILITIES_FORMAT];
    if valid & libc::VOL_CAP_FMT_CASE_SENSITIVE == 0 {
        Folding::Unspecified
    } else if capabilities & libc::VOL_CAP_FMT_CASE_SENSITIVE != 0 {
        Folding::Exact
    } else {
        Folding::Folded
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn directory_semantics(directory: BorrowedFd<'_>) -> NamespaceSemantics {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the descriptor remains live and stat is a writable native output
    // buffer. No pathname is resolved and no filesystem metadata is changed.
    if unsafe { libc::fstatfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return NamespaceSemantics::UNSPECIFIED;
    }
    // SAFETY: successful fstatfs initialized the structure.
    let stat = unsafe { stat.assume_init() };
    match stat.f_type {
        // ext4/F2FS folding is a directory property, not a property of the
        // filesystem type. A failed flag query cannot establish byte exactness.
        0xEF53 | 0xF2F5_2010 => casefold_semantics(linux_casefolded(directory)),
        // These formats have byte-exact directory names.
        0x9123_683E | 0x0102_1994 | 0x8584_58f6 | 0xE0F5_E1E2 | 0x3434 | 0x7371_7368 => {
            NamespaceSemantics::BYTE_EXACT
        }
        // FAT/exFAT fold case; their normalization model is not qualified.
        0x4D44 | 0x2011_BAB0 => NamespaceSemantics {
            case: Folding::Folded,
            normalization: Folding::Unspecified,
        },
        // XFS can use the ascii-ci format, and NTFS drivers/mount options
        // differ. Their magic numbers alone do not establish name rules.
        // FUSE, NFS, ZFS, CIFS, overlayfs and unknown formats are also unqualified.
        _ => NamespaceSemantics::UNSPECIFIED,
    }
}

#[cfg(target_os = "linux")]
fn casefold_semantics(casefolded: Option<bool>) -> NamespaceSemantics {
    match casefolded {
        Some(true) => NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED,
        Some(false) => NamespaceSemantics::BYTE_EXACT,
        None => NamespaceSemantics::UNSPECIFIED,
    }
}

#[cfg(target_os = "linux")]
fn linux_casefolded(directory: BorrowedFd<'_>) -> Option<bool> {
    // libc supplies the target-specific _IOR('f', 1, long) request. The
    // kernel's returned value is an unsigned int, not a native long.
    const FS_CASEFOLD_FL: libc::c_uint = 0x4000_0000;
    let mut flags: libc::c_uint = 0;
    // SAFETY: directory remains live and flags is a writable unsigned int, the output
    // type used by the kernel's FS_IOC_GETFLAGS implementation.
    let status = unsafe {
        libc::ioctl(
            directory.as_raw_fd(),
            libc::FS_IOC_GETFLAGS,
            (&mut flags as *mut libc::c_uint).cast::<libc::c_void>(),
        )
    };
    (status == 0).then_some(flags & FS_CASEFOLD_FL != 0)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn directory_semantics(_directory: BorrowedFd<'_>) -> NamespaceSemantics {
    NamespaceSemantics::UNSPECIFIED
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_casefold_query_does_not_certify_exact_names() {
        use super::*;
        use std::os::fd::AsFd;
        let unsupported = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            casefold_semantics(linux_casefolded(unsupported.as_fd())),
            NamespaceSemantics::UNSPECIFIED
        );
        for (observation, expected) in [
            (None, NamespaceSemantics::UNSPECIFIED),
            (Some(false), NamespaceSemantics::BYTE_EXACT),
            (
                Some(true),
                NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED,
            ),
        ] {
            assert_eq!(casefold_semantics(observation), expected);
        }
    }
}
