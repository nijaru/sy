//! Independent bounded native directory cursor; I/O failure is never EOF.
use std::ffi::{CStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;

pub(crate) struct DirectoryStream {
    directory: *mut libc::DIR,
    exhausted: bool,
}

impl DirectoryStream {
    pub(crate) fn open(fd: RawFd) -> io::Result<Self> {
        let scan_fd = unsafe {
            // SAFETY: `fd` is a live directory descriptor and `.` is a fixed native
            // component. Reopening it creates a distinct open file description, so
            // concurrent scans do not share the directory offset as with dup(2).
            libc::openat(
                fd,
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if scan_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh descriptor owned by this call.
        Self::from_directory(unsafe { OwnedFd::from_raw_fd(scan_fd) })
    }

    /// Consume a freshly opened directory; callers must not share its open-file
    /// description with another cursor. The descriptor remains available for
    /// confined child lookups and identity checks through exact exhaustion.
    pub(crate) fn from_directory(fd: OwnedFd) -> io::Result<Self> {
        let directory = unsafe {
            // SAFETY: fd remains live and owned here; fdopendir consumes it only
            // on success. Failure leaves the OwnedFd responsible for closing it.
            libc::fdopendir(fd.as_raw_fd())
        };
        if directory.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _ = fd.into_raw_fd();
        Ok(Self {
            directory,
            exhausted: false,
        })
    }
}

impl AsRawFd for DirectoryStream {
    fn as_raw_fd(&self) -> RawFd {
        // SAFETY: a successfully constructed stream owns a live DIR until Drop,
        // including after EOF or a read error. dirfd borrows its descriptor.
        unsafe { libc::dirfd(self.directory) }
    }
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: the DIR and its descriptor are uniquely owned by this guard.
            libc::closedir(self.directory);
        }
    }
}

impl Iterator for DirectoryStream {
    type Item = io::Result<OsString>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.exhausted {
            errno::set_errno(errno::Errno(0));
            let entry = unsafe {
                // SAFETY: self owns a live DIR pointer until Drop.
                libc::readdir(self.directory)
            };
            if entry.is_null() {
                let errno = errno::errno().0;
                self.exhausted = true;
                return (errno != 0).then(|| Err(io::Error::from_raw_os_error(errno)));
            }
            let name = unsafe {
                // SAFETY: successful readdir returns a NUL-terminated d_name
                // valid until the next call; copy it before advancing.
                CStr::from_ptr((*entry).d_name.as_ptr())
            }
            .to_bytes();
            if name != b"." && name != b".." {
                return Some(Ok(OsString::from_vec(name.to_vec())));
            }
        }
        None
    }
}
