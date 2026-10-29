//! Pin a genuine current-thread procfs FD link, not a pathname impersonation.
use super::*;

pub(super) struct ProcAddress(File);

fn stat(fd: RawFd) -> Result<Option<libc::statx>> {
    let mut value = MaybeUninit::<libc::statx>::zeroed();
    let mask = libc::STATX_TYPE | libc::STATX_INO | libc::STATX_MNT_ID_UNIQUE;
    // SAFETY: fd is held, the empty string is terminated, and value owns one
    // writable statx. EMPTY_PATH inspects the held link/directory itself.
    if unsafe {
        libc::statx(
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            mask,
            value.as_mut_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
            return Ok(None);
        }
        return Err(error.into());
    }
    // SAFETY: successful statx initialized the output.
    let value = unsafe { value.assume_init() };
    Ok((value.stx_mask & mask == mask && value.stx_mnt_id != 0).then_some(value))
}

fn read_link(fd: RawFd) -> Result<Option<OsString>> {
    let mut bytes = [0; libc::PATH_MAX as usize];
    // SAFETY: fd holds the procfs link itself, the empty name is terminated,
    // and bytes owns the complete writable output. No projected path is opened.
    let count =
        unsafe { libc::readlinkat(fd, c"".as_ptr(), bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let count = usize::try_from(count)
        .map_err(|_| std::io::Error::other("invalid native address length"))?;
    if count == bytes.len() {
        return Ok(None);
    }
    Ok(Some(OsStr::from_bytes(&bytes[..count]).to_os_string()))
}

impl ProcAddress {
    pub(super) fn new(fd: RawFd) -> Result<Option<Self>> {
        let proc =
            source_observations::operator::directory_at(libc::AT_FDCWD, OsStr::new("/proc"))?;
        Self::from_root(proc, fd)
    }

    fn from_root(proc: OwnedFd, fd: RawFd) -> Result<Option<Self>> {
        let mut fs = MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: proc is held and fs owns writable statfs storage.
        if unsafe { libc::fstatfs(proc.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful fstatfs initialized fs.
        let fs = unsafe { fs.assume_init() };
        let Some(root) = stat(proc.as_raw_fd())? else {
            return Ok(None);
        };
        // Linux proc_ns.h reserves PROC_ROOT_INO=1. A procfs task/fd subtree
        // (including one bind-mounted at /proc) is not the genuine procfs root.
        if i128::from(fs.f_type) != i128::from(libc::PROC_SUPER_MAGIC)
            || root.stx_ino != 1
            || u32::from(root.stx_mode) & libc::S_IFMT != libc::S_IFDIR
        {
            return Ok(None);
        }
        let own = open_symlink_at(proc.as_raw_fd(), OsStr::new("thread-self"))?;
        if stat(own.as_raw_fd())?.is_none_or(|value| value.stx_mnt_id != root.stx_mnt_id) {
            return Ok(None);
        }
        // SAFETY: gettid takes no pointers and returns the current kernel TID.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let expected = format!("{}/task/{tid}", std::process::id());
        if read_link(own.as_raw_fd())?.as_deref() != Some(OsStr::new(&expected)) {
            return Ok(None);
        }
        drop(own);
        let mut cursor = proc;
        for name in expected.split('/').chain(std::iter::once("fd")) {
            cursor =
                source_observations::operator::directory_at(cursor.as_raw_fd(), OsStr::new(name))?;
            if stat(cursor.as_raw_fd())?.is_none_or(|value| value.stx_mnt_id != root.stx_mnt_id) {
                // Even another genuine procfs mount can substitute a different
                // task at a component. All steps must retain this mounted view.
                return Ok(None);
            }
        }
        let link = open_symlink_at(cursor.as_raw_fd(), OsStr::new(&fd.to_string()))?;
        if stat(link.as_raw_fd())?.is_none_or(|value| value.stx_mnt_id != root.stx_mnt_id) {
            return Ok(None);
        }
        Ok(Some(Self(link)))
    }

    pub(super) fn read(&self) -> Result<Option<OsString>> {
        let Some(address) = read_link(self.0.as_raw_fd())? else {
            return Ok(None);
        };
        let bytes = address.as_bytes();
        if !bytes.starts_with(b"/") || bytes.ends_with(b" (deleted)") || bytes.contains(&0) {
            return Ok(None);
        }
        Ok(Some(address))
    }
}

#[cfg(test)]
mod tests;
