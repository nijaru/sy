//! Native held-inode ACL state; comparison is advisory only for SET permission.
use super::*;

pub(super) fn read_acl_entries_fd(file: &File) -> Result<Vec<exacl::AclEntry>> {
    #[cfg(target_os = "linux")]
    return Ok(acl_linux::read_numeric_entries(file)?);
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
    let entries = exacl::from_str(text)?;
    validate_acl_entries(&entries)?;
    Ok(entries)
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
        validate_acl_entries(desired)?;
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
    acl_linux::write_numeric_entries(file, &entries)?;
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

pub(super) fn validate_acl_entries(entries: &[exacl::AclEntry]) -> Result<()> {
    #[cfg(target_os = "linux")]
    acl_linux::validate_numeric_entries(entries)?;
    #[cfg(target_os = "macos")]
    acl_macos::validate_raw_entries(entries)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = entries;
        return Err(RootedFsError::AclUnsupported("unsupported ACL platform"));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Ok(())
}
