//! Opaque LINK creation and publication; targets are never resolved.
use super::*;

impl RootedFs {
    /// Privately stage a symlink and publish through the common namespace
    /// transaction. Empty directories require atomic exchange; nonempty ones
    /// are refused. The resolved parent stays pinned. The target is stored as
    /// opaque native path data and is never resolved by this operation.
    ///
    /// This is a blocking syscall API and must run on a blocking worker.
    pub fn replace_symlink_blocking(
        &self,
        relative: &RelativePath,
        target: &Path,
        expected: ExpectedDestination,
        modified: Option<Timestamp>,
    ) -> Result<()> {
        self.publish_symlink_blocking(relative, target, expected, modified)
            .map(|_| ())
    }

    pub fn publish_symlink_blocking(
        &self,
        relative: &RelativePath,
        target: &Path,
        expected: ExpectedDestination,
        modified: Option<Timestamp>,
    ) -> Result<PublishedEntryProof> {
        #[cfg(unix)]
        {
            let staged =
                self.begin_staged_symlink_blocking(relative, target, expected, modified)?;
            let observed = staged.commit()?;
            let identity = identity_from_stat(&observed.stat).ok_or_else(|| {
                RootedFsError::CommittedFinalizationFailed {
                    path: relative.as_path().to_path_buf(),
                    reason: "published symlink has no identity".into(),
                }
            })?;
            let proof = PublishedEntryProof {
                path: relative.clone(),
                kind: EntryKind::Symlink,
                identity,
            };
            proof.revalidate_blocking(self).map_err(|error| {
                RootedFsError::CommittedFinalizationFailed {
                    path: relative.as_path().to_path_buf(),
                    reason: error.to_string(),
                }
            })?;
            Ok(proof)
        }
        #[cfg(not(unix))]
        {
            let _ = (relative, target, expected, modified);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }

    pub(super) fn begin_staged_symlink_blocking(
        &self,
        relative: &RelativePath,
        target: &Path,
        expected: ExpectedDestination,
        modified: Option<Timestamp>,
    ) -> Result<RootedNamespaceTransaction> {
        #[cfg(unix)]
        {
            let mut namespace = self.begin_namespace_blocking(
                relative.as_path(),
                expected,
                StagingOrigin::Created,
            )?;
            let mut prepare = || -> Result<()> {
                let target = CString::new(target.as_os_str().as_bytes())
                    .map_err(|_| RootedFsError::PathContainsNul)?;
                let name = component_cstring(&namespace.temp_name)?;
                let result = unsafe {
                    // SAFETY: both strings are live and NUL terminated; the held
                    // private directory owns the new leaf, target is not resolved.
                    libc::symlinkat(
                        target.as_ptr(),
                        namespace.staging_dir_fd.as_raw_fd(),
                        name.as_ptr(),
                    )
                };
                if result < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                #[cfg(test)]
                self.pause_mutation(PublicationPausePoint::AfterSymlinkCreation);
                let held =
                    open_symlink_at(namespace.staging_dir_fd.as_raw_fd(), &namespace.temp_name)?;
                // symlinkat does not return a creator FD. Exclude original
                // SOURCE before adopting the opened entry as cleanup ownership.
                self.require_source_effect_blocking(SourceEffect {
                    kind: SourceEffectKind::PrivateLeafMetadata,
                    parent: namespace.staging_dir_fd.as_raw_fd(),
                    name: &namespace.temp_name,
                    object: Some(held.as_raw_fd()),
                    path: relative.as_path(),
                })?;
                let created = stat_fd(held.as_raw_fd())?;
                let mut actual_target = vec![0; target.as_bytes().len() + 1];
                // SAFETY: the held private parent, live component and writable
                // bounded buffer belong to this creation attempt. Native
                // symlinkat already accepted the target's platform-bound size.
                let count = unsafe {
                    libc::readlinkat(
                        namespace.staging_dir_fd.as_raw_fd(),
                        name.as_ptr(),
                        actual_target.as_mut_ptr().cast(),
                        actual_target.len(),
                    )
                };
                if count < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                let count = usize::try_from(count)
                    .map_err(|_| std::io::Error::other("invalid readlink length"))?;
                let after = stat_fd(held.as_raw_fd())?;
                if created.st_nlink != 1
                    || count != target.as_bytes().len()
                    || actual_target[..count] != *target.as_bytes()
                    || !staging::same_observation(&created, &after)
                    || stat_at_optional(namespace.staging_dir_fd.as_raw_fd(), &namespace.temp_name)?
                        .as_ref()
                        .is_none_or(|named| !staging::same_observation(&after, named))
                {
                    return Err(RootedFsError::StagingEntryChanged(
                        relative.as_path().to_path_buf(),
                    ));
                }
                namespace.register_observed_staging(&held, created)?;
                if let Some(modified) = modified {
                    self.require_source_effect_blocking(SourceEffect {
                        kind: SourceEffectKind::PrivateLeafMetadata,
                        parent: namespace.staging_dir_fd.as_raw_fd(),
                        name: &namespace.temp_name,
                        object: Some(held.as_raw_fd()),
                        path: relative.as_path(),
                    })?;
                    #[cfg(test)]
                    self.pause_mutation(PublicationPausePoint::BeforeSymlinkMetadata);
                    namespace.verify_private_cleanup_directory_blocking()?;
                    namespace.observe_staging()?;
                    if stat_fd(held.as_raw_fd())?.st_nlink != 1 {
                        return Err(RootedFsError::StagingEntryChanged(
                            relative.as_path().to_path_buf(),
                        ));
                    }
                    #[cfg(target_os = "macos")]
                    apply_fd_metadata(held.as_raw_fd(), None, Some(modified))?;
                    #[cfg(not(target_os = "macos"))]
                    set_symlink_mtime_at(
                        namespace.staging_dir_fd.as_raw_fd(),
                        &namespace.temp_name,
                        modified,
                    )?;
                }
                Ok(())
            };
            if let Err(error) = prepare() {
                return Err(namespace.abort_after(error));
            }
            Ok(namespace)
        }
        #[cfg(not(unix))]
        {
            let _ = (relative, target, expected, modified);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }
}

#[cfg(unix)]
pub(super) fn open_symlink_at(parent: RawFd, component: &OsStr) -> Result<File> {
    let name = component_cstring(component)?;
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = libc::O_SYMLINK | libc::O_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(RootedFsError::UnsupportedPlatform);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: the parent and NUL-terminated name are live; these native flags
        // hold the symlink inode itself, never its target.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful openat transferred one owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if stat_fd(file.as_raw_fd())?.st_mode & libc::S_IFMT != libc::S_IFLNK {
            return Err(RootedFsError::EntryKindMismatch {
                path: PathBuf::from(component),
                expected: EntryKind::Symlink,
            });
        }
        Ok(file)
    }
}
