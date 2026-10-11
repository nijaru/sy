//! One catalogue-selected inode, populated once and published through its owner.
use super::*;
use crate::endpoint::Endpoint;
use crate::engine::domain::SyncOp;
use crate::engine::hardlink_runtime::{GroupError, HardlinkGroupPlan};
use crate::engine::work::{TransferSummary, WorkResult};
use crate::rooted_fs::{PublishedFileProof, RootedFsError};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

impl LocalSyncExecutor {
    pub(super) async fn execute_whole_group(
        &self,
        group: &mut HardlinkGroupPlan<'_>,
        policy: ExecutionPolicy,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                active_files: 1,
                buffered_bytes: LOCAL_FILE_WORKING_SET,
                metadata_ops: 1,
                cpu_tasks: 1,
                network_writes: 0,
            })
            .await?;
        let mut members = group.members().await?;
        let first = members
            .next()
            .await?
            .ok_or(GroupError::Invalid("empty group"))?;
        let source = first.source().clone();
        self.check_source_identity(&source).await?;
        let representative = destination(&first).cloned();
        let mut canonical_source = transfers_bytes(&first);
        let coalesced_identity = representative
            .as_ref()
            .and_then(|entry| entry.is_file().then_some(entry.identity).flatten());
        while let Some(member) = members.next().await? {
            self.check_source_identity(member.source()).await?;
            canonical_source |= transfers_bytes(&member)
                || coalesced_identity.is_none()
                || destination(&member).and_then(|entry| entry.identity) != coalesced_identity;
        }
        let mode = if policy.preserve_permissions
            || representative.is_none()
            || matches!(first, SyncOp::Replace { .. })
        {
            source.unix_mode
        } else {
            representative.as_ref().and_then(|entry| entry.unix_mode)
        }
        .ok_or_else(|| LocalSyncError::MissingScannedMode(self.source_path(&source.path)))?;
        let modified = if policy.preserve_times {
            Some(source.modified)
        } else if matches!(first, SyncOp::Metadata { .. } | SyncOp::Unchanged { .. }) {
            representative.as_ref().map(|entry| entry.modified)
        } else {
            None
        };
        let rooted = self.metadata_authority().await?;
        let source_file =
            if canonical_source || self.xattrs || self.acls || self.remove_source_files {
                let file = if self.follow_symlinks {
                    self.source_endpoint
                        .open_native_file_following(source.path.as_path())
                        .await?
                } else {
                    self.source_endpoint
                        .open_native_file(source.path.as_path())
                        .await?
                }
                .ok_or_else(|| ExistingDestinationError::MissingObservation(source.path.clone()))?;
                Some(Arc::new(file))
            } else {
                None
            };
        let preservation = joined({
            let root = self.source_root.clone();
            let source = source.clone();
            let file = source_file.clone();
            let follow = self.follow_symlinks;
            let request = crate::endpoint::io::PreservationRequest {
                xattrs: self.xattrs,
                acl: self.acls,
            };
            move || capture_source_preservation(&root, &source, file.as_deref(), follow, request)
        })
        .await?;
        let requested_flags = self.read_source_bsd_flags(&source).await?;
        joined({
            let root = self.source_root.clone();
            let source = source.clone();
            let file = source_file.clone();
            let follow = self.follow_symlinks;
            move || Ok(validate_source(&root, &source, file.as_deref(), follow)?)
        })
        .await?;

        if !canonical_source {
            let observed = representative
                .as_ref()
                .ok_or(GroupError::Invalid("retained group has no destination"))?;
            let expected = observed.identity.ok_or_else(|| {
                LocalSyncError::MissingDestinationIdentity(self.destination_path(&observed.path))
            })?;
            let matches = joined({
                let rooted = rooted.clone();
                let relative = observed.path.clone();
                let preservation = preservation.clone();
                move || {
                    rooted.verify_root_path_blocking()?;
                    Ok(rooted.observed_file_preservation_matches_blocking(
                        &relative,
                        expected,
                        Some(mode),
                        modified,
                        &crate::rooted_fs::MetadataPreservation {
                            xattrs: preservation.xattrs.as_deref(),
                            acl: preservation.acl.as_deref(),
                            bsd_flags: requested_flags,
                        },
                    )?)
                }
            })
            .await?;
            if matches {
                // Hash each inode once; group size must not multiply byte reads.
                let fingerprints = if self.remove_source_files {
                    let options = FingerprintOptions {
                        permissions: policy.preserve_permissions,
                        times: policy.preserve_times,
                        ..Default::default()
                    };
                    Some((
                        existing::fingerprint(self.source_root.rooted(), source.clone(), options)
                            .await?,
                        existing::fingerprint(rooted.clone(), observed.clone(), options).await?,
                    ))
                } else {
                    None
                };
                let mut members = group.members().await?;
                while let Some(member) = members.next().await? {
                    self.check_source_identity(member.source()).await?;
                    let old = destination(&member)
                        .ok_or(GroupError::Invalid("no-op member has no destination"))?;
                    let proof = joined({
                        let rooted = rooted.clone();
                        let relative = old.path.clone();
                        let expected = old
                            .identity
                            .ok_or(GroupError::Invalid("missing no-op identity"))?;
                        move || {
                            rooted.verify_root_path_blocking()?;
                            rooted.verify_retired_destination_blocking(
                                &relative,
                                EntryKind::File,
                                expected,
                            )?;
                            rooted.verify_root_path_blocking()?;
                            Ok(PublishedFileProof {
                                path: relative,
                                identity: expected,
                            })
                        }
                    })
                    .await?;
                    group.confirm_member(&member, &proof).await?;
                    if let Some((source_fingerprint, destination_fingerprint)) = fingerprints {
                        self.defer_verified_noop(
                            group.key(),
                            member.source(),
                            old,
                            source_fingerprint,
                            destination_fingerprint,
                        )
                        .await?;
                    }
                }
                joined({
                    let rooted = rooted.clone();
                    move || Ok(rooted.verify_root_path_blocking()?)
                })
                .await?;
                return Ok(());
            }
        }

        let retained = matches!(first, SyncOp::Metadata { .. } | SyncOp::Unchanged { .. });
        let first_path = first.path().clone();
        let expected = expected_destination(&first)?;
        let rate_limiter = self.rate_limiter.clone();
        let source_root = self.source_root.clone();
        let follow = self.follow_symlinks;
        let (mut owner, digest, parity, flags, copied_bytes) = joined({
            let rooted = rooted.clone();
            let first_path = first_path.clone();
            let representative = representative.clone();
            let source = source.clone();
            let source_file = source_file.clone();
            move || {
                validate_source(&source_root, &source, source_file.as_deref(), follow)?;
                let old = if retained || !canonical_source {
                    let old = representative
                        .as_ref()
                        .ok_or(GroupError::Invalid("missing retained representative"))?;
                    rooted.verify_retired_destination_blocking(
                        &old.path,
                        EntryKind::File,
                        old.identity
                            .ok_or(GroupError::Invalid("missing retained identity"))?,
                    )?;
                    let file = rooted.open_regular_blocking(&old.path)?;
                    rooted.observed_regular_metadata_blocking(
                        &file,
                        &old.path,
                        old.identity
                            .ok_or(GroupError::Invalid("missing retained identity"))?,
                    )?;
                    Some(file)
                } else {
                    None
                };
                let mut staged =
                    rooted.begin_staged_file_with_expectation_blocking(&first_path, expected)?;
                if retained {
                    staged.copy_retained_metadata_blocking(
                        old.as_ref().ok_or(GroupError::Invalid("missing old FD"))?,
                    )?;
                }
                let input = if canonical_source {
                    source_file.as_deref().ok_or(GroupError::Invalid(
                        "canonical bytes require original source FD",
                    ))?
                } else {
                    old.as_ref().ok_or(GroupError::Invalid(
                        "retained bytes require original destination FD",
                    ))?
                };
                let mut input = input.try_clone()?;
                input.seek(SeekFrom::Start(0))?;
                let size = input.metadata()?.len();
                let digest = existing::hash_observed_bytes(&mut input, size, &source.path)?;
                input.seek(SeekFrom::Start(0))?;
                let copied = copy(&mut input, staged.file_mut(), rate_limiter.as_ref())?;
                if staged.file_mut().metadata()?.len() != size {
                    return Err(RootedFsError::CopySourceChanged(
                        source.path.as_path().to_path_buf(),
                    )
                    .into());
                }
                staged.apply_preservation_blocking(
                    Some(mode),
                    modified,
                    preservation.xattrs.as_deref(),
                    preservation.acl.as_deref(),
                )?;
                if staged.staged_hash_blocking()?.as_bytes() != &digest {
                    return Err(LocalSyncError::VerificationFailed {
                        path: rooted.root_path().join(first_path.as_path()),
                        expected: blake3::Hash::from_bytes(digest).to_string(),
                        actual: "staged bytes differ".into(),
                    });
                }
                validate_source(&source_root, &source, source_file.as_deref(), follow)?;
                if let (Some(old_entry), Some(old)) = (&representative, &old) {
                    rooted.observed_regular_metadata_blocking(
                        old,
                        &old_entry.path,
                        old_entry
                            .identity
                            .ok_or(GroupError::Invalid("missing old identity"))?,
                    )?;
                    rooted.verify_retired_destination_blocking(
                        &old_entry.path,
                        EntryKind::File,
                        old_entry
                            .identity
                            .ok_or(GroupError::Invalid("missing old identity"))?,
                    )?;
                }
                let parity = if canonical_source {
                    true
                } else if let Some(file) = &source_file {
                    let mut file = file.try_clone()?;
                    file.seek(SeekFrom::Start(0))?;
                    existing::hash_observed_bytes(&mut file, source.size, &source.path)? == digest
                } else {
                    false
                };
                validate_source(&source_root, &source, source_file.as_deref(), follow)?;
                let flags = match requested_flags {
                    Some(flags) => Some(flags),
                    None => match &old {
                        Some(old) => native_flags(old)?,
                        None => None,
                    },
                };
                Ok((staged.into_group_blocking()?, digest, parity, flags, copied))
            }
        })
        .await?;
        let admission = Arc::new(crate::endpoint::publication::PublicationAdmission::default());
        let _close_admission = CloseAdmission(Arc::clone(&admission));
        let mut members = group.members().await?;
        let mut first_copy = true;
        let mut published_count = 0_u64;
        while let Some(member) = members.next().await? {
            self.check_source_identity(member.source()).await?;
            if let Some(old) = destination(&member).filter(|entry| self.backup && entry.is_file()) {
                self.backup_replacement_file(&old.path, old.identity)
                    .await?;
            }
            let relative = member.path().clone();
            let expected = expected_destination(&member)?;
            owner = joined({
                let source_root = self.source_root.clone();
                let source = member.source().clone();
                let source_file = source_file.clone();
                let admission = Arc::clone(&admission);
                let rooted = rooted.clone();
                move || {
                    rooted.verify_root_path_blocking()?;
                    if let Err(operation) =
                        owner.publish_member_blocking(&relative, expected, &admission, || {
                            rooted.verify_root_path_blocking()?;
                            validate_source(&source_root, &source, source_file.as_deref(), follow)
                        })
                    {
                        return match owner.abort_blocking() {
                            Ok(()) => Err(operation.into()),
                            Err(abort) => Err(RootedFsError::StagingOperationAbortFailed {
                                operation: operation.to_string(),
                                abort: abort.to_string(),
                            }
                            .into()),
                        };
                    }
                    crate::endpoint::local::verify_committed_root(
                        &rooted,
                        &rooted.root_path().join(relative.as_path()),
                    )?;
                    Ok(owner)
                }
            })
            .await?;
            self.check_source_identity(member.source()).await?;
            if canonical_source {
                let literal_bytes = if first_copy { copied_bytes } else { 0 };
                group.record_work(
                    &member,
                    WorkResult::Transfer(TransferSummary {
                        file_size: source.size,
                        digest,
                        literal_bytes,
                        reused_bytes: if first_copy { 0 } else { source.size },
                    }),
                )?;
                self.report_transfer(
                    if destination(&member).is_some() {
                        crate::sync::output::ItemizeOp::Update
                    } else {
                        crate::sync::output::ItemizeOp::Create
                    },
                    member.path(),
                    source.size,
                    literal_bytes,
                );
            } else {
                group.record_work(&member, WorkResult::Metadata)?;
                self.report(
                    crate::sync::output::ItemizeOp::Update,
                    crate::sync::output::ItemizeKind::File,
                    member.path(),
                );
            }
            first_copy = false;
            published_count = published_count
                .checked_add(1)
                .ok_or(GroupError::Invalid("publication count overflow"))?;
        }
        let finalized = joined({
            let rooted = rooted.clone();
            move || {
                rooted.verify_root_path_blocking()?;
                let finalized = owner.finish_blocking(first_path.clone(), flags, admission)?;
                crate::endpoint::local::verify_committed_root(
                    &rooted,
                    &rooted.root_path().join(first_path.as_path()),
                )?;
                Ok(finalized)
            }
        })
        .await?;
        if finalized.member_count() != published_count {
            return Err(GroupError::Invalid("native publication count disagrees").into());
        }
        let mut members = group.members().await?;
        let finalized = Arc::new(finalized);
        let mut publication = None;
        while let Some(member) = members.next().await? {
            self.check_source_identity(member.source()).await?;
            let proof = joined({
                let finalized = Arc::clone(&finalized);
                let relative = member.path().clone();
                let rooted = rooted.clone();
                move || {
                    crate::endpoint::local::verify_committed_root(
                        &rooted,
                        &rooted.root_path().join(relative.as_path()),
                    )?;
                    let proof = finalized.proof_for_member_blocking(&relative)?;
                    crate::endpoint::local::verify_committed_root(
                        &rooted,
                        &rooted.root_path().join(relative.as_path()),
                    )?;
                    Ok(proof)
                }
            })
            .await?;
            group.confirm_member(&member, &proof).await?;
            publication.get_or_insert(proof.clone());
            if parity && self.remove_source_files {
                let receipt = PublishedDestinationReceipt::for_file(
                    member.source().path.clone(),
                    member.path().clone(),
                    member.source().identity,
                    &crate::endpoint::io::VerificationStatus::Verified,
                    proof,
                );
                self.defer_grouped_source_removal(group.key(), &receipt, member.source())
                    .await?;
            }
        }
        joined({
            let rooted = rooted.clone();
            let path = source.path.clone();
            move || {
                Ok(crate::endpoint::local::verify_committed_root(
                    &rooted,
                    &rooted.root_path().join(path.as_path()),
                )?)
            }
        })
        .await?;
        if self.remove_source_files && parity {
            let proof = publication.ok_or(GroupError::Invalid("group has no final publication"))?;
            self.hardlink_groups
                .lock()
                .await
                .insert(
                    group.key(),
                    HardlinkRepresentative {
                        path: proof.path,
                        publication: proof.identity,
                        unix_mode: Some(mode),
                        modified,
                    },
                )
                .await?;
        }
        Ok(())
    }

    async fn defer_verified_noop(
        &self,
        group: [u8; 32],
        source: &Entry,
        destination: &Entry,
        mut source_proof: existing::ExistingFingerprint,
        mut destination_proof: existing::ExistingFingerprint,
    ) -> Result<()> {
        let (source_binding, destination_binding) = joined({
            let source_root = self.source_root.rooted();
            let destination_root = self.metadata_authority().await?;
            let source = source.clone();
            let destination = destination.clone();
            move || {
                existing::validate_path(&source_root, &source)?;
                destination_root.verify_retired_destination_blocking(
                    &destination.path,
                    EntryKind::File,
                    destination
                        .identity
                        .ok_or(GroupError::Invalid("missing no-op identity"))?,
                )?;
                Ok((
                    source_root.entry_binding_blocking(&source.path)?,
                    destination_root.entry_binding_blocking(&destination.path)?,
                ))
            }
        })
        .await?;
        source_proof.binding = source_binding;
        destination_proof.binding = destination_binding;
        match existing::receipt(source, destination, source_proof, destination_proof) {
            Ok(receipt) => {
                self.hardlink_removals
                    .append_existing(group, source, &receipt)
                    .await?;
                Ok(())
            }
            Err(ExistingDestinationError::ContentMismatch(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

struct CloseAdmission(Arc<crate::endpoint::publication::PublicationAdmission>);

impl Drop for CloseAdmission {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn destination(operation: &SyncOp) -> Option<&Entry> {
    match operation {
        SyncOp::Create { .. } | SyncOp::Skip { .. } => None,
        SyncOp::Update { destination, .. }
        | SyncOp::Replace { destination, .. }
        | SyncOp::Metadata { destination, .. }
        | SyncOp::Unchanged { destination, .. } => Some(destination),
    }
}

fn transfers_bytes(operation: &SyncOp) -> bool {
    matches!(
        operation,
        SyncOp::Create { .. } | SyncOp::Update { .. } | SyncOp::Replace { .. }
    )
}

fn expected_destination(operation: &SyncOp) -> Result<crate::endpoint::ExpectedDestination> {
    Ok(match destination(operation) {
        Some(destination) => crate::endpoint::ExpectedDestination::Unchanged(
            destination.identity.ok_or_else(|| {
                LocalSyncError::MissingDestinationIdentity(destination.path.as_path().to_path_buf())
            })?,
        ),
        None => crate::endpoint::ExpectedDestination::Absent,
    })
}

fn capture_source_preservation(
    root: &crate::endpoint::source_root::SourceRoot,
    source: &Entry,
    file: Option<&File>,
    follow: bool,
    request: crate::endpoint::io::PreservationRequest,
) -> Result<crate::endpoint::io::Preservation> {
    validate_source(root, source, file, follow)?;
    let preservation = match file {
        Some(file) => {
            let rooted = root.rooted();
            crate::endpoint::io::Preservation {
                xattrs: request
                    .xattrs
                    .then(|| rooted.read_open_file_xattrs_blocking(file, &source.path))
                    .transpose()?,
                acl: request
                    .acl
                    .then(|| rooted.read_open_file_acl_blocking(file, &source.path))
                    .transpose()?
                    .map(|acl| acl.unwrap_or_default()),
            }
        }
        None => crate::endpoint::io::Preservation::default(),
    };
    validate_source(root, source, file, follow)?;
    Ok(preservation)
}

pub(super) fn validate_source(
    root: &crate::endpoint::source_root::SourceRoot,
    source: &Entry,
    file: Option<&File>,
    follow: bool,
) -> crate::rooted_fs::Result<()> {
    root.validate_blocking()?;
    let named = if follow {
        crate::endpoint::local_identity::metadata_identity(
            &std::fs::metadata(root.path().join(source.path.as_path()))?,
            EntryKind::File,
        )
    } else {
        root.rooted()
            .path_identity_blocking(&source.path)?
            .and_then(|(kind, identity)| (kind == EntryKind::File).then_some(identity))
    };
    if source.identity.is_none() || named != source.identity {
        return Err(RootedFsError::CopySourceChanged(
            source.path.as_path().to_path_buf(),
        ));
    }
    if let Some(file) = file {
        if crate::endpoint::local_identity::metadata_identity(&file.metadata()?, EntryKind::File)
            != source.identity
        {
            return Err(RootedFsError::CopySourceChanged(
                source.path.as_path().to_path_buf(),
            ));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn source_preservation_lease_rejects_wrong_fd_even_after_name_restoration() {
    use futures::TryStreamExt;
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"same").unwrap();
    std::fs::write(foreign.path().join("file"), b"same").unwrap();
    let authority = crate::endpoint::source_root::SourceRoot::open(root.path().to_path_buf())
        .await
        .unwrap();
    let source = authority
        .entries(Default::default())
        .try_next()
        .await
        .unwrap()
        .unwrap();
    let wrong = File::open(foreign.path().join("file")).unwrap();
    // The operator name is correct again, but the transiently opened descriptor
    // must not supply a different inode's preservation to a no-op receipt.
    assert!(matches!(
        capture_source_preservation(
            &authority,
            &source,
            Some(&wrong),
            false,
            crate::endpoint::io::PreservationRequest {
                xattrs: true,
                acl: false
            }
        ),
        Err(LocalSyncError::Rooted(RootedFsError::CopySourceChanged(_)))
    ));
    assert!(
        authority
            .rooted()
            .observed_regular_metadata_blocking(&wrong, &source.path, source.identity.unwrap(),)
            .is_err(),
        "retained destination metadata/bytes require the original observed FD too"
    );
    let original = authority
        .rooted()
        .open_regular_blocking(&source.path)
        .unwrap();
    capture_source_preservation(
        &authority,
        &source,
        Some(&original),
        false,
        crate::endpoint::io::PreservationRequest {
            xattrs: true,
            acl: false,
        },
    )
    .unwrap();
}

fn native_flags(file: &File) -> Result<Option<u32>> {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        Ok(Some(file.metadata()?.st_flags()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file;
        Ok(None)
    }
}

async fn joined<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RootedFsError::Worker(error.to_string()))?
}

fn copy(
    source: &mut File,
    destination: &mut File,
    limiter: Option<&Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
) -> Result<u64> {
    if let Some(limiter) = limiter {
        let mut bytes = 0_u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                return Ok(bytes);
            }
            let delay = limiter
                .lock()
                .map_err(|_| std::io::Error::other("rate limiter poisoned"))?
                .consume(u64::try_from(count).map_err(std::io::Error::other)?);
            std::thread::sleep(delay);
            destination.write_all(&buffer[..count])?;
            bytes = bytes
                .checked_add(u64::try_from(count).map_err(std::io::Error::other)?)
                .ok_or_else(|| std::io::Error::other("copy byte count overflow"))?;
        }
    }
    #[cfg(unix)]
    let sparse = crate::endpoint::local::copy_sparse_native_file_blocking(source, destination)?;
    #[cfg(not(unix))]
    let sparse: Option<u64> = None;
    Ok(match sparse {
        Some(bytes) => bytes,
        None => crate::endpoint::local::copy_native_file_blocking(source, destination)?,
    })
}
