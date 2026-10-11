//! Native held-inode ACL state; comparison is advisory only for SET permission.
use super::*;

pub(super) fn read_acl_entries_fd(file: &File) -> Result<Vec<exacl::AclEntry>> {
    #[cfg(target_os = "linux")]
    return Ok(exacl::getfacl(fd_alias_path(file), None)?);
    #[cfg(target_os = "macos")]
    return Ok(acl_macos::read_fd_entries(file.as_raw_fd())?);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = file;
        Err(RootedFsError::AclUnsupported(
            "access control lists are only supported on Linux and macOS",
        ))
    }
}

pub(super) fn desired_acl_entries(file: &File, text: &str) -> Result<Vec<exacl::AclEntry>> {
    #[cfg(target_os = "linux")]
    if text.is_empty() {
        use std::os::unix::fs::PermissionsExt;
        // Empty Linux requests remove extended and default entries, not the
        // native access ACL's mandatory owner/group/other entries.
        return Ok(exacl::from_mode(file.metadata()?.permissions().mode()));
    }
    let _ = file;
    Ok(exacl::from_str(text)?)
}

pub(super) fn acl_matches_fd(file: &File, desired: &[exacl::AclEntry]) -> Result<bool> {
    #[cfg(target_os = "macos")]
    let actual = acl_macos::read_fd_native_entries(file.as_raw_fd());
    #[cfg(target_os = "linux")]
    let actual = acl_linux::read_numeric_entries(file);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let actual = match actual {
        Ok(entries) => entries,
        // READ and SET permissions are independent. Optional READ denial means
        // unknown state, never equality; required wire reads still propagate.
        Err(error) if acl_read_denied(&error) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    #[cfg(target_os = "macos")]
    {
        let desired = acl_macos::desired_native_entries(desired)?;
        // Ordered allow/deny ACEs are policy: do not sort or deduplicate.
        Ok(desired == actual)
    }
    #[cfg(target_os = "linux")]
    {
        let mut actual = actual;
        let mut desired = desired.to_vec();
        canonicalize_linux_names(&mut desired)?;
        // Actual qualifiers are already kernel UID/GIDs. Never resolve them
        // through names: reverse and forward NSS records need not agree.
        // POSIX access/default entry order is immaterial. Keep every entry,
        // including the default bit and mask; duplicates cannot match native.
        desired.sort();
        actual.sort();
        Ok(desired == actual)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (file, desired);
        Err(RootedFsError::AclUnsupported(
            "access control lists are only supported on Linux and macOS",
        ))
    }
}

pub(super) fn apply_acl_fd(file: &File, text: &str) -> Result<()> {
    let entries = desired_acl_entries(file, text)?;
    if acl_matches_fd(file, &entries)? {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    exacl::setfacl(&[fd_alias_path(file)], &entries, None)?;
    #[cfg(target_os = "macos")]
    acl_macos::write_fd_entries(file.as_raw_fd(), &entries)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(RootedFsError::AclUnsupported(
        "access control lists are only supported on Linux and macOS",
    ));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn acl_read_denied(error: &std::io::Error) -> bool {
    // exacl's path_err retains ErrorKind but discards raw errno. Do not make
    // optional comparison a READ gate merely because an error was contextualized.
    error.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[test]
fn optional_acl_read_recognizes_contextualized_permission_denial() {
    for errno in [libc::EACCES, libc::EPERM] {
        let native = std::io::Error::from_raw_os_error(errno);
        let wrapped = std::io::Error::new(native.kind(), format!("File: {native}"));
        assert_eq!(wrapped.raw_os_error(), None);
        assert!(acl_read_denied(&native));
        assert!(acl_read_denied(&wrapped));
    }
    assert!(!acl_read_denied(&std::io::Error::from_raw_os_error(
        libc::EIO
    )));
}

#[cfg(target_os = "linux")]
fn canonicalize_linux_names(entries: &mut [exacl::AclEntry]) -> Result<()> {
    // Resolve names before falling back to decimal IDs, exactly as exacl SET
    // does. Reentrant NSS lookup avoids global passwd/group pointer lifetimes.
    macro_rules! resolve {
        ($name:expr, $record:ty, $lookup:path, $id:ident) => {{
            let name = component_cstring(OsStr::new($name))?;
            let mut buffer = vec![0_u8; 4096];
            loop {
                let mut record = MaybeUninit::<$record>::uninit();
                let mut result = std::ptr::null_mut();
                // SAFETY: name is NUL terminated; record, buffer and result
                // are live writable storage of the declared lengths/types.
                let error = unsafe {
                    $lookup(
                        name.as_ptr(),
                        record.as_mut_ptr(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        &mut result,
                    )
                };
                if error == libc::ERANGE && buffer.len() < 1_048_576 {
                    buffer.resize(buffer.len() * 4, 0);
                    continue;
                }
                if error != 0 {
                    return Err(std::io::Error::from_raw_os_error(error).into());
                }
                if !result.is_null() {
                    // SAFETY: successful lookup with non-null result initialized record.
                    break unsafe { record.assume_init().$id }.to_string();
                }
                break $name
                    .parse::<u32>()
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "unknown ACL principal",
                        )
                    })?
                    .to_string();
            }
        }};
    }
    for entry in entries {
        if entry.name.is_empty() {
            continue;
        }
        entry.name = match entry.kind {
            exacl::AclEntryKind::User => {
                resolve!(&entry.name, libc::passwd, libc::getpwnam_r, pw_uid)
            }
            exacl::AclEntryKind::Group => {
                resolve!(&entry.name, libc::group, libc::getgrnam_r, gr_gid)
            }
            _ => continue,
        };
    }
    Ok(())
}
