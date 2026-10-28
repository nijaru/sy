//! One original creator FD and protected namespace anchor own a staged inode
//! across bounded, sequential hardlink publication. Receipts are not aggregate
//! completion or source-removal authority; the caller owns exact membership.
use super::*;

#[derive(Debug)]
pub(crate) struct RootedStagedGroup {
    anchor: RootedStagedFile,
    progress: GroupProgress,
}

#[derive(Debug)]
enum GroupProgress {
    Active { members: u64 },
    Failed,
}

/// A member became visible, but later links/anchor retirement still change its
/// identity. This receipt does not certify group completion or preservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedGroupMember {
    pub(crate) path: RelativePath,
    pub(crate) identity: EntryIdentity,
}

/// Retains the original creator FD and final observation for the caller's
/// disk-backed all-member binding pass, without retaining member paths/FDs.
#[derive(Debug)]
pub(crate) struct RootedFinalizedGroup {
    published: RootedPublishedFile,
    members: u64,
}

impl RootedStagedFile {
    pub(crate) fn into_group_blocking(mut self) -> Result<RootedStagedGroup> {
        #[cfg(unix)]
        {
            self.namespace.strict_cleanup = true;
            let observed = self.observe_original_file()?;
            if observed.stat.st_mode & libc::S_IFMT != libc::S_IFREG || observed.stat.st_nlink != 1
            {
                return Err(RootedFsError::StagingEntryChanged(
                    self.namespace.destination_path.clone(),
                ));
            }
            self.namespace.sealed.get_or_insert(observed);
            if self.namespace.rooted.local_source_effects.is_some() {
                self.namespace.group_links = Some(Arc::new(std::sync::Mutex::new(
                    group_links::GroupLinks::new(&self.namespace)?,
                )));
                self.namespace.group_private_link = Some(group_links::PrivateLink::Anchor);
            }
            Ok(RootedStagedGroup {
                anchor: self,
                progress: GroupProgress::Active { members: 0 },
            })
        }
        #[cfg(not(unix))]
        {
            Err(RootedFsError::UnsupportedPlatform)
        }
    }
}

impl RootedStagedGroup {
    pub(crate) fn publish_member_blocking(
        &mut self,
        relative: &RelativePath,
        expected: ExpectedDestination,
        admission: &Arc<PublicationAdmission>,
        validate_source: impl Fn() -> Result<()>,
    ) -> Result<PublishedGroupMember> {
        if matches!(self.progress, GroupProgress::Failed) {
            return Err(RootedFsError::IncompleteStagedGroup);
        }
        let committed_members = match self.progress {
            GroupProgress::Active { members } => members,
            GroupProgress::Failed => return Err(RootedFsError::IncompleteStagedGroup),
        };
        match self.publish_member(relative, expected, admission, validate_source) {
            Ok(receipt) => Ok(receipt),
            Err(source) => {
                // No whole-group rollback. Report previously completed native
                // publications explicitly; failed groups never issue final proofs.
                self.progress = GroupProgress::Failed;
                if committed_members == 0 {
                    Err(source)
                } else {
                    Err(RootedFsError::PartialGroupPublication {
                        path: relative.as_path().to_path_buf(),
                        committed_members,
                        source: Box::new(source),
                    })
                }
            }
        }
    }

