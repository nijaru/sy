//! Retained metadata follows the original destination FD, not exported NSS names.
use super::*;

impl RootedStagedFile {
    #[allow(clippy::unnecessary_cast)] // libc mode widths differ across Unix.
    pub(crate) fn copy_retained_metadata_blocking(
        &mut self,
        original: &File,
        overrides: crate::endpoint::io::PreservationRequest,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            let before = stat_fd(original.as_raw_fd())?;
            let current = stat_fd(self.file.as_raw_fd())?;
            if before.st_uid != current.st_uid || before.st_gid != current.st_gid {
                // SAFETY: the destination is our held private regular file;
                // both IDs come directly from the observed original inode.
                if unsafe { libc::fchown(self.file.as_raw_fd(), before.st_uid, before.st_gid) } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            // Explicit SOURCE fields supersede OLD defaults. Reading those
            // discarded fields can fail unnecessarily or exceed their budget.
            if !overrides.xattrs {
                let attrs = read_xattrs_from_file(original)?;
                metadata::mirror_xattrs_fd(&self.file, &attrs)?;
            }
            if !overrides.acl {
                copy_native_acl(original, &self.file)?;
            }
            apply_fd_metadata(
                self.file.as_raw_fd(),
                Some(before.st_mode as u32 & 0o7777),
                Some(
                    Timestamp::new(
                        before.st_mtime,
                        u32::try_from(before.st_mtime_nsec)
                            .map_err(|_| RootedFsError::TimestampOutOfRange)?,
                    )
                    .map_err(|_| RootedFsError::TimestampOutOfRange)?,
                ),
            )?;
            if !staging::same_observation(&before, &stat_fd(original.as_raw_fd())?) {
                return Err(RootedFsError::DestinationChanged(
                    self.namespace.destination_path.clone(),
                ));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (original, overrides);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }
}

#[cfg(any(all(target_os = "linux", feature = "acl"), target_os = "macos"))]
fn copy_native_acl(original: &File, destination: &File) -> Result<()> {
    use std::ffi::c_void;
    #[cfg_attr(target_os = "linux", link(name = "acl"))]
    extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut c_void;
        #[cfg(target_os = "macos")]
        fn acl_init(count: libc::c_int) -> *mut c_void;
        fn acl_set_fd(fd: libc::c_int, acl: *mut c_void) -> libc::c_int;
        fn acl_free(acl: *mut c_void) -> libc::c_int;
    }
    struct OwnedAcl(*mut c_void);
    impl Drop for OwnedAcl {
        fn drop(&mut self) {
            // SAFETY: acl_get_fd returned this owned non-null allocation;
            // its use by acl_set_fd has ended before cleanup.
            unsafe { acl_free(self.0) };
        }
    }
    // SAFETY: original pins the original observed regular inode throughout.
    let raw = unsafe { acl_get_fd(original.as_raw_fd()) };
    #[cfg(target_os = "macos")]
    let raw =
        if raw.is_null() && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            // An absent macOS extended ACL is an empty ACL, not unknown permission.
            // SAFETY: acl_init returns a new owned native empty ACL allocation.
            unsafe { acl_init(0) }
        } else {
            raw
        };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let acl = OwnedAcl(raw);
    // SAFETY: destination is our held private inode and acl is the live native
    // allocation. Copying the native ACL retains numeric IDs/raw UUID ACE order.
    if unsafe { acl_set_fd(destination.as_raw_fd(), acl.0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(all(
    unix,
    not(any(all(target_os = "linux", feature = "acl"), target_os = "macos"))
))]
fn copy_native_acl(_original: &File, _destination: &File) -> Result<()> {
    // Linux access ACLs were copied as native xattrs above even without exacl.
    Ok(())
}
