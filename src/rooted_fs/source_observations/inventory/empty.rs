//! Native empty-directory evidence, captured before omission. This does not
//! enumerate a pruned subtree or promote it to a completed traversal.
use super::*;
#[cfg(all(test, target_os = "macos"))]
#[path = "empty_tests.rs"]
mod tests;

fn stamp(stat: &libc::stat) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sy/source-empty-directory/v1\0");
    #[allow(clippy::unnecessary_cast)] // Native stat widths vary across Unix.
    for value in [
        stat.st_dev as i64,
        stat.st_ino as i64,
        stat.st_mode as i64,
        stat.st_nlink as i64,
        stat.st_size as i64,
        stat.st_mtime as i64,
        stat.st_mtime_nsec as i64,
        stat.st_ctime as i64,
        stat.st_ctime_nsec as i64,
    ] {
        hash.update(&value.to_le_bytes());
    }
    *hash.finalize().as_bytes()
}

#[cfg(target_os = "macos")]
fn entry_count(fd: RawFd, name: Option<&OsStr>) -> Result<Option<u32>> {
    let common = libc::ATTR_CMN_RETURNED_ATTRS;
    let directory = libc::ATTR_DIR_ENTRYCOUNT;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: common,
        volattr: 0,
        dirattr: directory,
        fileattr: 0,
        forkattr: 0,
    };
    // Length, returned attribute set, and a u32 entry count excluding . and ..
    // (getattrlist(2), ATTR_DIR_ENTRYCOUNT). APFS stores its child count natively.
    let mut bytes = [0u32; 7];
    let name = name.map(component_cstring).transpose()?;
    // SAFETY: fd and optional single component are live; attrs and aligned
    // output have their declared sizes. No link target or child is traversed.
    let result = unsafe {
        match name {
            Some(name) => libc::getattrlistat(
                fd,
                name.as_ptr(),
                (&mut attrs as *mut libc::attrlist).cast(),
                bytes.as_mut_ptr().cast(),
                std::mem::size_of_val(&bytes),
                libc::c_ulong::from(libc::FSOPT_NOFOLLOW | libc::FSOPT_RETURN_REALDEV),
            ),
            None => libc::fgetattrlist(
                fd,
                (&mut attrs as *mut libc::attrlist).cast(),
                bytes.as_mut_ptr().cast(),
                std::mem::size_of_val(&bytes),
                libc::FSOPT_RETURN_REALDEV,
            ),
        }
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        // This optional metadata is not a new read-access requirement for an
        // omitted directory. Missing capability grants no exclusion evidence.
        if matches!(
            error.raw_os_error(),
            Some(libc::EACCES | libc::EPERM | libc::ENOTSUP)
        ) {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok((bytes[..6] == [28, common, 0, directory, 0, 0]).then_some(bytes[6]))
}

#[cfg(not(target_os = "macos"))]
fn entry_count(_fd: RawFd, _name: Option<&OsStr>) -> Result<Option<u32>> {
    Ok(None)
}

fn stat(fd: RawFd, name: Option<&OsStr>) -> Result<libc::stat> {
    match name {
        Some(name) => stat_at_no_follow(fd, name),
        None => stat_fd(fd),
    }
}

impl Catalogue {
    /// Only initial acquisition may add evidence. Replays never repair an
    /// originally nonempty/unknown directory after its descendants disappear.
    pub(in crate::rooted_fs::source_observations) fn capture_empty_directory(
        &mut self,
        path: &Path,
        fd: RawFd,
        name: Option<&OsStr>,
        original: &Observation,
        before: &libc::stat,
    ) -> Result<()> {
        if original.followed
            || original.named.kind != libc::S_IFDIR as u64
            || original.object_key.is_none()
        {
            return Ok(());
        }
        if entry_count(fd, name)? != Some(0) {
            return Ok(());
        }
        let object = object_key(fd, name)?;
        let after = stat(fd, name)?;
        if object != original.object_key
            || !original
                .named
                .same_binding(NativeObservation::from_stat(&after)?)
            || stamp(before) != stamp(&after)
        {
            return Err(self.changed(path.to_path_buf()));
        }
        self.set_index_record(
            state_key(b"empty", path_key(path)),
            IndexRecord::EmptyStamp(stamp(before)),
        )?;
        Ok(())
    }

    pub(super) fn empty_directory_stamp(&mut self, key: [u8; 32]) -> Result<Option<[u8; 32]>> {
        match self.index_record(state_key(b"empty", key))? {
            None => Ok(None),
            Some(IndexRecord::EmptyStamp(stamp)) => Ok(Some(stamp)),
            Some(_) => Err(invalid().into()),
        }
    }

    pub(super) fn check_empty_directory(
        &mut self,
        key: [u8; 32],
        fd: RawFd,
        name: Option<&OsStr>,
        path: &Path,
    ) -> Result<()> {
        let expected = self.empty_directory_stamp(key)?.ok_or_else(invalid)?;
        let original = self.get(key)?.ok_or_else(invalid)?;
        let before = stat(fd, name)?;
        if stamp(&before) != expected
            || object_key(fd, name)? != original.object_key
            || entry_count(fd, name)? != Some(0)
            || stamp(&stat(fd, name)?) != expected
        {
            return Err(self.changed(path.to_path_buf()));
        }
        Ok(())
    }
}