    #[cfg(unix)]
    fn publish_member(
        &mut self,
        relative: &RelativePath,
        expected: ExpectedDestination,
        admission: &Arc<PublicationAdmission>,
        validate_source: impl Fn() -> Result<()>,
    ) -> Result<PublishedGroupMember> {
        let members = match self.progress {
            GroupProgress::Active { members } => members
                .checked_add(1)
                .ok_or(RootedFsError::StagedGroupCountExhausted)?,
            GroupProgress::Failed => return Err(RootedFsError::IncompleteStagedGroup),
        };
        self.anchor.observe_original_file()?;
        let mut alias = self.anchor.namespace.rooted.begin_namespace_blocking(
            relative.as_path(),
            expected,
            StagingOrigin::Created,
        )?;
        alias.strict_cleanup = true;
        alias.group_links = self.anchor.namespace.group_links.clone();
        // Pin cleanup ownership before linkat, including clone failure. The
        // original creator FD, not an opened visible member, identifies alias.
        let observed = self.anchor.observe_original_file()?;
        if stat_at_optional(alias.parent_fd.as_raw_fd(), &alias.destination_name)?
            .as_ref()
            .is_some_and(|named| staging::same_inode(&observed.stat, named))
        {
            // Exact selected membership belongs to the caller. Never count an
            // already-published binding twice or rely on same-inode rename
            // consuming a private alias (POSIX may leave both names intact).
            return Err(RootedFsError::DestinationChanged(
                relative.as_path().to_path_buf(),
            ));
        }
        alias.staged = Some(OwnedStagingEntry {
            file: Arc::clone(
                &self
                    .anchor
                    .namespace
                    .staged
                    .as_ref()
                    .ok_or(RootedFsError::IncompleteStagedGroup)?
                    .file,
            ),
            created: observed.stat,
            origin: staging::StagingObjectOrigin::Borrowed,
        });
        alias.cleanup_owned = Some(observed.stat);
        alias.sealed = Some(observed);
        let prepare = (|| {
            let _permits = alias.rooted.admit_mutation_with_transaction_blocking(
                Some(admission),
                SourceEffect {
                    kind: SourceEffectKind::LinkCountChange,
                    parent: self.anchor.namespace.staging_dir_fd.as_raw_fd(),
                    name: &self.anchor.namespace.temp_name,
                    object: Some(self.anchor.file.as_raw_fd()),
                    path: relative.as_path(),
                },
            )?;
            validate_source()?;
            // SOURCE census may be long. Recheck both original private DIR
            // bindings, destination ancestry/OLD and the creator seal after it;
            // a held contents FD alone cannot detect private-DIR relocation.
            self.anchor.namespace.verify_group_links_blocking()?;
            alias.verify_parent_binding()?;
            alias.verify_expected_destination()?;
            alias.verify_private_cleanup_directory_blocking()?;
            self.anchor
                .namespace
                .verify_private_cleanup_directory_blocking()?;
            self.anchor.observe_original_file()?;
            // Always link from the ORIGINAL protected staging namespace.
            // Cross-submount EXDEV is a safe failure, never a copy fallback.
            link_at(
                self.anchor.namespace.staging_dir_fd.as_raw_fd(),
                &self.anchor.namespace.temp_name,
                alias.staging_dir_fd.as_raw_fd(),
                &alias.temp_name,
            )?;
            if alias.group_links.is_some() {
                alias.group_private_link = Some(group_links::PrivateLink::Alias);
            }
            // Only this exact successful syscall can advance nlink/ctime.
            self.anchor.namespace.advance_staging(1)?;
            alias.sealed = self.anchor.namespace.sealed;
            self.anchor.observe_original_file()?;
            alias.observe_staging()?;
            Ok(())
        })();
        let result = match prepare {
            Ok(()) => alias.commit_prepared_in_place(Some(admission), || {
                validate_source()?;
                self.anchor.observe_original_file()?;
                Ok(())
            }),
            Err(operation) => Err(alias.abort_after(operation)),
        };
        // Abort has already reported retained artifacts. Do not let Drop
        // retry an unlink after we propagate the seal: such a later own delta
        // would be invisible to the live group owner.
        alias.committed = true;
        // A retained populated alias may have moved into SOURCE without any
        // FILE stat change. Exact nlink alone cannot authorize anchor retirement.
        // An empty pre-link alias or a proven own unlink does not revoke it.
        if alias.group_private_link.is_some() {
            if let Some(links) = &alias.group_links {
                links
                    .lock()
                    .map_err(|_| RootedFsError::IncompleteStagedGroup)?
                    .retain_uncertain_alias();
            }
        }
        // Both native publication and owned abort cleanup advance the alias's
        // seal through exact deltas. Propagate that evidence, never fresh stat.
        self.anchor.namespace.sealed = alias.sealed;
        let observation = result?;
        self.anchor.observe_original_file().map_err(|error| {
            RootedFsError::CommittedFinalizationFailed {
                path: relative.as_path().to_path_buf(),
                reason: error.to_string(),
            }
        })?;
        let identity = publication::verify_file_binding(
            &self.anchor.file,
            &alias.rooted,
            relative,
            &observation.stat,
        )
        .map_err(|error| RootedFsError::CommittedFinalizationFailed {
            path: relative.as_path().to_path_buf(),
            reason: error.to_string(),
        })?;
        self.progress = GroupProgress::Active { members };
        Ok(PublishedGroupMember {
            path: relative.clone(),
            identity,
        })
    }

