//! One observed inode owns every requested in-place preservation field.
use super::*;

#[derive(Debug, Default)]
pub(crate) struct MetadataPreservation<'a> {
    pub xattrs: Option<&'a [(OsString, Vec<u8>)]>,
    pub acl: Option<&'a str>,
    pub bsd_flags: Option<u32>,
}

impl MetadataPreservation<'_> {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(xattrs) = self.xattrs {
            if xattrs.len() > crate::protocol::MAX_XATTR_ENTRIES {
                return Err(RootedFsError::XattrTooManyEntries {
                    count: xattrs.len(),
                    max: crate::protocol::MAX_XATTR_ENTRIES,
                });
            }
            #[cfg(unix)]
            let total = xattrs.iter().try_fold(0_usize, |total, (name, value)| {
                total
                    .checked_add(name.as_bytes().len())?
                    .checked_add(value.len())
            });
            #[cfg(not(unix))]
            let total = xattrs.iter().try_fold(0_usize, |total, (name, value)| {
                total.checked_add(name.len())?.checked_add(value.len())
            });
            let total = total.unwrap_or(usize::MAX);
            if total > crate::protocol::MAX_XATTR_TOTAL_BYTES {
                return Err(RootedFsError::XattrSetTooLarge {
                    len: total,
                    max: crate::protocol::MAX_XATTR_TOTAL_BYTES,
                });
            }
        }
        if let Some(acl) = self.acl {
            if acl.len() > crate::protocol::MAX_ACL_TEXT_BYTES {
                return Err(RootedFsError::AclSetTooLarge {
                    len: acl.len(),
                    max: crate::protocol::MAX_ACL_TEXT_BYTES,
                });
            }
            #[cfg(all(feature = "acl", any(target_os = "linux", target_os = "macos")))]
            exacl::from_str(acl)?;
            #[cfg(not(all(feature = "acl", any(target_os = "linux", target_os = "macos"))))]
            return Err(RootedFsError::AclUnsupported(
                "ACL preservation requires supported Unix and the acl feature",
            ));
        }
        #[cfg(not(target_os = "macos"))]
        if self.bsd_flags.is_some() {
            return Err(RootedFsError::UnsupportedPlatform);
        }
        Ok(())
    }

    /// Demand-read only requested fields. Unknown ACL read permission cannot
    /// establish an effect-free request, but does not forbid an exclusive SET.
    #[cfg(unix)]
    fn matches(
        &self,
        file: &File,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> Result<bool> {
        let current = stat_fd(file.as_raw_fd())?;
        if unix_mode.is_some_and(|mode| current.st_mode & 0o7777 != (mode & 0o7777) as libc::mode_t)
        {
            return Ok(false);
        }
        if let Some(times) = modified.map(modified_timespecs).transpose()? {
            if current.st_mtime != times[1].tv_sec || current.st_mtime_nsec != times[1].tv_nsec {
                return Ok(false);
            }
        }
        if let Some(xattrs) = self.xattrs {
            use xattr::FileExt;
            for (name, value) in xattrs {
                if !bounded_xattrs::value_matches(file, name, value)? {
                    return Ok(false);
                }
            }
            for name in file.list_xattr()? {
                if !xattrs.iter().any(|(wanted, _)| wanted == &name) {
                    return Ok(false);
                }
            }
        }
        if let Some(acl) = self.acl {
            #[cfg(feature = "acl")]
            if !acl_matches_fd(file, &desired_acl_entries(file, acl)?)? {
                return Ok(false);
            }
            #[cfg(not(feature = "acl"))]
            {
                let _ = acl;
                return Err(RootedFsError::AclUnsupported(
                    "ACL preservation requires the acl feature",
                ));
            }
        }
        #[cfg(target_os = "macos")]
        if self
            .bsd_flags
            .is_some_and(|flags| flags != current.st_flags)
        {
            return Ok(false);
        }
        Ok(true)
    }

    #[cfg(unix)]
    pub(super) fn apply(
        &self,
        file: &File,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> Result<()> {
        if let Some(xattrs) = self.xattrs {
            mirror_xattrs_fd(file, xattrs)?;
        }
        apply_fd_metadata(file.as_raw_fd(), unix_mode, modified)?;
        apply_acl_and_verify_mode(file, self.acl, unix_mode)?;
        // Immutable flags are last: they can prevent preceding required fields.
        if let Some(flags) = self.bsd_flags {
            #[cfg(target_os = "macos")]
            apply_bsd_flags_fd(file, flags)?;
            #[cfg(not(target_os = "macos"))]
            {
                let _ = flags;
                return Err(RootedFsError::UnsupportedPlatform);
            }
        }
        Ok(())
    }
}

/// Apply a validated exact set through the held inode. Required removals are
/// not best-effort: failure may leave earlier effects, but must never report
/// successful preservation while a stale attribute survives.
#[cfg(unix)]
pub(super) fn mirror_xattrs_fd(file: &File, xattrs: &[(OsString, Vec<u8>)]) -> Result<()> {
    use xattr::FileExt;
    for (name, value) in xattrs {
        // Preservation is a state contract, not a requirement to rewrite an
        // identical attribute (which needlessly changes ctime).
        if !bounded_xattrs::value_matches(file, name, value)? {
            file.set_xattr(name, value)?;
        }
    }
    for name in file.list_xattr()? {
        if !xattrs.iter().any(|(wanted, _)| wanted == &name) {
            file.remove_xattr(&name)?;
        }
    }
    Ok(())
}

/// ACL application can change mode bits. A requested mode is an independent
/// contract to verify, not permission to rewrite the requested ACL with chmod.
#[cfg(unix)]
pub(super) fn apply_acl_and_verify_mode(
    file: &File,
    acl: Option<&str>,
    unix_mode: Option<u32>,
) -> Result<()> {
    if let Some(acl) = acl {
        #[cfg(feature = "acl")]
        apply_acl_fd(file, acl)?;
        #[cfg(not(feature = "acl"))]
        {
            let _ = acl;
            return Err(RootedFsError::AclUnsupported(
                "ACL preservation requires the acl feature",
            ));
        }
    }
    if let Some(expected) = unix_mode.map(|mode| mode & 0o7777) {
        use std::os::unix::fs::PermissionsExt;
        let actual = file.metadata()?.permissions().mode() & 0o7777;
        if actual != expected {
            return Err(RootedFsError::PreservationModeConflict { expected, actual });
        }
    }
    Ok(())
}

impl RootedFs {
    /// Validate the scanned destination and apply every requested field through
    /// that single no-follow FD. Shared regular files are refused before the
    /// first field unless the entire request is already fulfilled. This is an
    /// admitted in-place operation, not rollback or compare-and-swap: later
    /// hardlink creation and concurrent inode writers cannot be excluded by a
    /// link-count observation.
    pub(crate) fn apply_observed_preservation_blocking(
        &self,
        relative: &RelativePath,
        kind: EntryKind,
        expected: EntryIdentity,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
        preservation: &MetadataPreservation<'_>,
    ) -> Result<EntryIdentity> {
        #[cfg(unix)]
        {
            self.require_writable()?;
            preservation.validate()?;
            // Validate even requests that turn out to have no effects.
            modified.map(modified_timespecs).transpose()?;
            // Serialize validation and effects with own old-inode retirements.
            // Translation cannot adopt a fresh stat or an unrelated ctime edit.
            let mut lineage = self
                .retirement
                .lock()
                .map_err(|_| std::io::Error::other("retirement authority lock poisoned"))?;
            let expected = lineage.resolve(expected)?;
            if kind == EntryKind::Symlink {
                if preservation.xattrs.is_some() {
                    return Err(RootedFsError::UnsupportedSymlinkXattrs);
                }
                if preservation.acl.is_some() {
                    return Err(RootedFsError::UnsupportedSymlinkAcls);
                }
                if preservation.bsd_flags.is_some() {
                    return Err(RootedFsError::UnsupportedSymlinkBsdFlags);
                }
                if unix_mode.is_some() {
                    return Err(RootedFsError::UnsupportedSymlinkMode);
                }
                let (parent, leaf) = self.open_parent_blocking(relative.as_path())?;
                ensure_symlink_at(parent.as_raw_fd(), &leaf, relative.as_path())?;
                if stat_at_optional(parent.as_raw_fd(), &leaf)?
                    .as_ref()
                    .and_then(identity_from_stat)
                    != Some(expected)
                {
                    return Err(RootedFsError::DestinationChanged(
                        relative.as_path().to_path_buf(),
                    ));
                }
                if let Some(modified) = modified {
                    self.verify_parent_binding_blocking(relative.as_path(), &parent)?;
                    let _permit = self.admit_mutation_blocking()?;
                    // Portable symlink time is a no-follow parent-relative
                    // syscall, not an atomic identity-conditional mutation.
                    set_symlink_mtime_at(parent.as_raw_fd(), &leaf, modified)?;
                }
                let current = stat_at_optional(parent.as_raw_fd(), &leaf)?
                    .as_ref()
                    .and_then(identity_from_stat)
                    .ok_or_else(|| {
                        RootedFsError::DestinationChanged(relative.as_path().to_path_buf())
                    })?;
                self.verify_parent_binding_blocking(relative.as_path(), &parent)?;
                // The no-follow time syscall cannot redirect to a link target;
                // portable Unix still cannot offer identity-conditional mutation.
                if self.path_identity_blocking(relative)? != Some((kind, current)) {
                    return Err(RootedFsError::DestinationChanged(
                        relative.as_path().to_path_buf(),
                    ));
                }
                return Ok(current);
            }
            let file = self.open_xattr_entry_blocking(relative.as_path(), kind)?;
            if identity_from_stat(&stat_fd(file.as_raw_fd())?) != Some(expected) {
                return Err(RootedFsError::DestinationChanged(
                    relative.as_path().to_path_buf(),
                ));
            }
            // Guard the entire request before any write. Advisory ACL read
            // denial means effects are unknown, not that exclusive SET is denied.
            let effect_free = preservation.matches(&file, unix_mode, modified)?;
            if !effect_free {
                require_exclusive_file_metadata(&file, relative.as_path())?;
            }
            self.verify_metadata_binding_blocking(relative.as_path(), &file, kind)?;
            // Keep the session closure guard even for effect-free completion.
            let _permit = self.admit_mutation_blocking()?;
            if effect_free {
                if identity_from_stat(&stat_fd(file.as_raw_fd())?) != Some(expected)
                    || !preservation.matches(&file, unix_mode, modified)?
                {
                    return Err(RootedFsError::DestinationChanged(
                        relative.as_path().to_path_buf(),
                    ));
                }
            } else {
                preservation.apply(&file, unix_mode, modified)?;
            }
            let current = identity_from_stat(&stat_fd(file.as_raw_fd())?).ok_or_else(|| {
                RootedFsError::DestinationChanged(relative.as_path().to_path_buf())
            })?;
            self.verify_metadata_binding_blocking(relative.as_path(), &file, kind)?;
            if (effect_free && current != expected)
                || self.path_identity_blocking(relative)? != Some((kind, current))
            {
                return Err(RootedFsError::DestinationChanged(
                    relative.as_path().to_path_buf(),
                ));
            }
            Ok(current)
        }
        #[cfg(not(unix))]
        {
            let _ = (relative, kind, expected, unix_mode, modified, preservation);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }
}
