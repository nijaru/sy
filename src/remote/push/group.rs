//! Sealed controller selection drives one live destination inode owner.
use super::*;
use crate::engine::hardlink_runtime::intent::{destination, GroupIntent};
use crate::engine::hardlink_runtime::HardlinkGroupPlan;
use crate::engine::work::{TransferSummary, WorkResult};
use crate::protocol::{
    WireEntryKind, WireFetchCompression, WireFileBegin, WireGroupMember, WireGroupOpen,
    WireGroupRole,
};
use crate::remote::group::destination_client::DestinationGroup;
use crate::remote::source_file::{GroupSource, SourceMetadataRequest};
use crate::rooted_fs::PublishedFileProof;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;

fn failure(error: impl std::error::Error + Send + Sync + 'static) -> RemotePushError {
    RemotePushError::Group(Box::new(error))
}

fn invalid(message: &'static str) -> RemotePushError {
    failure(std::io::Error::other(message))
}

async fn native<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(failure)?
}

impl RemotePushExecutor {
    pub(super) async fn execute_whole_group(
        &self,
        group: &mut HardlinkGroupPlan<'_>,
        policy: ExecutionPolicy,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                active_files: 1,
                buffered_bytes: REMOTE_FILE_WORKING_SET,
                metadata_ops: 1,
                cpu_tasks: 1,
                network_writes: 1,
            })
            .await?;
        let mut members = group.members().await.map_err(failure)?;
        let first = members
            .next()
            .await
            .map_err(failure)?
            .ok_or_else(|| invalid("empty selected group"))?;
        let source = first.source().clone();
        let identity = source
            .identity
            .ok_or_else(|| invalid("missing source scan identity"))?;
        // Resolve byte intent from the original sealed selection before opening
        // SOURCE. First-selected ordinary metadata policy remains unchanged.
        let mut intent = GroupIntent::new(&first, policy);
        while let Some(member) = members.next().await.map_err(failure)? {
            intent.include(&member);
        }
        let original = native({
            let root = self.source_root.clone();
            let source = source.clone();
            let require_data = intent.canonical_source || self.remove_source_files;
            let request = SourceMetadataRequest {
                xattrs: self.xattrs,
                acls: self.acls,
                bsd_flags: self.bsd_flags,
            };
            move || {
                root.validate_blocking()?;
                let file = GroupSource::capture_blocking(
                    root.rooted(),
                    source.path,
                    identity,
                    source.size,
                    require_data,
                    request,
                )
                .map_err(failure)?;
                root.validate_blocking()?;
                Ok(Arc::new(file))
            }
        })
        .await?;
        let result = self
            .run_group(group, intent, first, Arc::clone(&original))
            .await;
        // Native handle destruction, like acquisition and all syscalls, belongs
        // on a blocking worker. The original survives the final ACK and proofs.
        native(move || {
            drop(original);
            Ok(())
        })
        .await?;
        result
    }

    async fn validate_group_member(
        &self,
        original: Arc<GroupSource>,
        source: &Entry,
    ) -> Result<()> {
        let source = source.clone();
        let root = self.source_root.clone();
        native(move || {
            root.validate_blocking()?;
            original
                .validate_member_blocking(
                    &source.path,
                    source
                        .identity
                        .ok_or_else(|| invalid("missing member identity"))?,
                )
                .map_err(failure)?;
            root.validate_blocking()?;
            Ok(())
        })
        .await
    }

    async fn validate_group(
        &self,
        group: &HardlinkGroupPlan<'_>,
        original: &Arc<GroupSource>,
    ) -> Result<()> {
        let mut members = group.members().await.map_err(failure)?;
        while let Some(member) = members.next().await.map_err(failure)? {
            self.validate_group_member(Arc::clone(original), member.source())
                .await?;
        }
        Ok(())
    }

    async fn run_group(
        &self,
        group: &mut HardlinkGroupPlan<'_>,
        intent: GroupIntent,
        first: SyncOp,
        original: Arc<GroupSource>,
    ) -> Result<()> {
        let mut members = group.members().await.map_err(failure)?;
        while let Some(member) = members.next().await.map_err(failure)? {
            self.validate_group_member(Arc::clone(&original), member.source())
                .await?;
        }
        let mode = intent.unix_mode.ok_or_else(|| {
            RemotePushLowerError::MissingFileMode(first.source().path.as_path().to_path_buf())
        })?;
        let (preservation, flags) = native({
            let root = self.source_root.clone();
            let original = Arc::clone(&original);
            let request = SourceMetadataRequest {
                xattrs: self.xattrs,
                acls: self.acls,
                bsd_flags: self.bsd_flags,
            };
            move || {
                root.validate_blocking()?;
                let metadata = original.read_metadata_blocking(request).map_err(failure)?;
                root.validate_blocking()?;
                Ok(metadata)
            }
        })
        .await?;
        // Only a regular observed representative can be a COPY basis. Member
        // namespace expectations remain independent declarations.
        let old = destination(&first).filter(|entry| entry.is_file());
        let basis = if intent.canonical_source {
            if let Some(old) = old.filter(|entry| {
                delta_candidate(entry, self.delta_min_size, self.remote.capabilities())
            }) {
                self.remote.delta_basis(old, self.delta_limits).await?
            } else {
                None
            }
        } else {
            None
        };
        let path =
            crate::remote::path::encode_relative_path(first.path().as_path()).map_err(failure)?;
        let file = match (&basis, old) {
            (Some(_), Some(old)) => WireFileBegin::delta(
                path,
                first.source().size,
                crate::protocol::WireFileBasis::new(
                    old.size,
                    *old.identity
                        .ok_or_else(|| invalid("missing basis identity"))?
                        .as_bytes(),
                ),
            ),
            _ => WireFileBegin::whole(path, first.source().size),
        }
        .with_metadata(
            Some(mode),
            intent
                .modified
                .map(|time| (time.seconds(), time.nanoseconds())),
        )
        .map_err(failure)?
        .with_bsd_flags(flags);
        let mut remote = DestinationGroup::open(
            self.remote.sender(),
            WireGroupOpen {
                role: WireGroupRole::Destination,
                source_bytes: intent.canonical_source,
                retain_metadata: intent.retain_metadata,
                xattrs: self.xattrs,
                acls: self.acls,
                source_flags: flags.is_some(),
                verify_bytes: self.remove_source_files,
                compression: match self.compression {
                    None => WireFetchCompression::None,
                    Some(CompressionPolicy::Auto) => WireFetchCompression::Auto,
                    Some(CompressionPolicy::Always) => WireFetchCompression::Always,
                },
                file,
            },
        )
        .await
        .map_err(failure)?;
        let mut members = group.members().await.map_err(failure)?;
        let mut ordinal = 0;
        while let Some(member) = members.next().await.map_err(failure)? {
            let observation = destination(&member)
                .map(|entry| -> Result<_> {
                    Ok((
                        match entry.kind {
                            EntryKind::File => WireEntryKind::File,
                            EntryKind::Directory => WireEntryKind::Directory,
                            EntryKind::Symlink => WireEntryKind::Symlink,
                        },
                        *entry
                            .identity
                            .ok_or_else(|| invalid("missing destination identity"))?
                            .as_bytes(),
                    ))
                })
                .transpose()?;
            remote
                .declare(WireGroupMember {
                    ordinal,
                    path: crate::remote::path::encode_relative_path(member.path().as_path())
                        .map_err(failure)?,
                    observation,
                })
                .await
                .map_err(failure)?;
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| invalid("group ordinal overflow"))?;
        }
        let prepared = remote.prepare(preservation).await.map_err(failure)?;
        let ready = if intent.canonical_source {
            let root = self.source_root.clone();
            let original = Arc::clone(&original);
            let source = first.source().clone();
            let compression = self.compression;
            remote
                .populate(move || {
                    let (tx, rx) = tokio::sync::mpsc::channel(8);
                    let worker = tokio::task::spawn_blocking(move || {
                        root.validate_blocking()?;
                        let reader = original.clone_reader_blocking()?;
                        root.validate_blocking()?;
                        let summary = crate::remote::transfer::produce_source(
                            reader,
                            source.identity.ok_or_else(|| {
                                std::io::Error::other("missing original identity")
                            })?,
                            source.size,
                            basis,
                            tx,
                            compression,
                        )?;
                        root.validate_blocking()?;
                        original.validate_blocking()?;
                        root.validate_blocking()?;
                        Ok(summary)
                    });
                    (rx, worker)
                })
                .await
                .map_err(failure)?
        } else {
            prepared.ok_or_else(|| invalid("retained group missing readiness"))?
        };
        let admission = async {
            let parity = if self.remove_source_files {
                let digest = native({
                    let root = self.source_root.clone();
                    let original = Arc::clone(&original);
                    let source = first.source().clone();
                    move || {
                        root.validate_blocking()?;
                        let mut reader = original.clone_reader_blocking().map_err(failure)?;
                        reader.seek(SeekFrom::Start(0))?;
                        let digest =
                            existing::hash_observed_bytes(&mut reader, source.size, &source.path)?;
                        original.validate_blocking().map_err(failure)?;
                        root.validate_blocking()?;
                        Ok(digest)
                    }
                })
                .await?;
                ready.size == first.source().size && ready.digest == Some(digest)
            } else {
                false
            };
            // All declared names must still belong to the original observation
            // before the first publication, not only the representative.
            self.validate_group(group, &original).await?;
            Ok::<_, RemotePushError>(parity)
        }
        .await;
        let parity = match admission {
            Ok(parity) => parity,
            Err(operation) => {
                // Ready is quiescent and no publication has been requested.
                // Byte-phase/lost-reply errors instead escape to session drain.
                if let Err(cleanup) = remote.abort().await {
                    return Err(failure(std::io::Error::other(format!(
                        "{operation}; private group cleanup failed: {cleanup}"
                    ))));
                }
                return Err(operation);
            }
        };
        let completion = async {
            let mut members = group.members().await.map_err(failure)?;
            let mut ordinal = 0;
            while let Some(member) = members.next().await.map_err(failure)? {
                if ready.publish {
                    if let (Some(plan), Some(old)) = (
                        &self.backup,
                        destination(&member).filter(|entry| entry.is_file()),
                    ) {
                        let backup = plan.destination(&old.path).ok_or_else(|| {
                            RemotePushError::InvalidBackupPath(old.path.as_path().to_path_buf())
                        })?;
                        self.copy_backup(&old.path, &backup, old.identity).await?;
                    }
                }
                self.validate_group_member(Arc::clone(&original), member.source())
                    .await?;
                remote.publish(ordinal).await.map_err(failure)?;
                ordinal += 1;
            }
            let final_identity = remote.finish().await.map_err(failure)?;
            self.validate_group(group, &original).await?;
            let mut members = group.members().await.map_err(failure)?;
            let mut first_bytes = true;
            while let Some(member) = members.next().await.map_err(failure)? {
                group
                    .confirm_member(
                        &member,
                        &PublishedFileProof {
                            path: member.path().clone(),
                            identity: final_identity,
                        },
                    )
                    .await
                    .map_err(failure)?;
                if ready.publish {
                    // Only population moves bytes. Native link publications
                    // are metadata work, not fictitious delta reconstruction.
                    let work = if intent.canonical_source && first_bytes {
                        WorkResult::Transfer(TransferSummary {
                            file_size: ready.size,
                            digest: ready
                                .digest
                                .ok_or_else(|| invalid("published group missing digest"))?,
                            literal_bytes: ready.literal_bytes,
                            reused_bytes: ready.reused_bytes,
                        })
                    } else {
                        WorkResult::Metadata
                    };
                    group.record_work(&member, work).map_err(failure)?;
                    self.report(
                        if destination(&member).is_some() {
                            crate::sync::output::ItemizeOp::Update
                        } else {
                            crate::sync::output::ItemizeOp::Create
                        },
                        crate::sync::output::ItemizeKind::File,
                        member.path(),
                    );
                }
                first_bytes = false;
            }
            group.verify_complete().await.map_err(failure)?;
            self.validate_group(group, &original).await?;
            // An inode-only remote ACK cannot distinguish a source name from
            // that very same destination binding on a shared filesystem. A
            // fulfilled group can legitimately retain the source inode: keep
            // those sources rather than unlink a potentially identical name.
            if parity && Some(final_identity) != first.source().identity {
                let mut members = group.members().await.map_err(failure)?;
                while let Some(member) = members.next().await.map_err(failure)? {
                    let receipt = crate::endpoint::PublishedDestinationReceipt::for_file(
                        member.source().path.clone(),
                        member.path().clone(),
                        member.source().identity,
                        &crate::endpoint::io::VerificationStatus::Verified,
                        PublishedFileProof {
                            path: member.path().clone(),
                            identity: final_identity,
                        },
                    );
                    self.source_removals
                        .append_published(member.source(), &receipt)
                        .await
                        .map_err(|error| {
                            RemotePushError::SourceRemoval(
                                self.source_root.path().to_path_buf(),
                                std::io::Error::other(error),
                            )
                        })?;
                }
            }
            Ok(())
        }
        .await;
        completion.map_err(|error| RemotePushError::SourceCompletionUncertain(Box::new(error)))
    }
}

#[cfg(all(test, unix))]
#[path = "group_tests.rs"]
mod tests;