    #[cfg(not(unix))]
    fn publish_member(
        &mut self,
        _relative: &RelativePath,
        _expected: ExpectedDestination,
        _admission: &Arc<PublicationAdmission>,
        _validate_source: impl Fn() -> Result<()>,
    ) -> Result<PublishedGroupMember> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    pub(crate) fn finish_blocking(
        mut self,
        representative: RelativePath,
        flags: Option<u32>,
        admission: Arc<PublicationAdmission>,
    ) -> Result<RootedFinalizedGroup> {
        let members = match self.progress {
            GroupProgress::Active { members } if members > 0 => members,
            _ => return Err(RootedFsError::IncompleteStagedGroup),
        };
        #[cfg(unix)]
        {
            let finalize = (|| {
                let rooted = self.anchor.namespace.rooted.clone();
                let _permits = rooted.admit_mutation_with_transaction_blocking(
                    Some(&admission),
                    SourceEffect {
                        kind: SourceEffectKind::LinkCountChange,
                        parent: self.anchor.namespace.staging_dir_fd.as_raw_fd(),
                        name: &self.anchor.namespace.temp_name,
                        object: Some(self.anchor.file.as_raw_fd()),
                        path: representative.as_path(),
                    },
                )?;
                let observed = self.anchor.observe_original_file()?;
                // Native nlink widths differ across Unix; compare losslessly.
                if Some(u128::from(observed.stat.st_nlink))
                    != members.checked_add(1).map(u128::from)
                {
                    return Err(RootedFsError::StagingEntryChanged(
                        self.anchor.namespace.destination_path.clone(),
                    ));
                }
                publication::verify_file_binding(
                    &self.anchor.file,
                    &rooted,
                    &representative,
                    &observed.stat,
                )?;
                // Retirement must precede immutable/append flags. abort uses
                // strict original-FD/name validation and records its own -1.
                self.anchor.namespace.abort_blocking()?;
                self.anchor.namespace.sealed.ok_or_else(|| {
                    RootedFsError::StagingEntryChanged(
                        self.anchor.namespace.destination_path.clone(),
                    )
                })
            })()
            .map_err(|error| RootedFsError::CommittedFinalizationFailed {
                path: representative.as_path().to_path_buf(),
                reason: error.to_string(),
            })?;
            let RootedStagedFile { file, namespace } = self.anchor;
            let mut published = RootedPublishedFile::from_committed(
                file,
                namespace.rooted.clone(),
                representative,
                Some(admission),
                finalize,
            )?;
            published.group_links = namespace.group_links.clone();
            let published = published.finalize_owned_blocking(flags)?;
            Ok(RootedFinalizedGroup { published, members })
        }
        #[cfg(not(unix))]
        {
            let _ = (representative, flags, admission);
            Err(RootedFsError::UnsupportedPlatform)
        }
    }

    /// Cleanup only the retained private anchor; visible members are never
    /// rolled back. Foreign substitutions/metadata cause artifact retention.
    pub(crate) fn abort_blocking(self) -> Result<()> {
        self.anchor.abort()
    }
}

impl RootedFinalizedGroup {
    pub(crate) fn member_count(&self) -> u64 {
        self.members
    }

    pub(crate) fn proof_for_member_blocking(
        &self,
        relative: &RelativePath,
    ) -> Result<PublishedFileProof> {
        self.published.proof_for_member_blocking(relative)
    }
}
