//! One original private inode, sequential publication, then exact final proof.
#[cfg(all(test, unix))]
#[path = "destination_tests.rs"]
mod tests;

use super::declarations::{Cursor, Declarations};
use super::{next_frame, send_result, GroupError, Result};
use crate::endpoint::io::Preservation;
use crate::endpoint::publication::PublicationAdmission;
use crate::endpoint::ExpectedDestination;
use crate::engine::domain::{EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::protocol::{
    FrameFlags, FrameKind, PlatformOs, ResolvedOperand, WireAcl, WireEntryKind, WireFileBasis,
    WireFileBegin, WireGroupMember, WireGroupOpen, WireGroupResult, WireGroupRole, WireGroupSeal,
    WireGroupStep, WireXattrResult,
};
use crate::remote::path::decode_relative_path;
use crate::remote::router::{IncomingStream, RouterSender, StreamInbox};
use crate::remote::transfer::population::{
    prepare_group_reconstruction_blocking, PreparedReconstruction, ReconstructionReceiver,
};
use crate::rooted_fs::{MetadataPreservation, RootedFs, RootedFsError, RootedStagedGroup};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;

struct Old {
    file: File,
    identity: EntryIdentity,
    size: u64,
}

enum Preparation {
    Reconstruction(Box<PreparedReconstruction>),
    Ready(WireGroupResult),
}

struct Destination {
    rooted: RootedFs,
    declarations: Declarations,
    cursor: Cursor,
    first: RelativePath,
    first_observation: Option<(WireEntryKind, [u8; 32])>,
    old: Option<Old>,
    owner: Option<RootedStagedGroup>,
    flags: Option<u32>,
    confirmed: u64,
}

fn observation(member: &WireGroupMember) -> Option<(EntryKind, EntryIdentity)> {
    member.observation.map(|(kind, identity)| {
        let kind = match kind {
            WireEntryKind::File => EntryKind::File,
            WireEntryKind::Directory => EntryKind::Directory,
            WireEntryKind::Symlink => EntryKind::Symlink,
        };
        (kind, EntryIdentity::from_bytes(identity))
    })
}

fn expected(member: &WireGroupMember) -> ExpectedDestination {
    observation(member).map_or(ExpectedDestination::Absent, |(_, identity)| {
        ExpectedDestination::Unchanged(identity)
    })
}

impl Destination {
    fn validate_member(&self, member: &WireGroupMember, peer: PlatformOs) -> Result<RelativePath> {
        let relative = decode_relative_path(member.path.clone(), peer)?;
        match observation(member) {
            Some((kind, identity)) => self
                .rooted
                .verify_retired_destination_blocking(&relative, kind, identity)?,
            None => {
                if self.rooted.path_identity_blocking(&relative)?.is_some() {
                    return Err(RootedFsError::DestinationChanged(
                        relative.as_path().to_path_buf(),
                    )
                    .into());
                }
            }
        }
        Ok(relative)
    }

    /// The client's retained-byte selection is valid only for a natively
    /// coalesced group. Equality uses exact retirement-resolved observations,
    /// not just an untrusted claim that these names share an inode.
    fn validate_all(&mut self, peer: PlatformOs, coalesced: bool) -> Result<()> {
        let mut cursor = self.declarations.cursor()?;
        let mut common = None;
        while let Some(member) = self.declarations.next(&mut cursor)? {
            self.validate_member(&member, peer)?;
            if coalesced {
                let Some((EntryKind::File, identity)) = observation(&member) else {
                    return Err(GroupError::Phase("retained group requires regular members"));
                };
                let identity = self
                    .rooted
                    .retired_destination_identity_blocking(identity)?;
                if common.is_some_and(|common| common != identity) {
                    return Err(GroupError::Phase(
                        "retained group is not natively coalesced",
                    ));
                }
                common = Some(identity);
            }
        }
        Ok(())
    }

    fn capture_old(&mut self) -> Result<()> {
        let Some((WireEntryKind::File, identity)) = self.first_observation else {
            return Err(GroupError::Phase(
                "retention requires a regular first destination",
            ));
        };
        let identity = EntryIdentity::from_bytes(identity);
        let file = self.rooted.open_regular_blocking(&self.first)?;
        let size = self
            .rooted
            .observed_regular_metadata_blocking(&file, &self.first, identity)?
            .len();
        self.old = Some(Old {
            file,
            identity,
            size,
        });
        Ok(())
    }

    fn validate_old(&self) -> crate::rooted_fs::Result<()> {
        if let Some(old) = &self.old {
            self.rooted
                .observed_regular_metadata_blocking(&old.file, &self.first, old.identity)?;
        }
        Ok(())
    }

    fn prepare(
        &mut self,
        open: &WireGroupOpen,
        preservation: Preservation,
        peer: PlatformOs,
    ) -> Result<Preparation> {
        MetadataPreservation {
            xattrs: preservation.xattrs.as_deref(),
            acl: preservation.acl.as_deref(),
            bsd_flags: open.file.bsd_flags(),
        }
        .validate()?;
        self.validate_all(peer, !open.source_bytes)?;
        if open.retain_metadata || !open.source_bytes {
            self.capture_old()?;
        }
        let modified = modified(&open.file)?;
        self.flags = open.file.bsd_flags();
        if !open.source_bytes {
            let old = self
                .old
                .as_ref()
                .ok_or(GroupError::Phase("missing OLD FD"))?;
            self.validate_old()?;
            let matches = self.rooted.observed_file_preservation_matches_blocking(
                &self.first,
                old.identity,
                open.file.unix_mode(),
                modified,
                &MetadataPreservation {
                    xattrs: preservation.xattrs.as_deref(),
                    acl: preservation.acl.as_deref(),
                    bsd_flags: self.flags,
                },
            )?;
            self.validate_old()?;
            if matches {
                let digest = if open.verify_bytes {
                    Some(hash_old(old, &self.first)?)
                } else {
                    None
                };
                self.validate_old()?;
                return Ok(Preparation::Ready(WireGroupResult::DestinationReady {
                    publish: false,
                    size: old.size,
                    digest,
                    literal_bytes: 0,
                    reused_bytes: 0,
                }));
            }
        }
        if self.flags.is_none() && open.retain_metadata {
            #[cfg(target_os = "macos")]
            {
                self.validate_old()?;
                self.flags = Some(
                    self.rooted.read_open_file_bsd_flags_blocking(
                        &self
                            .old
                            .as_ref()
                            .ok_or(GroupError::Phase("missing OLD FD"))?
                            .file,
                    )?,
                );
                self.validate_old()?;
            }
        }
        let destination = self.first_observation.map(|(_, identity)| {
            // This is target authority, NOT a regular-file COPY observation.
            // Population consults only its identity; COPY retains its own size.
            WireFileBasis::new(open.file.file_size(), identity)
        });
        if open.source_bytes {
            self.validate_old()?;
            let retained = if open.retain_metadata {
                Some(
                    &self
                        .old
                        .as_ref()
                        .ok_or(GroupError::Phase("missing OLD FD"))?
                        .file,
                )
            } else {
                None
            };
            let prepared = prepare_group_reconstruction_blocking(
                self.rooted.clone(),
                &self.first,
                destination,
                open.file.basis(),
                preservation,
                |rooted, relative, expectation| {
                    let mut stage = rooted
                        .begin_staged_file_with_expectation_blocking(relative, expectation)?;
                    if let Some(old) = retained {
                        let operation = stage.copy_retained_metadata_blocking(old);
                        if let Err(operation) = operation {
                            return Err(abort_stage(stage, operation).into());
                        }
                    }
                    Ok(stage)
                },
            )?;
            self.validate_old()?;
            return Ok(Preparation::Reconstruction(Box::new(prepared)));
        }
        let old = self
            .old
            .as_ref()
            .ok_or(GroupError::Phase("missing OLD FD"))?;
        let digest = hash_old(old, &self.first)?;
        self.validate_old()?;
        let member = WireGroupMember {
            ordinal: 0,
            path: open.file.path.clone(),
            observation: self.first_observation,
        };
        let mut stage = self
            .rooted
            .begin_staged_file_with_expectation_blocking(&self.first, expected(&member))?;
        let operation: Result<u64> = (|| {
            if open.retain_metadata {
                stage.copy_retained_metadata_blocking(&old.file)?;
            }
            self.validate_old()?;
            let mut input = old.file.try_clone()?;
            #[cfg(unix)]
            let sparse = crate::endpoint::local::copy_sparse_native_file_blocking(
                &mut input,
                stage.file_mut(),
            )?;
            #[cfg(not(unix))]
            let sparse: Option<u64> = None;
            let copied = match sparse {
                Some(bytes) => bytes,
                None => {
                    crate::endpoint::local::copy_native_file_blocking(&mut input, stage.file_mut())?
                }
            };
            self.validate_old()?;
            if stage.file_mut().metadata()?.len() != old.size || input.metadata()?.len() != old.size
            {
                return Err(GroupError::Phase("retained copy size changed"));
            }
            stage.apply_preservation_blocking(
                open.file.unix_mode(),
                modified,
                preservation.xattrs.as_deref(),
                preservation.acl.as_deref(),
            )?;
            if stage.staged_hash_blocking()?.as_bytes() != &digest {
                return Err(crate::remote::transfer::RemoteTransferError::DigestMismatch.into());
            }
            self.validate_old()?;
            Ok(copied)
        })();
        let copied = match operation {
            Ok(copied) => copied,
            Err(operation) => {
                return match stage.abort() {
                    Ok(()) => Err(operation),
                    Err(abort) => Err(GroupError::Cleanup {
                        operation: Box::new(operation),
                        cleanup: Box::new(abort.into()),
                    }),
                }
            }
        };
        self.owner = Some(stage.into_group_blocking()?);
        Ok(Preparation::Ready(WireGroupResult::DestinationReady {
            publish: true,
            size: old.size,
            digest: Some(digest),
            literal_bytes: 0,
            reused_bytes: copied,
        }))
    }

    fn publish(
        &mut self,
        ordinal: u64,
        peer: PlatformOs,
        admission: &Arc<PublicationAdmission>,
    ) -> Result<()> {
        if ordinal != self.confirmed {
            return Err(GroupError::Phase(
                "destination publication ordinal mismatch",
            ));
        }
        let member = self
            .declarations
            .next(&mut self.cursor)?
            .ok_or(GroupError::Phase("extra destination publication"))?;
        let relative = self.validate_member(&member, peer)?;
        self.validate_old()?;
        let old_identity = self
            .old
            .as_ref()
            .map(|old| {
                self.rooted
                    .retired_destination_identity_blocking(old.identity)
            })
            .transpose()?;
        if let Some(mut owner) = self.owner.take() {
            let operation =
                owner.publish_member_blocking(&relative, expected(&member), admission, || {
                    // Commit invokes this under the retirement mutex. Use the
                    // already-resolved observation, never reacquire that mutex.
                    if let (Some(old), Some(identity)) = (&self.old, old_identity) {
                        crate::remote::transfer::validate_source(&old.file, identity, old.size)
                            .map_err(|_| {
                                RootedFsError::DestinationChanged(
                                    self.first.as_path().to_path_buf(),
                                )
                            })?;
                        if self.rooted.path_identity_blocking(&self.first)?
                            != Some((EntryKind::File, identity))
                        {
                            return Err(RootedFsError::DestinationChanged(
                                self.first.as_path().to_path_buf(),
                            ));
                        }
                    }
                    Ok(())
                });
            self.owner = Some(owner);
            operation?;
            // OLD has been consumed and validated. Its first name is now NEW;
            // an unlinked single-link OLD has intentionally no alias lineage.
            // Only NoOp retains OLD for the final all-binding proof.
            self.old = None;
        }
        self.confirmed = self
            .confirmed
            .checked_add(1)
            .ok_or(GroupError::Phase("destination publication count overflow"))?;
        Ok(())
    }

    fn finish(
        &mut self,
        seal: WireGroupSeal,
        peer: PlatformOs,
        admission: Arc<PublicationAdmission>,
    ) -> Result<EntryIdentity> {
        if self.confirmed != seal.count || self.declarations.next(&mut self.cursor)?.is_some() {
            return Err(GroupError::Phase("destination publication incomplete"));
        }
        self.validate_old()?;
        if let Some(owner) = self.owner.take() {
            let finalized = owner.finish_blocking(self.first.clone(), self.flags, admission)?;
            if finalized.member_count() != seal.count {
                return Err(GroupError::Phase("native group member count mismatch"));
            }
            let mut cursor = self.declarations.cursor()?;
            let mut identity = None;
            while let Some(member) = self.declarations.next(&mut cursor)? {
                let relative = decode_relative_path(member.path, peer)?;
                let proof = finalized.proof_for_member_blocking(&relative)?;
                if identity.is_some_and(|identity| identity != proof.identity) {
                    return Err(GroupError::Phase("final group identities differ"));
                }
                identity = Some(proof.identity);
            }
            identity.ok_or(GroupError::Phase("empty final group proof"))
        } else {
            self.validate_all(peer, true)?;
            self.validate_old()?;
            let old = self
                .old
                .as_ref()
                .ok_or(GroupError::Phase("missing no-op OLD FD"))?;
            Ok(self
                .rooted
                .retired_destination_identity_blocking(old.identity)?)
        }
    }

    fn abort(mut self) -> Result<()> {
        if let Some(owner) = self.owner.take() {
            owner.abort_blocking()?;
        }
        Ok(())
    }
}

fn abort_stage(
    stage: crate::rooted_fs::RootedStagedFile,
    operation: RootedFsError,
) -> RootedFsError {
    match stage.abort() {
        Ok(()) => operation,
        Err(abort) => RootedFsError::StagingOperationAbortFailed {
            operation: operation.to_string(),
            abort: abort.to_string(),
        },
    }
}

fn modified(begin: &WireFileBegin) -> Result<Option<Timestamp>> {
    begin
        .modified()
        .map(|(seconds, nanoseconds)| {
            Timestamp::new(seconds, nanoseconds)
                .map_err(|_| GroupError::Phase("invalid intended mtime"))
        })
        .transpose()
}

fn hash_old(old: &Old, relative: &RelativePath) -> Result<[u8; 32]> {
    let mut file = old.file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(crate::endpoint::existing::hash_observed_bytes(
        &mut file, old.size, relative,
    )?)
}

async fn metadata(inbox: &mut StreamInbox, open: &WireGroupOpen) -> Result<Option<Preservation>> {
    let mut preservation = Preservation::default();
    loop {
        let routed = next_frame(inbox).await?;
        let frame = routed.frame();
        match frame.kind() {
            FrameKind::FileXattrs if open.xattrs && preservation.xattrs.is_none() => {
                let values = WireXattrResult::decode(frame.payload())?;
                preservation.xattrs = Some(
                    values
                        .entries()
                        .iter()
                        .map(|value| {
                            Ok((
                                crate::engine::native_path::decode(value.name())?.into_os_string(),
                                value.value().to_vec(),
                            ))
                        })
                        .collect::<std::io::Result<_>>()?,
                );
            }
            FrameKind::FileAcls if open.acls && preservation.acl.is_none() => {
                preservation.acl = Some(WireAcl::decode(frame.payload())?.text().to_owned());
            }
            FrameKind::GroupStep => match WireGroupStep::decode(frame.payload())? {
                WireGroupStep::Prepare
                    if preservation.xattrs.is_some() == open.xattrs
                        && preservation.acl.is_some() == open.acls =>
                {
                    return Ok(Some(preservation))
                }
                WireGroupStep::Abort => return Ok(None),
                _ => {
                    return Err(GroupError::Phase(
                        "destination metadata incomplete or invalid preparation",
                    ))
                }
            },
            _ => {
                return Err(GroupError::Phase(
                    "unexpected or duplicate destination metadata",
                ))
            }
        }
    }
}

/// The session must drain this handler rather than abort its task: native
/// publication already admitted cannot be interrupted or rolled back, and
/// ordinary outcomes join private cleanup before releasing the original owner.
pub(super) async fn serve(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
    resolved: ResolvedOperand,
) -> Result<()> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream = inbox.stream_id();
    if first.frame().kind() != FrameKind::GroupOpen
        || first.frame().stream_id() != stream
        || !first.frame().flags().is_empty()
    {
        return Err(GroupError::Phase(
            "invalid destination opener stream or flags",
        ));
    }
    let open = WireGroupOpen::decode(first.frame().payload())?;
    if open.role != WireGroupRole::Destination
        || open.source_flags != open.file.bsd_flags().is_some()
    {
        return Err(GroupError::Phase(
            "destination role or requested flags mismatch",
        ));
    }
    drop(first);
    let mut declarations = tokio::task::spawn_blocking(Declarations::new)
        .await
        .map_err(|error| GroupError::Worker(error.to_string()))??;
    let seal = loop {
        let routed = next_frame(&mut inbox).await?;
        match routed.frame().kind() {
            FrameKind::GroupMember => {
                crate::remote::runtime::scope::authorize(&resolved, routed.frame(), peer)?;
                let member = WireGroupMember::decode(routed.frame().payload())?;
                declarations = tokio::task::spawn_blocking(move || {
                    declarations.append(&member)?;
                    Ok::<_, GroupError>(declarations)
                })
                .await
                .map_err(|error| GroupError::Worker(error.to_string()))??;
            }
            FrameKind::GroupMembersEnd => break WireGroupSeal::decode(routed.frame().payload())?,
            _ => return Err(GroupError::Phase("expected destination declarations")),
        }
    };
    let file_path = open.file.path.clone();
    let copy_basis = open.file.basis();
    let native = tokio::task::spawn_blocking(move || {
        declarations.seal(seal)?;
        let mut cursor = declarations.cursor()?;
        let first = declarations
            .next(&mut cursor)?
            .ok_or(GroupError::Phase("empty destination declarations"))?;
        if first.path != file_path {
            return Err(GroupError::Phase(
                "destination opener differs from first declaration",
            ));
        }
        if let Some(basis) = copy_basis {
            if first.observation != Some((WireEntryKind::File, basis.identity())) {
                return Err(GroupError::Phase(
                    "COPY basis differs from first observation",
                ));
            }
        }
        let cursor = declarations.cursor()?;
        Ok::<_, GroupError>(Destination {
            rooted,
            declarations,
            cursor,
            first: decode_relative_path(first.path, peer)?,
            first_observation: first.observation,
            old: None,
            owner: None,
            flags: None,
            confirmed: 0,
        })
    })
    .await
    .map_err(|error| GroupError::Worker(error.to_string()))??;
    let mut native = Some(native);
    let mut published = 0;
    let result: Result<()> = async {
        send_result(sender, stream, WireGroupResult::Declarations(seal)).await?;
        let Some(preservation) = metadata(&mut inbox, &open).await? else {
            let owner = native
                .take()
                .ok_or(GroupError::Phase("destination ownership"))?;
            tokio::task::spawn_blocking(move || owner.abort())
                .await
                .map_err(|error| GroupError::Worker(error.to_string()))??;
            send_result(sender, stream, WireGroupResult::Aborted).await?;
            return Ok(());
        };
        let mut owner = native
            .take()
            .ok_or(GroupError::Phase("destination ownership"))?;
        let worker_open = open.clone();
        let (owner, prepared) = tokio::task::spawn_blocking(move || {
            let result = owner.prepare(&worker_open, preservation, peer);
            (owner, result)
        })
        .await
        .map_err(|error| GroupError::Worker(error.to_string()))?;
        native = Some(owner);
        let ready = match prepared? {
            Preparation::Reconstruction(prepared) => {
                let mut receiver = ReconstructionReceiver::start(*prepared, open.file.clone());
                let received: Result<_> = async {
                    let routed = next_frame(&mut inbox).await?;
                    if routed.frame().kind() != FrameKind::FileBegin
                        || WireFileBegin::decode(routed.frame().payload())? != open.file
                    {
                        return Err(GroupError::Phase("group FileBegin differs from opener"));
                    }
                    drop(routed);
                    Ok(receiver
                        .receive(&mut inbox, |frame| {
                            if frame.flags() == FrameFlags::ACK_REQUIRED {
                                Ok(())
                            } else {
                                Err(crate::remote::transfer::RemoteTransferError::FileEndFlags {
                                    flags: frame.flags().bits(),
                                })
                            }
                        })
                        .await?)
                }
                .await;
                let received = match received {
                    Ok(ready) => ready,
                    Err(operation) => {
                        return match receiver.abort().await {
                            Ok(()) => Err(operation),
                            Err(cleanup) => Err(GroupError::Cleanup {
                                operation: Box::new(operation),
                                cleanup: Box::new(cleanup.into()),
                            }),
                        }
                    }
                };
                let mut owner = native
                    .take()
                    .ok_or(GroupError::Phase("destination ownership"))?;
                let (owner, ready) = tokio::task::spawn_blocking(move || {
                    let result: Result<_> = (|| {
                        owner.validate_old()?;
                        let (stage, summary, _) = received.into_staged_blocking()?;
                        owner.owner = Some(stage.into_group_blocking()?);
                        Ok(WireGroupResult::DestinationReady {
                            publish: true,
                            size: summary.file_size,
                            digest: Some(summary.digest),
                            literal_bytes: summary.literal_bytes,
                            reused_bytes: summary.reused_bytes,
                        })
                    })();
                    (owner, result)
                })
                .await
                .map_err(|error| GroupError::Worker(error.to_string()))?;
                native = Some(owner);
                ready?
            }
            Preparation::Ready(ready) => ready,
        };
        let effects = matches!(
            ready,
            WireGroupResult::DestinationReady { publish: true, .. }
        );
        send_result(sender, stream, ready).await?;
        let admission = sender.publication_admission();
        loop {
            let routed = next_frame(&mut inbox).await?;
            if routed.frame().kind() != FrameKind::GroupStep {
                return Err(GroupError::Phase(
                    "expected destination publication or completion",
                ));
            }
            let step = WireGroupStep::decode(routed.frame().payload())?;
            drop(routed);
            let mut owner = native
                .take()
                .ok_or(GroupError::Phase("destination ownership"))?;
            let worker_admission = Arc::clone(&admission);
            let (owner, reply) = tokio::task::spawn_blocking(move || {
                let result: Result<_> = match step {
                    WireGroupStep::Publish(ordinal) => owner
                        .publish(ordinal, peer, &worker_admission)
                        .map(|()| WireGroupResult::Published(ordinal)),
                    WireGroupStep::Finish(supplied) if supplied == seal => owner
                        .finish(seal, peer, worker_admission)
                        .map(|identity| WireGroupResult::DestinationComplete {
                            seal,
                            identity: *identity.as_bytes(),
                        }),
                    WireGroupStep::Abort => Ok(WireGroupResult::Aborted),
                    _ => Err(GroupError::Phase("destination command or seal mismatch")),
                };
                (owner, result)
            })
            .await
            .map_err(|error| GroupError::Worker(error.to_string()))?;
            if effects {
                published = owner.confirmed;
            }
            native = Some(owner);
            let reply = reply?;
            if reply == WireGroupResult::Aborted {
                let owner = native
                    .take()
                    .ok_or(GroupError::Phase("destination ownership"))?;
                tokio::task::spawn_blocking(move || owner.abort())
                    .await
                    .map_err(|error| GroupError::Worker(error.to_string()))??;
                if published != 0 {
                    return Err(GroupError::Phase("group aborted after publication"));
                }
            }
            send_result(sender, stream, reply).await?;
            if matches!(
                reply,
                WireGroupResult::DestinationComplete { .. } | WireGroupResult::Aborted
            ) {
                return Ok(());
            }
        }
    }
    .await;
    let cleanup = tokio::task::spawn_blocking(move || match native {
        Some(owner) => owner.abort(),
        None => Ok(()),
    })
    .await
    .map_err(|error| GroupError::Worker(error.to_string()))?;
    let result = match (result, cleanup) {
        (Err(operation), Err(cleanup)) => Err(GroupError::Cleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        }),
        (result, Ok(())) => result,
        (Ok(()), Err(cleanup)) => Err(cleanup),
    };
    result.map_err(|source| {
        if published == 0 {
            source
        } else {
            GroupError::PartialPublication {
                published,
                source: Box::new(source),
            }
        }
    })
}
