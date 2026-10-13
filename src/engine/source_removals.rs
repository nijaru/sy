//! Deferred source removal for standalone and hardlink-group members, after
//! the caller's main-work finalization.
//!
//! Eligibility is authenticated by the original PlanJournal append seals. No
//! destination proof is minted from a fresh scan or a replay scratch checksum.
//! Destination validation is endpoint-owned; source authority is always the
//! original local SourceRoot (including for Push). These observational checks
//! are not namespace CAS, snapshot isolation, or whole-run rollback.

mod state_index;
use super::domain::{
    ContentComparison, Entry, EntryIdentity, EntryKind, RelativePath, SyncOp, Timestamp,
};
use super::plan_journal::{PlanJournal, PlanJournalError, PlanJournalReader};
use crate::endpoint::existing::{self, ExistingDestinationError};
use crate::endpoint::source_root::SourceRoot;
use crate::endpoint::{PublishedDestinationReceipt, VerifiedExistingDestinationReceipt};
use crate::rooted_fs::{HardlinkSourceState, PublishedEntryProof, RootedFs, RootedFsError};
use state_index::StateIndex;
#[cfg(test)]
use state_index::STATE_RECORD_BYTES;
#[cfg(test)]
use std::fs::File;
use std::future::Future;
use std::io;
#[cfg(test)]
use std::io::{Seek, SeekFrom, Write};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Distinct authorities reconstructed only from authenticated original records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceDestinationProof {
    Published(PublishedEntryProof),
    Existing {
        source_path: RelativePath,
        path: RelativePath,
        identity: EntryIdentity,
        own_source_effect: Option<OwnSourceEffect>,
    },
}

/// A source-only continuation read from the authenticated native state index.
/// It qualifies equal original tokens only within the local native inode domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OwnSourceEffect {
    original_identity: EntryIdentity,
    current: HardlinkSourceState,
}

