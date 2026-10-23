//! Native parent semantics with the genuine SOURCE ROOT as confinement floor.
//! A recorded predecessor is corroboration, never a substitute for this result.
use super::*;

#[cfg(test)]
#[path = "beneath_tests.rs"]
mod tests;

#[cfg(not(target_os = "linux"))]
pub(super) fn directory(_root: RawFd, _path: &Path, logical: &Path) -> Result<OwnedFd> {
    // Darwin's O_RESOLVE_BENEATH checks `..` against the starting vnode, but
    // does not prevent a traversed child from being renamed outside that root.
    // XNU vfs_lookup.c / vfs_cache.c use ni_usedvp, not a rename-safe ancestry
    // proof. A native rename-race probe on Darwin 25.6.0 returned an outside FD.
    // Reject before lookup: checking that returned FD would already be outside
    // discovery. Flag availability and sequential tests cannot qualify this.
    Err(RootedFsError::SourceFollowUnsupported(
        logical.to_path_buf(),
    ))
}

#[cfg(target_os = "linux")]
pub(super) fn directory(root: RawFd, path: &Path, logical: &Path) -> Result<OwnedFd> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RootedFsError::PathContainsNul)?;
    let unsupported = || RootedFsError::SourceFollowUnsupported(logical.to_path_buf());
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        // Linux uapi: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
        // RESOLVE_NO_MAGICLINKS. No RESOLVE_IN_ROOT or silent fallback.
        resolve: 0x08 | 0x04 | 0x02,
    };
    // SAFETY: root is held, path is live; how has the Linux open_how ABI.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root,
            path.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EXDEV | libc::ENOSYS | libc::ENAMETOOLONG) => return Err(unsupported()),
            // openat2 documents this specific confinement race. EINVAL remains
            // fatal; do not turn arbitrary syscall errors into CHILD retries.
            Some(libc::EAGAIN) => {
                return Err(RootedFsError::SourceBindingChanged(logical.to_path_buf()));
            }
            _ => return Err(source_read_race(error.into(), logical)),
        }
    }
    // SAFETY: native open returned a fresh nonnegative C-int descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
