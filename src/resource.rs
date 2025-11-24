use crate::error::{Result, SyncError};
use std::path::Path;

/// Check if there's enough disk space on the destination filesystem
pub fn check_disk_space(path: &Path, bytes_needed: u64) -> Result<()> {
    // Get available space on the filesystem
    let available = get_available_space(path)?;

    // Require 10% buffer for safety (temp files, metadata, etc.)
    let required = bytes_needed + (bytes_needed / 10);

    if available < required {
        return Err(SyncError::InsufficientDiskSpace {
            path: path.to_path_buf(),
            required,
            available,
        });
    }

    // Warn if less than 20% buffer
    let comfortable = bytes_needed + (bytes_needed / 5);
    if available < comfortable {
        tracing::warn!(
            "Low disk space: {} available, {} needed (plus buffer)",
            format_bytes(available),
            format_bytes(bytes_needed)
        );
    }

    Ok(())
}

/// Check file descriptor limits and warn if we might exceed them
#[cfg(unix)]
pub fn check_fd_limits(parallel_workers: usize) -> Result<()> {
    use libc::{getrlimit, rlimit, RLIMIT_NOFILE};

    let mut limit = rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    unsafe {
        if getrlimit(RLIMIT_NOFILE, &mut limit) != 0 {
            // If we can't get the limit, just warn and continue
            tracing::warn!("Failed to get file descriptor limits");
            return Ok(());
        }
    }

    let soft_limit = limit.rlim_cur as usize;
    let hard_limit = limit.rlim_max as usize;

    // Estimate FDs needed: each worker might need multiple FDs
    // (source file, dest file, temp files, sockets for SSH, etc.)
    let estimated_fds = parallel_workers * 10;

    // Also account for FDs already open by the process
    let reserved_fds = 50; // Reserve for stdout, stderr, logging, etc.
    let total_needed = estimated_fds + reserved_fds;

    if total_needed > soft_limit {
        tracing::warn!(
            "May hit file descriptor limit: {} workers need ~{} FDs, but soft limit is {}",
            parallel_workers,
            total_needed,
            soft_limit
        );

        if total_needed <= hard_limit {
            tracing::info!(
                "Consider increasing limit: ulimit -n {} (hard limit: {})",
                hard_limit,
                hard_limit
            );
        } else {
            tracing::warn!(
                "Requested workers ({}) may exceed hard limit ({}). Consider reducing with -j flag.",
                parallel_workers,
                hard_limit / 10
            );
        }
    }

    Ok(())
}

/// Non-Unix platforms don't have getrlimit
#[cfg(not(unix))]
pub fn check_fd_limits(_parallel_workers: usize) -> Result<()> {
    // Windows doesn't have the same FD limits concept
    // Just succeed silently
    Ok(())
}

/// Get available space on filesystem containing the given path
#[cfg(unix)]
fn get_available_space(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Create parent directory if it doesn't exist (for statvfs)
    let check_path = if path.exists() {
        path
    } else if let Some(parent) = path.parent() {
        parent
    } else {
        Path::new("/")
    };

    let path_cstr = CString::new(check_path.as_os_str().as_bytes())
        .map_err(|e| SyncError::Io(std::io::Error::other(format!("Invalid path: {}", e))))?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

    unsafe {
        if libc::statvfs(path_cstr.as_ptr(), &mut stat) != 0 {
            return Err(SyncError::Io(std::io::Error::last_os_error()));
        }
    }

    // Available space = available blocks * block size
    // Note: f_bavail and f_frsize types vary by platform (u32 on macOS, u64 on Linux)
    #[allow(clippy::unnecessary_cast)]
    let available = stat.f_bavail as u64 * stat.f_frsize as u64;
    Ok(available)
}

/// Windows implementation using GetDiskFreeSpaceEx
#[cfg(windows)]
fn get_available_space(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // Check parent directory if path doesn't exist (same as Unix behavior)
    let check_path = if path.exists() {
        path
    } else if let Some(parent) = path.parent() {
        parent
    } else {
        path
    };

    let wide_path: Vec<u16> = check_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    let mut free_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut available_bytes: u64 = 0;

    unsafe {
        if GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut available_bytes as *mut u64 as *mut _,
            &mut total_bytes as *mut u64 as *mut _,
            &mut free_bytes as *mut u64 as *mut _,
        ) == 0
        {
            return Err(SyncError::Io(std::io::Error::last_os_error()));
        }
    }

    Ok(available_bytes)
}

/// Format bytes for human-readable display
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_check_disk_space_sufficient() {
        let temp = TempDir::new().unwrap();

        // Check for 1KB - should always pass
        let result = check_disk_space(temp.path(), 1024);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_disk_space_insufficient() {
        let temp = TempDir::new().unwrap();

        // Check for impossibly large amount (1 PB)
        let result = check_disk_space(temp.path(), 1024 * 1024 * 1024 * 1024 * 1024);
        assert!(result.is_err());

        if let Err(SyncError::InsufficientDiskSpace {
            path: _,
            required,
            available,
        }) = result
        {
            assert!(required > available);
        } else {
            panic!("Expected InsufficientDiskSpace error");
        }
    }

    #[test]
    fn test_get_available_space() {
        let temp = TempDir::new().unwrap();
        let available = get_available_space(temp.path()).unwrap();

        // Should have at least 1MB free (conservative check)
        assert!(
            available > 1024 * 1024,
            "Expected at least 1MB free, got {}",
            available
        );
    }

    #[test]
    fn test_get_available_space_nonexistent_path() {
        let temp = TempDir::new().unwrap();
        let nonexistent = temp.path().join("nonexistent");

        // Should check parent directory
        let result = get_available_space(&nonexistent);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn test_check_fd_limits() {
        // Should succeed and just warn if limits might be exceeded
        let result = check_fd_limits(10);
        assert!(result.is_ok());

        // Even with high worker count, should succeed (just warn)
        let result = check_fd_limits(1000);
        assert!(result.is_ok());
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 1024), "1.00 TB");
    }
}