impl SourceDestinationProof {
    pub(crate) fn revalidate_local_blocking(
        &self,
        source: &RootedFs,
        destination: &RootedFs,
    ) -> io::Result<()> {
        destination
            .verify_root_path_blocking()
            .map_err(io::Error::other)?;
        match self {
            Self::Published(proof) => proof
                .revalidate_blocking(destination)
                .map_err(io::Error::other)?,
            Self::Existing {
                source_path,
                path,
                identity,
                own_source_effect,
            } => {
                source
                    .verify_source_root_path_blocking()
                    .map_err(io::Error::other)?;
                if source
                    .entry_binding_blocking(source_path)
                    .map_err(io::Error::other)?
                    == destination
                        .entry_binding_blocking(path)
                        .map_err(io::Error::other)?
                {
                    return Err(io::Error::other(ExistingDestinationError::SameBinding(
                        source_path.clone(),
                    )));
                }
                match own_source_effect {
                    Some(effect) => {
                        if effect.original_identity != *identity {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "source continuation does not match original destination",
                            ));
                        }
                        // Both original tokens were native local observations of
                        // the same inode. Only our SOURCE unlink can advance it;
                        // destination retirement is never source authority.
                        destination
                            .validate_source_continuation_blocking(
                                path,
                                effect.original_identity,
                                &effect.current,
                            )
                            .map_err(io::Error::other)?;
                    }
                    None => destination
                        .verify_retired_destination_blocking(path, EntryKind::File, *identity)
                        .map_err(io::Error::other)?,
                }
                source
                    .verify_source_root_path_blocking()
                    .map_err(io::Error::other)?;
            }
        }
        destination
            .verify_root_path_blocking()
            .map_err(io::Error::other)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RemovalCause {
    #[error(transparent)]
    Journal(#[from] PlanJournalError),
    #[error(transparent)]
    Receipt(#[from] crate::error::SyncError),
    #[error(transparent)]
    Existing(#[from] ExistingDestinationError),
    #[error("{0}")]
    Rooted(#[from] RootedFsError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("source-removal worker failed: {0}")]
    Worker(#[source] tokio::task::JoinError),
    #[error("source-removal journal is terminal")]
    Terminal,
    #[error("invalid source-removal eligibility: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, thiserror::Error)]
#[error("source removal failed after {removed} confirmed unlinks (current native removal uncertain: {removal_uncertain}): {cause}")]
pub(crate) struct SourceRemovalError {
    /// Confirmed unlinks, including a successful unlink before scratch failure.
    pub removed: u64,
    /// A failing native helper may have unlinked the current name before failing.
    /// At most one additional removal is uncertain: native work is sequential.
    pub removal_uncertain: bool,
    #[source]
    pub cause: RemovalCause,
}

type Result<T> = std::result::Result<T, SourceRemovalError>;

#[derive(Default)]
pub(crate) struct SourceRemovalJournal {
    state: Mutex<JournalState>,
}

#[derive(Default)]
struct JournalState {
    terminal: bool,
    writer: Option<Box<PlanJournal>>,
    reader: Option<PlanJournalReader>,
    // Retained across cancellation: close() must join already-admitted work.
    worker: Option<JoinHandle<Result<NativeCompletion>>>,
    plan_worker: Option<JoinHandle<super::plan_journal::Result<PlanCompletion>>>,
    worker_removing: bool,
    removed: u64,
}

// Plan operations retain their owner in a task so cancelling the joining future
// cannot detach PlanJournal's admitted filesystem work. close() joins the task.
enum PlanOwner {
    Writer(Box<PlanJournal>),
    Reader(PlanJournalReader),
}

type PlanCompletion = (PlanOwner, Option<SyncOp>);
// One source phase returns its native owner and, for the late gate, one proof.
// The retained worker owns both until joined, including after cancellation.
type NativeCompletion = (NativeState, Option<SourceDestinationProof>);

impl JournalState {
    fn error(&self, cause: impl Into<RemovalCause>) -> SourceRemovalError {
        SourceRemovalError {
            removed: self.removed,
            removal_uncertain: false,
            cause: cause.into(),
        }
    }

    async fn join_plan(&mut self) -> Result<Option<SyncOp>> {
        let Some(worker) = self.plan_worker.as_mut() else {
            return Ok(None);
        };
        let joined = worker.await;
        self.plan_worker.take();
        let (owner, operation) = joined
            .map_err(|e| self.error(RemovalCause::Worker(e)))?
            .map_err(|e| self.error(e))?;
        match owner {
            PlanOwner::Writer(writer) => self.writer = Some(writer),
            PlanOwner::Reader(reader) => self.reader = Some(reader),
        }
        Ok(operation)
    }

    async fn join_worker(&mut self) -> Result<Option<NativeCompletion>> {
        self.join_plan().await?;
        let Some(worker) = self.worker.as_mut() else {
            return Ok(None);
        };
        let joined = worker.await;
        self.worker.take();
        let result = joined.map_err(|cause| SourceRemovalError {
            removed: self.removed,
            removal_uncertain: self.worker_removing,
            cause: RemovalCause::Worker(cause),
        })?;
        match result {
            Ok((native, proof)) => {
                self.removed = native.removed;
                Ok(Some((native, proof)))
            }
            Err(error) => {
                self.removed = error.removed;
                Err(error)
            }
        }
    }

    async fn native(
        &mut self,
        removing: bool,
        operation: impl FnOnce() -> Result<NativeCompletion> + Send + 'static,
    ) -> Result<NativeCompletion> {
        self.worker_removing = removing;
        self.worker = Some(tokio::task::spawn_blocking(operation));
        self.join_worker()
            .await?
            .ok_or_else(|| self.error(RemovalCause::Invalid("missing native worker")))
    }

    async fn append(&mut self, operation: SyncOp) -> Result<()> {
        let writer = self.writer.take();
        self.plan_worker = Some(tokio::spawn(async move {
            let mut writer = match writer {
                Some(writer) => writer,
                None => Box::new(PlanJournal::new().await?),
            };
            writer.append(&operation).await?;
            Ok((PlanOwner::Writer(writer), None))
        }));
        self.join_plan().await?;
        self.terminal = false;
        Ok(())
    }

    async fn seal(&mut self, writer: Box<PlanJournal>) -> Result<()> {
        self.plan_worker = Some(tokio::spawn(async move {
            Ok((PlanOwner::Reader((*writer).seal().await?), None))
        }));
        self.join_plan().await?;
        Ok(())
    }

    async fn next(&mut self) -> Result<Option<(Entry, SourceDestinationProof)>> {
        let mut reader = self
            .reader
            .take()
            .ok_or_else(|| self.error(RemovalCause::Invalid("missing sealed journal")))?;
        self.plan_worker = Some(tokio::spawn(async move {
            let op = reader.next().await?;
            Ok((PlanOwner::Reader(reader), op))
        }));
        self.join_plan()
            .await?
            .map(decode_record)
            .transpose()
            .map_err(|e| self.error(e))
    }

    fn rewind(&mut self) -> Result<()> {
        self.reader
            .as_mut()
            .ok_or_else(|| SourceRemovalError {
                removed: self.removed,
                removal_uncertain: false,
                cause: RemovalCause::Invalid("missing sealed journal"),
            })?
            .rewind()
            .map_err(|e| self.error(e))
    }
}

impl SourceRemovalJournal {
    pub async fn append_published(
        &self,
        source: &Entry,
        receipt: &PublishedDestinationReceipt,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        state.join_worker().await?;
        if state.terminal {
            return Err(state.error(RemovalCause::Terminal));
        }
        // Validation errors and cancellation are terminal too, not just disk errors.
        state.terminal = true;
        validate_source(source).map_err(|e| state.error(e))?;
        receipt
            .validate_source_removal(source)
            .map_err(|e| state.error(e))?;
        let proof = receipt.publication().map_err(|e| state.error(e))?;
        let destination = destination_entry(proof.path.clone(), proof.kind, proof.identity);
        state
            .append(SyncOp::Update {
                source: source.clone(),
                destination,
            })
            .await
    }

    pub async fn append_existing(
        &self,
        source: &Entry,
        receipt: &VerifiedExistingDestinationReceipt,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        state.join_worker().await?;
        if state.terminal {
            return Err(state.error(RemovalCause::Terminal));
        }
        state.terminal = true;
        validate_source(source).map_err(|e| state.error(e))?;
        if source.kind != EntryKind::File
            || receipt.source_path() != &source.path
            || Some(receipt.source_identity()) != source.identity
        {
            return Err(state.error(RemovalCause::Invalid(
                "verified-existing receipt source mismatch",
            )));
        }
        let destination = destination_entry(
            receipt.destination_path().clone(),
            EntryKind::File,
            receipt.destination_identity(),
        );
        state
            .append(SyncOp::Unchanged {
                source: source.clone(),
                destination,
                comparison: ContentComparison::Blake3,
            })
            .await
    }

    /// Call only after ALL required controller/main-work finalization succeeds.
    /// Standalone and finalized group receipts share this exact gate, native
    /// continuation state, and run-wide confirmed-unlink count.
    /// All recorded destination and original source names pass before any unlink.
    /// Late native checks still detect races; failure can leave partial removal.
    /// If this future is cancelled, call close() to join all admitted work.
    pub async fn replay<F, Fut>(&self, source_root: SourceRoot, mut validate: F) -> Result<u64>
    where
        F: FnMut(SourceDestinationProof) -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let mut state = self.state.lock().await;
        state.join_worker().await?;
        if state.terminal {
            return Err(state.error(RemovalCause::Terminal));
        }
        state.terminal = true;
        let Some(writer) = state.writer.take() else {
            return Ok(0);
        };
        state.seal(writer).await?;
        // A complete authentication/eligibility/destination pass precedes effects.
        while let Some((_, proof)) = state.next().await? {
            validate(proof).await.map_err(|e| state.error(e))?;
        }
        state.rewind()?;
        let (mut native, _) = state
            .native(false, move || {
                NativeState::new(source_root).map(|native| (native, None))
            })
            .await?;
        while let Some((source, _)) = state.next().await? {
            (native, _) = state
                .native(false, move || {
                    native.validate(&source).map(|native| (native, None))
                })
                .await?;
        }
        state.rewind()?;
        // Each execution read checks the original seal again, not a gate checksum.
        while let Some((source, proof)) = state.next().await? {
            // The complete gate is not a snapshot: a later candidate's checks
            // may run after this destination was replaced or modified.
            let proof = if matches!(&proof, SourceDestinationProof::Existing { identity, .. }
                if Some(*identity) == source.identity)
            {
                let phase_source = source.clone();
                let (owner, proof) = state
                    .native(false, move || {
                        native.prepare_destination(&phase_source, proof)
                    })
                    .await?;
                native = owner;
                proof.ok_or_else(|| {
                    state.error(RemovalCause::Invalid("missing late destination proof"))
                })?
            } else {
                proof
            };
            validate(proof).await.map_err(|e| state.error(e))?;
            (native, _) = state
                .native(true, move || {
                    native.remove(&source).map(|native| (native, None))
                })
                .await?;
        }
        // A last admitted unlink can complete after cutoff. Keep its confirmed
        // count visible rather than reporting a cancelled removal as retention
        // or discarding its outcome just because no later candidate exists.
        if native.root.rooted().mutation_admission_closed() {
            return Err(native.error(RemovalCause::Terminal, false));
        }
        Ok(native.removed)
    }

    /// Terminally close admission and join work retained by a cancelled operation.
    /// Drop cannot await native syscalls; early-exit owners must call this method.
    pub async fn close(&self) -> Result<u64> {
        let mut state = self.state.lock().await;
        state.terminal = true;
        state.join_worker().await?;
        Ok(state.removed)
    }
}

fn validate_source(source: &Entry) -> std::result::Result<(), RemovalCause> {
    if source.is_directory() || source.identity.is_none() {
        return Err(RemovalCause::Invalid(
            "source removal requires an observed file or symlink",
        ));
    }
    Ok(())
}

fn destination_entry(path: RelativePath, kind: EntryKind, identity: EntryIdentity) -> Entry {
    let mut entry = Entry::file(path, 0, Timestamp::UNIX_EPOCH);
    entry.kind = kind;
    entry.identity = Some(identity);
    entry
}

fn decode_record(op: SyncOp) -> std::result::Result<(Entry, SourceDestinationProof), RemovalCause> {
    let (source, destination, published) = match op {
        SyncOp::Update {
            source,
            destination,
        } => (source, destination, true),
        SyncOp::Unchanged {
            source,
            destination,
            comparison: ContentComparison::Blake3,
        } => (source, destination, false),
        _ => {
            return Err(RemovalCause::Invalid(
                "invalid authenticated removal record",
            ))
        }
    };
    validate_source(&source)?;
    let identity = destination
        .identity
        .ok_or(RemovalCause::Invalid("missing destination identity"))?;
    if destination.kind != source.kind || (!published && !source.is_file()) {
        return Err(RemovalCause::Invalid("destination kind mismatch"));
    }
    let proof = if published {
        SourceDestinationProof::Published(PublishedEntryProof {
            path: destination.path,
            kind: destination.kind,
            identity,
        })
    } else {
        SourceDestinationProof::Existing {
            source_path: source.path.clone(),
            path: destination.path,
            identity,
            own_source_effect: None,
        }
    };
    Ok((source, proof))
}

struct NativeState {
    root: SourceRoot,
    states: StateIndex,
    removed: u64,
}

impl NativeState {
    fn error(&self, cause: impl Into<RemovalCause>, removal_uncertain: bool) -> SourceRemovalError {
        SourceRemovalError {
            removed: self.removed,
            removal_uncertain,
            cause: cause.into(),
        }
    }

    fn new(root: SourceRoot) -> Result<Self> {
        let fail = |cause| SourceRemovalError {
            removed: 0,
            removal_uncertain: false,
            cause,
        };
        root.validate_blocking().map_err(|e| fail(e.into()))?;
        let states = StateIndex::new().map_err(|e| fail(e.into()))?;
        Ok(Self {
            root,
            states,
            removed: 0,
        })
    }

    fn validate(self, source: &Entry) -> Result<Self> {
        self.root
            .validate_blocking()
            .map_err(|e| self.error(e, false))?;
        let rooted = self.root.rooted();
        if source.is_file() {
            rooted
                .validate_hardlink_source_member_blocking(
                    &source.path,
                    source.identity.ok_or_else(|| {
                        self.error(RemovalCause::Invalid("missing source identity"), false)
                    })?,
                )
                .map_err(|e| self.error(e, false))?;
        } else {
            existing::validate_path(&rooted, source).map_err(|e| self.error(e, false))?;
        }
        self.root
            .validate_blocking()
            .map_err(|e| self.error(e, false))?;
        Ok(self)
    }

    fn prepare_destination(
        mut self,
        source: &Entry,
        mut proof: SourceDestinationProof,
    ) -> Result<NativeCompletion> {
        if let SourceDestinationProof::Existing {
            identity,
            own_source_effect,
            ..
        } = &mut proof
        {
            let original_identity = source.identity.ok_or_else(|| {
                self.error(RemovalCause::Invalid("missing source identity"), false)
            })?;
            if *identity == original_identity {
                *own_source_effect = self
                    .states
                    .get(*original_identity.as_bytes())
                    .map_err(|e| self.error(e, false))?
                    .map(|current| OwnSourceEffect {
                        original_identity,
                        current,
                    });
            }
        }
        Ok((self, Some(proof)))
    }

    fn remove(mut self, source: &Entry) -> Result<Self> {
        self.root
            .validate_blocking()
            .map_err(|e| self.error(e, false))?;
        let next = self.removed.checked_add(1).ok_or_else(|| {
            self.error(
                RemovalCause::Invalid("source removal count overflow"),
                false,
            )
        })?;
        let rooted = self.root.rooted();
        if source.is_file() {
            let identity = source.identity.ok_or_else(|| {
                self.error(RemovalCause::Invalid("missing source identity"), false)
            })?;
            let expected = self
                .states
                .get(*identity.as_bytes())
                .map_err(|e| self.error(e, false))?;
            let updated = rooted
                .remove_hardlink_source_member_blocking(&source.path, identity, expected.as_ref())
                .map_err(|e| {
                    let uncertain = !matches!(e, RootedFsError::CommitCancelled);
                    self.error(e, uncertain)
                })?;
            // Count the native effect BEFORE any fallible continuation-state I/O.
            self.removed = next;
            self.states
                .set(*identity.as_bytes(), updated)
                .map_err(|e| self.error(e, false))?;
        } else {
            existing::remove_observed_source_blocking(&rooted, source).map_err(|e| {
                let uncertain = !matches!(
                    e,
                    ExistingDestinationError::Rooted(RootedFsError::CommitCancelled)
                );
                self.error(e, uncertain)
            })?;
            self.removed = next;
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests;
