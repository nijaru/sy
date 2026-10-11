//! Pull's source lease and original local creator own one sealed group.
use super::*;
use crate::endpoint::io::StagedWriter;
use crate::endpoint::local::LocalStagedWriter;
use crate::engine::hardlink_runtime::intent::{destination, GroupIntent};
use crate::engine::hardlink_runtime::{GroupError, HardlinkGroupPlan};
use crate::engine::planner::ExecutionPolicy;
use crate::engine::work::{TransferSummary, WorkResult};
use crate::protocol::{
    WireEntryKind, WireFetchCompression, WireFileBasis, WireFileBegin, WireGroupMember,
    WireGroupOpen, WireGroupRole,
};
use crate::remote::group::source_client::SourceLease;
use crate::rooted_fs::{PublishedFileProof, RootedFs, RootedFsError};
use std::fs::File;
use std::io::{Seek, SeekFrom};

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct PullGroupError(PullGroupCause);

#[derive(Debug, thiserror::Error)]
enum PullGroupCause {
    #[error(transparent)]
    Source(#[from] crate::remote::group::GroupError),
    #[error(transparent)]
    Completion(#[from] GroupError),
}

impl From<crate::remote::group::GroupError> for RemotePullError {
    fn from(error: crate::remote::group::GroupError) -> Self {
        PullGroupError(PullGroupCause::Source(error)).into()
    }
}

impl From<GroupError> for RemotePullError {
    fn from(error: GroupError) -> Self {
        PullGroupError(PullGroupCause::Completion(error)).into()
    }
}

impl RemotePullExecutor {
    pub(super) async fn execute_whole_group(
        &self,
        group: &mut HardlinkGroupPlan<'_>,
        policy: ExecutionPolicy,
    ) -> Result<()> {
        let mut publication_started = false;
        let result = self
            .execute_group_inner(group, policy, &mut publication_started)
            .await;
        match result {
            Err(source) if publication_started => Err(RemotePullError::SourceCompletionUncertain {
                path: self.destination_root.clone(),
                source: Box::new(source),
            }),
            result => result,
        }
    }

    async fn execute_group_inner(
        &self,
        group: &mut HardlinkGroupPlan<'_>,
        policy: ExecutionPolicy,
        publication_started: &mut bool,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                active_files: 1,
                buffered_bytes: REMOTE_FETCH_WORKING_SET,
                metadata_ops: 1,
                cpu_tasks: 1,
                network_writes: 1,
            })
            .await?;
        self.validate_file_fetch_options()?;
        if self.bsd_flags && !self.remote.capabilities().preserve_flags {
            return Err(
                crate::remote::transfer::RemoteTransferError::PreservationUnavailable {
                    feature: "BSD flags",
                }
                .into(),
            );
        }
        let mut members = group.members().await?;
        let first = members
            .next()
            .await?
            .ok_or(GroupError::Invalid("empty group"))?;
        let source = first.source();
        let mut intent = GroupIntent::new(&first, policy);
        while let Some(member) = members.next().await? {
            intent.include(&member);
        }
        let metadata = PullTransferMetadata {
            unix_mode: intent.unix_mode,
            modified: intent.modified,
        };
        let staged_metadata = staged_file_metadata(&metadata, &self.dest_path(first.path()))?;
        // The same acquired local authority owns staging, observations and effects.
        let rooted = Arc::new(self.metadata_authority().await?.clone());
        let identity = source
            .identity
            .ok_or(GroupError::Invalid("missing source observation"))?;
        let physical = crate::remote::path::encode_relative_path(source.path.as_path())
            .map_err(crate::remote::group::GroupError::from)?;
        let mut lease = SourceLease::open(
            self.sender.clone(),
            WireGroupOpen {
                role: WireGroupRole::Source,
                source_bytes: intent.canonical_source,
                retain_metadata: false,
                xattrs: self.xattrs,
                acls: self.acls,
                source_flags: self.bsd_flags,
                verify_bytes: false,
                compression: match self.compression {
                    None => WireFetchCompression::None,
                    Some(CompressionPolicy::Auto) => WireFetchCompression::Auto,
                    Some(CompressionPolicy::Always) => WireFetchCompression::Always,
                },
                file: WireFileBegin::delta(
                    physical,
                    source.size,
                    WireFileBasis::new(source.size, *identity.as_bytes()),
                ),
            },
        )
        .await?;
        let mut members = group.members().await?;
        let mut ordinal = 0_u64;
        while let Some(member) = members.next().await? {
            lease
                .declare(WireGroupMember {
                    ordinal,
                    path: crate::remote::path::encode_relative_path(member.source().path.as_path())
                        .map_err(crate::remote::group::GroupError::from)?,
                    observation: Some((
                        WireEntryKind::File,
                        *member
                            .source()
                            .identity
                            .ok_or(GroupError::Invalid("missing member source observation"))?
                            .as_bytes(),
                    )),
                })
                .await?;
            ordinal = next_ordinal(ordinal)?;
        }
        let (preservation, requested_flags) = lease.prepare().await?;
        let representative = destination(&first).cloned();
        // Retained bytes/metadata must come from the original observed FD, not
        // merely a pathname that was restored after a transient parent exchange.
        let mut old = if intent.retain_metadata || !intent.canonical_source {
            let old = representative
                .clone()
                .ok_or(GroupError::Invalid("missing retained destination"))?;
            Some(Arc::new(
                joined({
                    let rooted = rooted.clone();
                    move || {
                        rooted.verify_root_path_blocking()?;
                        let file = rooted.open_regular_blocking(&old.path)?;
                        validate_old(&rooted, &file, &old)?;
                        Ok(file)
                    }
                })
                .await?,
            ))
        } else {
            None
        };

        if !intent.canonical_source {
            let observed = representative
                .as_ref()
                .ok_or(GroupError::Invalid("missing no-op destination"))?;
            let file = old
                .as_ref()
                .ok_or(GroupError::Invalid("missing no-op FD"))?;
            let matches = joined({
                let rooted = rooted.clone();
                let observed = observed.clone();
                let file = file.clone();
                let preservation = preservation.clone();
                move || {
                    rooted.verify_root_path_blocking()?;
                    validate_old(&rooted, &file, &observed)?;
                    let matches = rooted.observed_file_preservation_matches_blocking(
                        &observed.path,
                        observed
                            .identity
                            .ok_or(GroupError::Invalid("missing no-op observation"))?,
                        metadata.unix_mode,
                        metadata.modified,
                        &crate::rooted_fs::MetadataPreservation {
                            xattrs: preservation.xattrs.as_deref(),
                            acl: preservation.acl.as_deref(),
                            bsd_flags: requested_flags,
                        },
                    )?;
                    validate_old(&rooted, &file, &observed)?;
                    Ok(matches)
                }
            })
            .await?;
            if matches {
                let mut members = group.members().await?;
                let mut ordinal = 0;
                while let Some(member) = members.next().await? {
                    lease.validate_member(ordinal).await?;
                    let observed = destination(&member)
                        .cloned()
                        .ok_or(GroupError::Invalid("missing no-op member"))?;
                    let proof = joined({
                        let rooted = rooted.clone();
                        let file = file.clone();
                        move || {
                            rooted.verify_root_path_blocking()?;
                            let native = validate_old(&rooted, &file, &observed)?;
                            rooted.verify_root_path_blocking()?;
                            Ok(PublishedFileProof {
                                path: observed.path,
                                identity: crate::endpoint::local_identity::metadata_identity(
                                    &native,
                                    EntryKind::File,
                                )
                                .ok_or(GroupError::Invalid("missing native no-op identity"))?,
                            })
                        }
                    })
                    .await?;
                    group.confirm_member(&member, &proof).await?;
                    ordinal = next_ordinal(ordinal)?;
                }
                verify_root(rooted.clone(), false, first.path().clone()).await?;
                lease.finish(group).await?;
                // A successful remote ACK cannot waive local operator continuity.
                verify_root(rooted, false, first.path().clone()).await?;
                return Ok(());
            }
        }

        let mut staged = LocalStagedWriter::new(
            rooted.clone(),
            first.path().clone(),
            destination_expectation(destination(&first))?,
            self.dest_path(first.path()),
        )
        .await?;
        let prepared: Result<(Option<TransferSummary>, Option<u32>)> = async {
            let retained_flags = if let (Some(old), Some(observed)) = (&old, &representative) {
                staged
                    .with_staged({
                        let old = old.clone();
                        let observed = observed.clone();
                        let rooted = rooted.clone();
                        let retain_metadata = intent.retain_metadata;
                        let retained_bytes = !intent.canonical_source;
                        move |stage| {
                            let native = rooted.observed_regular_metadata_blocking(
                                &old,
                                &observed.path,
                                observed.identity.ok_or_else(|| {
                                    RootedFsError::DestinationChanged(
                                        observed.path.as_path().to_path_buf(),
                                    )
                                })?,
                            )?;
                            if retain_metadata {
                                stage.copy_retained_metadata_blocking(&old)?;
                            }
                            if retained_bytes {
                                let mut input = old.try_clone()?;
                                input.seek(SeekFrom::Start(0))?;
                                #[cfg(unix)]
                                let sparse =
                                    crate::endpoint::local::copy_sparse_native_file_blocking(
                                        &mut input,
                                        stage.file_mut(),
                                    )?;
                                #[cfg(not(unix))]
                                let sparse: Option<u64> = None;
                                if sparse.is_none() {
                                    crate::endpoint::local::copy_native_file_blocking(
                                        &mut input,
                                        stage.file_mut(),
                                    )?;
                                }
                                // Sparse data-byte counts are not the logical FD length.
                                if stage.file_mut().metadata()?.len() != native.len() {
                                    return Err(RootedFsError::DestinationChanged(
                                        observed.path.as_path().to_path_buf(),
                                    ));
                                }
                            }
                            rooted.observed_regular_metadata_blocking(
                                &old,
                                &observed.path,
                                observed.identity.ok_or_else(|| {
                                    RootedFsError::DestinationChanged(
                                        observed.path.as_path().to_path_buf(),
                                    )
                                })?,
                            )?;
                            native_flags(&old)
                        }
                    })
                    .await?
            } else {
                None
            };
            let summary = if intent.canonical_source {
                Some(
                    lease
                        .receive(&mut staged, self.rate_limiter.as_deref())
                        .await?,
                )
            } else {
                None
            };
            staged
                .prepare_metadata(&staged_metadata, &preservation)
                .await?;
            if let Some(summary) = summary {
                let expected = blake3::Hash::from_bytes(summary.digest);
                let actual = staged
                    .staged_hash()
                    .await?
                    .ok_or(GroupError::Invalid("missing staged hash"))?;
                if actual != expected {
                    return Err(RemotePullError::StagedVerificationFailed {
                        path: self.dest_path(first.path()),
                        expected: expected.to_string(),
                        actual: actual.to_string(),
                    });
                }
            }
            if let (Some(old), Some(observed)) = (&old, &representative) {
                joined({
                    let rooted = rooted.clone();
                    let old = old.clone();
                    let observed = observed.clone();
                    move || {
                        validate_old(&rooted, &old, &observed)?;
                        Ok(())
                    }
                })
                .await?;
            }
            staged.prepare_publication().await?;
            Ok((summary, requested_flags.or(retained_flags)))
        }
        .await;
        let (summary, flags) = match prepared {
            Ok(value) => value,
            Err(operation) => {
                return match Box::new(staged).abort().await {
                    Ok(()) => Err(operation),
                    Err(abort) => Err(crate::error::SyncError::StagingAbortFailed {
                        operation: operation.to_string(),
                        abort: abort.to_string(),
                    }
                    .into()),
                }
            }
        };
        let mut owner = Some(staged.into_group().await?);
        // Keep the consumed OLD descriptor through the first native effect.
        // Never re-observe it after our own replacement: single-link retirement
        // may unlink OLD without a lineage record. The creator remains held.
        // The rooted authority also admits against the SSH session. This
        // transaction guard closes queued native work if its caller is dropped.
        let admission = Arc::new(crate::endpoint::publication::PublicationAdmission::default());
        let _close_admission = CloseAdmission(admission.clone());
        let publication: Result<u64> = async {
            let mut members = group.members().await?;
            let mut ordinal = 0;
            while let Some(member) = members.next().await? {
                lease.validate_member(ordinal).await?;
                self.backup_replacement(destination(&member)).await?;
                let relative = member.path().clone();
                let expected =
                    rooted_destination_expectation(destination_expectation(destination(&member))?);
                // Cancellation/native errors after this point cannot claim rollback.
                *publication_started = true;
                owner = Some(
                    joined({
                        let mut current = owner
                            .take()
                            .ok_or(GroupError::Invalid("missing native owner"))?;
                        let rooted = rooted.clone();
                        let admission = admission.clone();
                        move || {
                            let result = (|| {
                                rooted.verify_root_path_blocking()?;
                                current.publish_member_blocking(
                                    &relative,
                                    expected,
                                    &admission,
                                    || rooted.verify_root_path_blocking(),
                                )?;
                                crate::endpoint::local::verify_committed_root(
                                    &rooted,
                                    &rooted.root_path().join(relative.as_path()),
                                )?;
                                Ok::<_, RemotePullError>(())
                            })();
                            match result {
                                Ok(()) => Ok(current),
                                Err(operation) => abort_group(current, operation),
                            }
                        }
                    })
                    .await?,
                );
                if ordinal == 0 {
                    drop(old.take());
                }
                let work = match summary {
                    Some(summary) if ordinal == 0 => WorkResult::Transfer(summary),
                    _ => WorkResult::Metadata,
                };
                group.record_work(&member, work)?;
                self.report(
                    if destination(&member).is_some() {
                        crate::sync::output::ItemizeOp::Update
                    } else {
                        crate::sync::output::ItemizeOp::Create
                    },
                    crate::sync::output::ItemizeKind::File,
                    member.path(),
                );
                ordinal = next_ordinal(ordinal)?;
            }
            Ok(ordinal)
        }
        .await;
        let ordinal = match publication {
            Ok(ordinal) => ordinal,
            Err(operation) => {
                return match owner.take() {
                    Some(owner) => joined(move || abort_group(owner, operation)).await,
                    None => Err(operation),
                }
            }
        };
        let owner = owner
            .take()
            .ok_or(GroupError::Invalid("missing final native owner"))?;
        let finalized = Arc::new(
            joined({
                let rooted = rooted.clone();
                let first_path = first.path().clone();
                move || {
                    if let Err(operation) = rooted.verify_root_path_blocking() {
                        return abort_group(owner, operation.into());
                    }
                    let finalized = owner.finish_blocking(first_path.clone(), flags, admission)?;
                    crate::endpoint::local::verify_committed_root(
                        &rooted,
                        &rooted.root_path().join(first_path.as_path()),
                    )?;
                    Ok(finalized)
                }
            })
            .await?,
        );
        if finalized.member_count() != ordinal {
            return Err(GroupError::Invalid("native member count disagrees").into());
        }
        let mut members = group.members().await?;
        while let Some(member) = members.next().await? {
            let proof = joined({
                let rooted = rooted.clone();
                let finalized = finalized.clone();
                let relative = member.path().clone();
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
        }
        verify_root(rooted.clone(), true, first.path().clone()).await?;
        lease.finish(group).await?;
        verify_root(rooted, true, first.path().clone()).await?;
        Ok(())
    }
}

fn abort_group<T>(
    owner: crate::rooted_fs::RootedStagedGroup,
    operation: RemotePullError,
) -> Result<T> {
    match owner.abort_blocking() {
        Ok(()) => Err(operation),
        Err(abort) => Err(RootedFsError::StagingOperationAbortFailed {
            operation: operation.to_string(),
            abort: abort.to_string(),
        }
        .into()),
    }
}

struct CloseAdmission(Arc<crate::endpoint::publication::PublicationAdmission>);

impl Drop for CloseAdmission {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn next_ordinal(ordinal: u64) -> Result<u64> {
    ordinal
        .checked_add(1)
        .ok_or_else(|| GroupError::Invalid("member ordinal overflow").into())
}

fn validate_old(rooted: &RootedFs, file: &File, observed: &Entry) -> Result<std::fs::Metadata> {
    let identity = observed
        .identity
        .ok_or(GroupError::Invalid("missing retained observation"))?;
    Ok(rooted.observed_regular_metadata_blocking(file, &observed.path, identity)?)
}

fn native_flags(file: &File) -> crate::rooted_fs::Result<Option<u32>> {
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

async fn verify_root(rooted: Arc<RootedFs>, committed: bool, path: RelativePath) -> Result<()> {
    joined(move || {
        if committed {
            crate::endpoint::local::verify_committed_root(
                &rooted,
                &rooted.root_path().join(path.as_path()),
            )?;
        } else {
            rooted.verify_root_path_blocking()?;
        }
        Ok(())
    })
    .await
}

async fn joined<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RootedFsError::Worker(error.to_string()))?
}
