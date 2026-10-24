//! Separate private-entry cleanup ownership from its prepared publication seal.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct VerifiedObservation {
    #[cfg(unix)]
    pub(super) stat: libc::stat,
}

#[cfg(unix)]
pub(super) struct OwnedStagingEntry {
    pub(super) file: Arc<File>,
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

impl RootedStagedFile {
    #[cfg(unix)]
    pub(super) fn validate_private_source_effect_blocking(&self) -> Result<()> {
        if self.namespace.rooted.local_source_effects.is_none() {
            return Ok(());
        }
        self.observe_original_file()?;
        self.namespace
            .rooted
            .require_source_effect_blocking(SourceEffect {
                kind: SourceEffectKind::PrivateLeafMetadata,
                parent: self.namespace.staging_dir_fd.as_raw_fd(),
                name: &self.namespace.temp_name,
                object: Some(self.file.as_raw_fd()),
                path: &self.namespace.destination_path,
            })?;
        // Census can outlive the original private-DIR binding while FILE stat
        // remains unchanged. A setter needs the same late namespace proof.
        self.namespace.verify_group_links_blocking()?;
        self.namespace.verify_private_cleanup_directory_blocking()?;
        self.observe_original_file().map(|_| ())
    }
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
        self.register_observed_staging(file, stat_fd(file.as_raw_fd())?)
    }

    /// Preserve a checked creation observation rather than adopting later
    /// link-count changes while retaining the genuine descriptor pin.
    #[cfg(unix)]
    pub(super) fn register_observed_staging(
        &mut self,
        file: &File,
        created: libc::stat,
    ) -> Result<()> {
        // Cached inode numbers authorize cleanup only while a descriptor pins
        // that inode against reuse, including descriptor-exhaustion failures.
        let retained = file.try_clone()?;
        self.staged = Some(OwnedStagingEntry {
            file: Arc::new(retained),
            created,
        });
        self.cleanup_owned = Some(created);
        self.observe_staging()?;
        Ok(())
    }

    /// Move the genuine creation FD into the owner before duplication. A failed
    /// writer-FD clone must not discard the only pin needed for abort cleanup.
    #[cfg(unix)]
    pub(super) fn register_created_staging(&mut self, file: File) -> Result<File> {
        let created = stat_fd(file.as_raw_fd())?;
        self.staged = Some(OwnedStagingEntry {
            file: Arc::new(file),
            created,
        });
        self.cleanup_owned = Some(created);
        self.observe_staging()?;
        Ok(self
            .staged
            .as_ref()
            .ok_or_else(|| self.staging_changed())?
            .file
            .try_clone()?)
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
    pub(super) fn verify_private_cleanup_parent_blocking(&self) -> Result<()> {
        let qualified = matches!(
            self.private_creation,
            local_source_effects::PrivateCreationAdmission::Qualified(_)
        );
        if !qualified {
            // Without a SOURCE scope, retained private ownership survives parent
            // relocation. The private directory's own named binding is still
            // checked before touching its contents.
            return Ok(());
        }
        self.rooted.verify_root_path_blocking()?;
        // SOURCE health is irrelevant, but every original DST ancestor still
        // matters: a foreign SOURCE directory could now enclose the same parent.
        let mut current = self.rooted.root_fd.try_clone()?;
        let mut prefix = PathBuf::new();
        self.rooted
            .verify_namespace_directory(&prefix, current.as_raw_fd())?;
        for component in self
            .destination_path
            .parent()
            .unwrap_or(Path::new(""))
            .components()
        {
            let Component::Normal(name) = component else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            current = open_dir_at(current.as_raw_fd(), name)?;
            prefix.push(name);
            self.rooted
                .verify_namespace_directory(&prefix, current.as_raw_fd())?;
        }
        if !same_inode(
            &stat_fd(current.as_raw_fd())?,
            &stat_fd(self.parent_fd.as_raw_fd())?,
        ) {
            return Err(RootedFsError::DestinationChanged(
                self.destination_path.clone(),
            ));
        }
        self.private_creation
            .verify_parent_blocking(self.parent_fd.as_raw_fd(), &self.destination_path)
    }

    /// A held creator directory alone is insufficient: it may have been moved
    /// into SOURCE. Check its original DST parent/name before touching children.
    #[cfg(unix)]
    pub(super) fn verify_private_cleanup_directory_blocking(&self) -> Result<()> {
        self.verify_private_cleanup_parent_blocking()?;
        let held = stat_fd(self.staging_dir_fd.as_raw_fd())?;
        let named = stat_at_optional(self.parent_fd.as_raw_fd(), &self.staging_dir_name)?;
        if named.as_ref().is_some_and(|named| same_inode(&held, named))
            || (named.is_none() && held.st_nlink == 0)
        {
            Ok(())
        } else {
            Err(RootedFsError::StagingDirectoryChanged)
        }
    }

    #[cfg(unix)]
    pub(super) fn remove_owned_contents(&mut self) -> Result<bool> {
        self.verify_private_cleanup_directory_blocking()?;
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
        let owned = self.staged.as_ref().ok_or_else(|| self.staging_changed())?;
        if same_inode(&owned.created, &named) {
            if self.strict_cleanup || self.committed {
                // Groups and completed publications may share a visible inode.
                // Their full seal includes each proven own link transition.
                self.sealed.ok_or_else(|| self.staging_changed())?;
                self.observe_staging()?;
            }
            let held = stat_fd(owned.file.as_raw_fd())?;
            if !same_inode(&owned.created, &held)
                || !same_observation(&held, &named)
                || (!self.strict_cleanup
                    && !self.committed
                    && held.st_nlink != owned.created.st_nlink)
            {
                // An unexpected outside alias can be in SOURCE; unlink would
                // change its nlink/ctime. Groups instead use their full seal
                // and the exact owned -1 transition in abort_blocking.
                return Err(self.staging_changed());
            }
        } else {
            // Exchange may park OLD here. Creation admission does not authorize
            // its retirement, even when the private container is genuinely ours.
            self.rooted.require_source_effect_blocking(SourceEffect {
                kind: SourceEffectKind::NamespaceRetirement,
                parent: self.staging_dir_fd.as_raw_fd(),
                name: &self.temp_name,
                object: None,
                path: &self.destination_path,
            })?;
        }
        self.verify_group_links_blocking()?;
        if self.group_links.is_some() {
            self.verify_private_cleanup_directory_blocking()?;
            self.observe_staging()?;
        }
        // Mutable private directory children do not invalidate inode ownership.
        // Never recursively delete them: rmdir retains a nonempty artifact.
        unlink_at(
            self.staging_dir_fd.as_raw_fd(),
            &self.temp_name,
            named.st_mode & libc::S_IFMT == libc::S_IFDIR,
        )?;
        if let Some(link) = self.group_private_link.take() {
            if let (group_links::PrivateLink::Anchor, Some(links)) = (link, &self.group_links) {
                links
                    .lock()
                    .map_err(|_| RootedFsError::IncompleteStagedGroup)?
                    .retire_anchor();
            }
        }
        Ok(true)
    }
}
