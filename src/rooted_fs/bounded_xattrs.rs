//! Descriptor-based xattr reads with size checks before allocation.
use super::{Result, RootedFsError};
use std::ffi::OsString;
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn read_from_file(file: &File) -> Result<Vec<(OsString, Vec<u8>)>> {
    use crate::protocol::{MAX_XATTR_ENTRIES, MAX_XATTR_NAME_BYTES, MAX_XATTR_TOTAL_BYTES};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStringExt;

    let names = read_bounded(0, MAX_XATTR_TOTAL_BYTES, |buffer| list(file, buffer))?;
    let mut attrs = Vec::new();
    let mut total = 0;
    for chunk in names.split_inclusive(|byte| *byte == 0) {
        if chunk.last() != Some(&0) || chunk.len() == 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid native xattr name list",
            )
            .into());
        }
        let name = &chunk[..chunk.len() - 1];
        if name.len() > MAX_XATTR_NAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native xattr name exceeds protocol limit",
            )
            .into());
        }
        if attrs.len() >= MAX_XATTR_ENTRIES {
            return Err(RootedFsError::XattrTooManyEntries {
                count: attrs.len() + 1,
                max: MAX_XATTR_ENTRIES,
            });
        }
        total += name.len();
        let c_name = CString::new(name).map_err(|_| RootedFsError::PathContainsNul)?;
        let value = read_bounded(total, MAX_XATTR_TOTAL_BYTES, |buffer| {
            get(file, &c_name, buffer)
        })?;
        total += value.len();
        attrs.push((OsString::from_vec(name.to_vec()), value));
    }
    Ok(attrs)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn read_from_file(_file: &File) -> Result<Vec<(OsString, Vec<u8>)>> {
    Err(RootedFsError::UnsupportedPlatform)
}

/// Compare only the requested value, without allocating for a potentially
/// much larger existing attribute. ERANGE means replacement is needed, not
/// permission to retry with an unbounded allocation.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn value_matches(file: &File, name: &std::ffi::OsStr, expected: &[u8]) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let name =
        std::ffi::CString::new(name.as_bytes()).map_err(|_| RootedFsError::PathContainsNul)?;
    let mut buffer = vec![0; expected.len()];
    match get(file, &name, &mut buffer) {
        Ok(length) => Ok(length == expected.len() && buffer == expected),
        Err(error) if error.raw_os_error() == Some(libc::ERANGE) => Ok(false),
        #[cfg(target_os = "linux")]
        Err(error) if error.raw_os_error() == Some(libc::ENODATA) => Ok(false),
        #[cfg(target_os = "macos")]
        Err(error) if error.raw_os_error() == Some(libc::ENOATTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(super) fn value_matches(
    _file: &File,
    _name: &std::ffi::OsStr,
    _expected: &[u8],
) -> Result<bool> {
    Err(RootedFsError::UnsupportedPlatform)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_bounded(
    used: usize,
    maximum: usize,
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> Result<Vec<u8>> {
    let length = read(&mut [])?;
    let total = used
        .checked_add(length)
        .ok_or(RootedFsError::XattrSetTooLarge {
            len: usize::MAX,
            max: maximum,
        })?;
    if total > maximum {
        return Err(RootedFsError::XattrSetTooLarge {
            len: total,
            max: maximum,
        });
    }
    let mut buffer = vec![0; length];
    let actual = read(&mut buffer)?;
    if actual > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "xattr length changed during bounded read",
        )
        .into());
    }
    buffer.truncate(actual);
    Ok(buffer)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn list(file: &File, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    let pointer = if buffer.is_empty() {
        std::ptr::null_mut()
    } else {
        buffer.as_mut_ptr().cast()
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        // SAFETY: the file FD is live; a null/zero buffer queries size, otherwise
        // pointer addresses exactly buffer.len() writable bytes for this call.
        libc::flistxattr(file.as_raw_fd(), pointer, buffer.len())
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        // SAFETY: same held descriptor and bounded output buffer; flags are zero.
        libc::flistxattr(file.as_raw_fd(), pointer, buffer.len(), 0)
    };
    syscall_length(result)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn get(file: &File, name: &std::ffi::CStr, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    let pointer = if buffer.is_empty() {
        std::ptr::null_mut()
    } else {
        buffer.as_mut_ptr().cast()
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        // SAFETY: FD and NUL-terminated name are live; output is null/zero for
        // size queries or exactly the bounded writable slice for a value read.
        libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), pointer, buffer.len())
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        // SAFETY: same live FD/name/output contract; position and flags are zero.
        libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), pointer, buffer.len(), 0, 0)
    };
    syscall_length(result)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn syscall_length(result: libc::ssize_t) -> io::Result<usize> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(result).map_err(|_| io::Error::other("xattr length is not representable"))
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn native_value_comparison_distinguishes_missing_empty_and_changed_values() {
        use xattr::FileExt;
        let file = tempfile::tempfile().unwrap();
        let name = std::ffi::OsStr::new("user.sy-compare");
        assert!(!value_matches(&file, name, b"").unwrap());
        file.set_xattr(name, b"longer value").unwrap();
        for wanted in [&b""[..], &b"short"[..], &b"longer value!"[..]] {
            assert!(!value_matches(&file, name, wanted).unwrap());
        }
        assert!(value_matches(&file, name, b"longer value").unwrap());
        file.set_xattr(name, b"").unwrap();
        assert!(value_matches(&file, name, b"").unwrap());
        assert!(!value_matches(&file, name, b"value").unwrap());
    }

    #[test]
    fn rejects_oversized_query_before_reading_or_allocating_the_value() {
        let error = read_bounded(8, 16, |buffer| {
            assert!(
                buffer.is_empty(),
                "oversized data must never receive an allocated buffer"
            );
            Ok(9)
        })
        .unwrap_err();
        assert!(matches!(
            error,
            RootedFsError::XattrSetTooLarge { len: 17, max: 16 }
        ));
    }

    #[test]
    fn concurrent_growth_cannot_escape_the_reserved_buffer() {
        let error =
            read_bounded(0, 16, |buffer| Ok(if buffer.is_empty() { 8 } else { 17 })).unwrap_err();
        assert!(
            matches!(error, RootedFsError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData)
        );
    }
}
