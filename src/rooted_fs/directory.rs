//! Observation-bound directory metadata, applied only in journal finalization.
use super::*;

#[derive(Debug, Clone, Default)]
pub struct DirectoryPreservation {
    pub xattrs: Option<Vec<(OsString, Vec<u8>)>>,
    pub acl: Option<String>,
    pub bsd_flags: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DirectoryPreservationRequest {
    pub xattrs: bool,
    pub acl: bool,
    pub bsd_flags: bool,
}

impl RootedFs {
    #[cfg(unix)]
    fn observed_directory(&self, path: &RelativePath, expected: EntryIdentity) -> Result<File> {
        self.validate_source_observation_blocking(path)?;
        let file = self.open_xattr_entry_blocking(path.as_path(), EntryKind::Directory)?;
        if identity_from_stat(&stat_fd(file.as_raw_fd())?) != Some(expected)
            || self.path_identity_blocking(path)? != Some((EntryKind::Directory, expected))
        {
            return Err(RootedFsError::DestinationChanged(
                path.as_path().to_path_buf(),
            ));
        }
        self.validate_source_reader_blocking(path, &file)?;
        Ok(file)
    }

    /// Every requested field comes from the same observed no-follow directory FD.
    pub fn read_directory_preservation_blocking(
        &self,
        path: &RelativePath,
        expected: EntryIdentity,
        request: DirectoryPreservationRequest,
    ) -> Result<DirectoryPreservation> {
        #[cfg(unix)]
        {
            let file = self.observed_directory(path, expected)?;
            let xattrs = request
                .xattrs
                .then(|| read_xattrs_from_file(&file))
                .transpose()?;
            let acl = if request.acl {
                Some(self.read_acl_from_file(&file)?.unwrap_or_default())
            } else {
                None
            };
            let bsd_flags = if request.bsd_flags {
                #[cfg(target_os = "macos")]
                {
                    use std::os::macos::fs::MetadataExt;
                    Some(file.metadata()?.st_flags())
                }
                #[cfg(not(target_os = "macos"))]
                return Err(RootedFsError::UnsupportedPlatform);
            } else {
                None
            };
            // Directory identity excludes times: our own descendant writes are
            // allowed, but replacing the observed inode is not.
            self.observed_directory(path, expected)?;
            Ok(DirectoryPreservation {
                xattrs,
                acl,
                bsd_flags,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (path, expected, request);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }

    /// In-place canonical finalization, not rollback-capable replacement.
    /// Admission covers this multi-field operation, not atomicity: after it
    /// starts, cancellation cannot interrupt it and failures may leave some
    /// fields applied. Directory link counts are not regular-file sharing.
    /// Mode precedes ACL application; conflicting effective modes fail rather
    /// than silently changing an ACL mask. Immutable flags are last, after
    /// children, deletion and every other required field.
    pub fn finalize_directory_blocking(
        &self,
        path: &RelativePath,
        expected: EntryIdentity,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
        preservation: &DirectoryPreservation,
    ) -> Result<()> {
        #[cfg(unix)]
        {
            self.require_writable()?;
            let preservation = MetadataPreservation {
                xattrs: preservation.xattrs.as_deref(),
                acl: preservation.acl.as_deref(),
                bsd_flags: preservation.bsd_flags,
            };
            preservation.validate()?;
            let file = self.observed_directory(path, expected)?;
            if preservation.basic_only() && unix_mode.is_none() && modified.is_none() {
                // Empty basic finalization is observation work, not permission
                // to mutate a directory or demand a full SOURCE data census.
                let _permit = self.admit_validated_observation_blocking(None)?;
                return Ok(());
            }
            let timestamp_only = preservation.basic_only() && unix_mode.is_none();
            let (parent, leaf) = self.open_parent_blocking(path.as_path())?;
            let _permit = self.admit_mutation_blocking(SourceEffect {
                kind: if timestamp_only {
                    SourceEffectKind::DirectoryTimestamp
                } else {
                    SourceEffectKind::DirectoryPolicy
                },
                parent: parent.as_raw_fd(),
                name: &leaf,
                object: Some(file.as_raw_fd()),
                path: path.as_path(),
            })?;
            if self.local_source_effects.is_some() {
                // SOURCE census is not a lease on the earlier DST opening.
                // Recheck original ROOT/ancestors/name after its final barrier,
                // without adopting a replacement inode as this setter's target.
                let current = self.observed_directory(path, expected)?;
                if !staging::same_inode(&stat_fd(file.as_raw_fd())?, &stat_fd(current.as_raw_fd())?)
                {
                    return Err(RootedFsError::DestinationChanged(
                        path.as_path().to_path_buf(),
                    ));
                }
            }
            if timestamp_only {
                // The native footprint is deliberately limited to the FD's
                // mtime/ctime; UTIME_OMIT preserves atime. Do not route this
                // admission through a broader preservation setter.
                apply_fd_metadata(file.as_raw_fd(), None, modified)?;
            } else {
                preservation.apply(&file, unix_mode, modified)?;
            }
            // chmod/ACL may change the mode included in directory tokens.
            // Revalidate the visible name against this held inode's new token,
            // never adopt the identity of a fresh pathname lookup.
            let current = identity_from_stat(&stat_fd(file.as_raw_fd())?)
                .ok_or_else(|| RootedFsError::DestinationChanged(path.as_path().to_path_buf()))?;
            if self.path_identity_blocking(path)? != Some((EntryKind::Directory, current)) {
                return Err(RootedFsError::DestinationChanged(
                    path.as_path().to_path_buf(),
                ));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (path, expected, unix_mode, modified, preservation);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }
}
