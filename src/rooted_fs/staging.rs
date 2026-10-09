//! Separate private-entry cleanup ownership from its prepared publication seal.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct VerifiedObservation {
    #[cfg(unix)]
    pub(super) stat: libc::stat,
}

#[cfg(unix)]
pub(super) struct OwnedStagingEntry {
    pub(super) file: File,
    pub(super) created: libc::stat,
}

#[cfg(unix)]
pub(super) fn same_inode(before: &libc::stat, after: &libc::stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_mode & libc::S_IFMT == after.st_mode & libc::S_IFMT
}

/// All properties except atime (our reads), ctime and nlink (native namespace
/// bookkeeping). Callers must check the exact expected nlink delta separately.
#[cfg(unix)]
pub(super) fn same_preserved_state(before: &libc::stat, after: &libc::stat) -> bool {
    let same = same_inode(before, after)
        && before.st_mode == after.st_mode
        && before.st_uid == after.st_uid
        && before.st_gid == after.st_gid
        && before.st_rdev == after.st_rdev
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec;
    #[cfg(target_os = "macos")]
    let same = same && before.st_flags == after.st_flags;
    same
}

#[cfg(unix)]
pub(super) fn same_observation(before: &libc::stat, after: &libc::stat) -> bool {
    same_preserved_state(before, after)
        && before.st_nlink == after.st_nlink
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
}

impl RootedNamespaceTransaction {
    #[cfg(unix)]
    fn staging_changed(&self) -> RootedFsError {
        RootedFsError::StagingEntryChanged(self.destination_path.clone())
    }

    /// Register immediately after creation, before fallible metadata preparation.
    /// An absent owner is never a wildcard permitting cleanup of a named entry.
    #[cfg(unix)]
    pub(super) fn register_staging(&mut self, file: &File) -> Result<()> {
        let created = stat_fd(file.as_raw_fd())?;
        self.cleanup_owned = Some(created);
        self.staged = Some(OwnedStagingEntry {
            file: file.try_clone()?,
            created,
        });
        self.observe_staging()?;
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn observe_staging(&self) -> Result<VerifiedObservation> {
        let owned = self.staged.as_ref().ok_or_else(|| self.staging_changed())?;
        let held = stat_fd(owned.file.as_raw_fd())?;
        let named = stat_at_optional(self.staging_dir_fd.as_raw_fd(), &self.temp_name)?
            .ok_or_else(|| self.staging_changed())?;
        if !same_inode(&owned.created, &held) || !same_observation(&held, &named) {
            return Err(self.staging_changed());
        }
        if let Some(sealed) = self.sealed {
            if !same_observation(&sealed.stat, &held) {
                return Err(self.staging_changed());
            }
        }
        Ok(VerifiedObservation { stat: held })
    }

    #[cfg(unix)]
    pub(super) fn seal_staging(&mut self) -> Result<()> {
        let observed = self.observe_staging()?;
        self.sealed.get_or_insert(observed);
        Ok(())
    }

    /// Advance only our exact namespace link-count transition. Stat checks do
    /// not isolate a foreign ctime-only edit during our syscall; this is not CAS.
    #[cfg(unix)]
    pub(super) fn advance_staging(&mut self, link_delta: i8) -> Result<()> {
        let before = self.sealed.ok_or_else(|| self.staging_changed())?.stat;
        let owned = self.staged.as_ref().ok_or_else(|| self.staging_changed())?;
        let after = stat_fd(owned.file.as_raw_fd())?;
        let links = match link_delta {
            1 => before.st_nlink.checked_add(1),
            -1 => before.st_nlink.checked_sub(1),
            0 => Some(before.st_nlink),
            _ => None,
        };
        if !same_preserved_state(&before, &after) || links != Some(after.st_nlink) {
            return Err(self.staging_changed());
        }
        self.sealed = Some(VerifiedObservation { stat: after });
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn remove_owned_contents(&self) -> Result<bool> {
        let Some(named) = stat_at_optional(self.staging_dir_fd.as_raw_fd(), &self.temp_name)?
        else {
            return Ok(false);
        };
        if !self
            .cleanup_owned
            .as_ref()
            .is_some_and(|owned| same_inode(owned, &named))
        {
            return Err(self.staging_changed());
        }
        // Mutable private directory children do not invalidate inode ownership.
        // Never recursively delete them: rmdir retains a nonempty artifact.
        unlink_at(
            self.staging_dir_fd.as_raw_fd(),
            &self.temp_name,
            named.st_mode & libc::S_IFMT == libc::S_IFDIR,
        )?;
        Ok(true)
    }
}
