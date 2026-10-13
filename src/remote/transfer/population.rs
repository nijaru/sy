//! Receive and verify private bytes without admitting destination publication.
use super::{
    checked_output_size, require_empty_flags, require_stream, validate_basis,
    validate_destination_state, RemoteTransferError, Result,
};
use crate::endpoint::io::Preservation;
use crate::endpoint::publication::PublicationAdmission;
use crate::endpoint::ExpectedDestination;
use crate::engine::domain::{EntryIdentity, RelativePath, Timestamp};
use crate::engine::work::TransferSummary;
use crate::protocol::{
    Frame, FrameKind, ProtocolError, WireDeltaCopy, WireFileBasis, WireFileBegin, WireFileEnd,
};
use crate::remote::data::DataChunk;
use crate::remote::router::StreamInbox;
use crate::rooted_fs::{RootedFs, RootedStagedFile};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const RECONSTRUCTION_QUEUE_DEPTH: usize = 8;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

pub(super) enum ReconstructionOp {
    Data(DataChunk),
    Copy(WireDeltaCopy),
    Xattrs(Vec<(OsString, Vec<u8>)>),
    Acls(String),
    End(WireFileEnd),
}

pub(in crate::remote) struct PreparedReconstruction {
    pub(super) staged: RootedStagedFile,
    /// Opened only on the first Copy, never for an all-literal replacement.
    basis: Option<File>,
    /// PULL signatures and COPY share the original OLD description and parent.
    pull_basis: Option<crate::rooted_fs::RootedOldFile>,
    retained_flags: Option<u32>,
    rooted: RootedFs,
    relative: RelativePath,
    destination: Option<WireFileBasis>,
    copy_basis: Option<WireFileBasis>,
    preservation: Option<Preservation>,
}

/// Owns the verified original creator FD and any opened COPY basis. No member
/// has been published and no publication admission has been requested.
pub(in crate::remote) struct ReadyReconstruction {
    prepared: PreparedReconstruction,
    summary: TransferSummary,
    final_flags: Option<u32>,
}

impl PreparedReconstruction {
    fn validate_blocking(&self) -> Result<()> {
        if let Some(binding) = &self.pull_basis {
            self.rooted.verify_root_path_blocking()?;
            self.rooted
                .validate_old_file_parent_blocking(&self.relative, binding)?;
            let expected = self
                .copy_basis
                .ok_or(RemoteTransferError::CopyWithoutBasis)?;
            super::validate_source(
                binding.file(),
                EntryIdentity::from_bytes(expected.identity()),
                expected.file_size(),
            )?;
        }
        validate_destination_state(&self.rooted, &self.relative, self.destination)?;
        if let Some(basis) = self.basis.as_ref() {
            validate_basis(
                &self.rooted,
                basis,
                &self.relative,
                self.copy_basis
                    .ok_or(RemoteTransferError::CopyWithoutBasis)?,
            )?;
        }
        Ok(())
    }
}

impl ReadyReconstruction {
    /// Revalidate the original destination/basis observation before handing the
    /// original stage to its native publication owner (ordinary file or group).
    /// These checks are observational, not namespace compare-and-swap. The
    /// native owner still validates its seal and acquires publication admission.
    pub(in crate::remote) fn into_staged_blocking(
        self,
    ) -> Result<(RootedStagedFile, TransferSummary, Option<u32>)> {
        if let Err(operation) = self.prepared.validate_blocking() {
            return match self.abort_blocking() {
                Ok(()) => Err(operation),
                Err(abort) => Err(RemoteTransferError::ReconstructionAbortFailed {
                    operation: Box::new(operation),
                    abort: Box::new(abort),
                }),
            };
        }
        Ok((self.prepared.staged, self.summary, self.final_flags))
    }

    /// Abort owned private staging without attempting publication validation.
    pub(in crate::remote) fn abort_blocking(self) -> Result<()> {
        Ok(self.prepared.staged.abort()?)
    }

    pub(super) fn commit_blocking(
        self,
        admission: &Arc<PublicationAdmission>,
    ) -> Result<(TransferSummary, crate::rooted_fs::PublishedFileProof)> {
        let (staged, summary, final_flags) = self.into_staged_blocking()?;
        let published = staged.commit_with_admission(admission)?;
        let proof = published.finalize_blocking(final_flags)?;
        Ok((summary, proof))
    }
}

pub(super) fn prepare_reconstruction(
    rooted: RootedFs,
    relative: &RelativePath,
    expectation: Option<WireFileBasis>,
) -> Result<PreparedReconstruction> {
    prepare_reconstruction_with_stage_blocking(
        rooted,
        relative,
        expectation,
        |rooted, path, expected| {
            Ok(rooted.begin_staged_file_with_expectation_blocking(path, expected)?)
        },
    )
}

/// Prepare against the original scanned destination, using one native stage.
///
/// The callback runs on the blocking worker after destination validation. It
/// must return the original empty private stage created by this root for this
/// path/expectation, not a reopened or cloned replacement transaction. A group
/// owner may supply its already-created stage with retained native metadata.
/// The caller must still carry intended mode/time in FileBegin: native writes
/// change mtime and can clear set-ID bits. Absent wire metadata requests leave
/// those native effects untouched, just as for an ordinary file transfer.
/// No filesystem path is re-resolved to reopen the session root.
pub(in crate::remote) fn prepare_reconstruction_with_stage_blocking(
    rooted: RootedFs,
    relative: &RelativePath,
    expectation: Option<WireFileBasis>,
    prepare_stage: impl FnOnce(
        &RootedFs,
        &RelativePath,
        ExpectedDestination,
    ) -> Result<RootedStagedFile>,
) -> Result<PreparedReconstruction> {
    prepare_with_metadata_blocking(
        rooted,
        relative,
        expectation,
        expectation,
        None,
        prepare_stage,
    )
}

/// A group declares its target identity independently of an optional regular
/// COPY basis. The size in `destination` is not consulted: only COPY's size
/// bounds reconstruction reads. `Some(preservation)` closes the metadata phase,
/// including when neither optional metadata field was requested.
/// The callback has the same original-stage contract as ordinary preparation.
pub(in crate::remote) fn prepare_group_reconstruction_blocking(
    rooted: RootedFs,
    relative: &RelativePath,
    destination: Option<WireFileBasis>,
    copy_basis: Option<WireFileBasis>,
    preservation: Preservation,
    prepare_stage: impl FnOnce(
        &RootedFs,
        &RelativePath,
        ExpectedDestination,
    ) -> Result<RootedStagedFile>,
) -> Result<PreparedReconstruction> {
    prepare_with_metadata_blocking(
        rooted,
        relative,
        destination,
        copy_basis,
        Some(preservation),
        prepare_stage,
    )
}

fn prepare_with_metadata_blocking(
    rooted: RootedFs,
    relative: &RelativePath,
    destination: Option<WireFileBasis>,
    copy_basis: Option<WireFileBasis>,
    preservation: Option<Preservation>,
    prepare_stage: impl FnOnce(
        &RootedFs,
        &RelativePath,
        ExpectedDestination,
    ) -> Result<RootedStagedFile>,
) -> Result<PreparedReconstruction> {
    validate_destination_state(&rooted, relative, destination)?;
    let expected = destination.map_or(ExpectedDestination::Absent, |expected| {
        ExpectedDestination::Unchanged(EntryIdentity::from_bytes(expected.identity()))
    });
    let mut staged = prepare_stage(&rooted, relative, expected)?;
    if staged.file_mut().metadata()?.len() != 0 {
        return Err(RemoteTransferError::NonemptyReconstructionStage);
    }
    staged.file_mut().rewind()?;
    Ok(PreparedReconstruction {
        staged,
        basis: None,
        pull_basis: None,
        retained_flags: None,
        rooted,
        relative: relative.clone(),
        destination,
        copy_basis,
        preservation,
    })
}

/// Prepare PULL with the genuine OLD FD before signing. Never reopen COPY's
/// basis after the signature pass or adopt a fresh OLD observation.
pub(in crate::remote) fn prepare_pull_reconstruction_blocking(
    rooted: RootedFs,
    relative: &RelativePath,
    expected: WireFileBasis,
    overrides: crate::endpoint::io::PreservationRequest,
    source_flags: bool,
) -> Result<PreparedReconstruction> {
    rooted.verify_root_path_blocking()?;
    let binding = rooted
        .capture_old_file_blocking(relative, EntryIdentity::from_bytes(expected.identity()))?;
    super::validate_source(
        binding.file(),
        EntryIdentity::from_bytes(expected.identity()),
        expected.file_size(),
    )?;
    let reader = binding.file().try_clone()?;
    let mut prepared = prepare_reconstruction(rooted, relative, Some(expected))?;
    prepared
        .staged
        .copy_retained_metadata_blocking(binding.file(), overrides)?;
    if cfg!(target_os = "macos") && !source_flags {
        prepared.retained_flags = Some(
            prepared
                .rooted
                .read_open_file_bsd_flags_blocking(binding.file())?,
        );
    }
    prepared.basis = Some(reader);
    prepared.pull_basis = Some(binding);
    prepared.validate_blocking()?;
    Ok(prepared)
}

impl PreparedReconstruction {
    pub(in crate::remote) fn produce_pull_signatures_blocking(
        mut self,
        block_size: crate::protocol::SignatureBlockSize,
        sender: mpsc::Sender<crate::protocol::WireSignature>,
    ) -> Result<(Self, crate::protocol::WireSignatureEnd)> {
        let produced = (|| {
            self.validate_blocking()?;
            let basis = self
                .basis
                .as_mut()
                .ok_or(RemoteTransferError::CopyWithoutBasis)?;
            basis.rewind()?;
            let (size, count) =
                crate::remote::signature::produce_reader_signatures(basis, block_size, sender)
                    .map_err(std::io::Error::other)?;
            self.validate_blocking()?;
            let expected = self
                .copy_basis
                .ok_or(RemoteTransferError::CopyWithoutBasis)?;
            if size != expected.file_size() {
                return Err(RemoteTransferError::BasisChanged {
                    expected_size: expected.file_size(),
                    actual_size: size,
                });
            }
            Ok(crate::protocol::WireSignatureEnd::new(
                size,
                count,
                expected.identity(),
            )?)
        })();
        match produced {
            Ok(end) => Ok((self, end)),
            Err(operation) => match self.abort_blocking() {
                Ok(()) => Err(operation),
                Err(abort) => Err(RemoteTransferError::ReconstructionAbortFailed {
                    operation: Box::new(operation),
                    abort: Box::new(abort),
                }),
            },
        }
    }

    pub(in crate::remote) fn abort_blocking(self) -> Result<()> {
        Ok(self.staged.abort()?)
    }
}

/// Bounded receiver and blocking-writer ownership across awaits. `receive`
/// joins on every ordinary outcome. If its future is cancelled, retain this
/// owner and call `abort` before releasing the handler; do not resume parsing
/// after cancellation (a frame may already have been consumed).
///
/// Arbitrary task abort/Drop cannot await native work. The session must drain
/// handlers, as for other native workers, rather than aborting their tasks.
pub(in crate::remote) struct ReconstructionReceiver {
    begin: WireFileBegin,
    sender: Option<mpsc::Sender<ReconstructionOp>>,
    worker: Option<JoinHandle<Result<ReadyReconstruction>>>,
    cleanup: Option<JoinHandle<Result<()>>>,
    metadata_captured: bool,
    pull_preservation: Option<crate::remote::fetch::FetchPreservationRequest>,
    rate_limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
}

impl ReconstructionReceiver {
    pub(in crate::remote) fn start(prepared: PreparedReconstruction, begin: WireFileBegin) -> Self {
        let (sender, receiver) = mpsc::channel(RECONSTRUCTION_QUEUE_DEPTH);
        let metadata_captured = prepared.preservation.is_some();
        let worker_begin = begin.clone();
        let worker = tokio::task::spawn_blocking(move || {
            populate_reconstruction(prepared, worker_begin, receiver)
        });
        Self {
            begin,
            sender: Some(sender),
            worker: Some(worker),
            cleanup: None,
            metadata_captured,
            pull_preservation: None,
            rate_limiter: None,
        }
    }

    pub(in crate::remote) fn with_pull_policy(
        mut self,
        preservation: crate::remote::fetch::FetchPreservationRequest,
        rate_limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    ) -> Self {
        self.pull_preservation = Some(preservation);
        self.rate_limiter = rate_limiter;
        self
    }

    /// Consume the existing file payload through FileEnd, returning private
    /// readiness, not an Ack or publication. The caller validates FileEnd's
    /// lifecycle flags explicitly: an ordinary stream requires FINAL, whereas
    /// a group must keep its inbox open. This function never drops that inbox.
    pub(in crate::remote) async fn receive(
        &mut self,
        inbox: &mut StreamInbox,
        validate_end: impl Fn(&Frame) -> Result<()>,
    ) -> Result<ReadyReconstruction> {
        let worker = self
            .worker
            .as_mut()
            .ok_or(RemoteTransferError::ReconstructionStopped)?;
        let sender = self
            .sender
            .as_ref()
            .ok_or(RemoteTransferError::ReconstructionStopped)?;
        // Observe worker failure while waiting for peer input; a rejected
        // chunk must not require another frame to wake the receiver.
        let received = tokio::select! {
            received = receive_reconstruction(inbox, &self.begin, sender, self.metadata_captured, self.pull_preservation, self.rate_limiter.as_deref(), validate_end) => received,
            reconstructed = worker => {
                self.sender = None;
                self.worker = None;
                return reconstructed
                    .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))?;
            }
        };
        self.sender = None;
        match received {
            Ok(()) => self.join().await,
            Err(error) => match (error, self.abort().await) {
                (RemoteTransferError::ReconstructionStopped, Err(error)) => Err(error),
                (operation, Err(abort)) => Err(RemoteTransferError::ReconstructionAbortFailed {
                    operation: Box::new(operation),
                    abort: Box::new(abort),
                }),
                (error, Ok(())) => Err(error),
            },
        }
    }

    /// Close the bounded queue and join before returning, including when a
    /// cancelled receive left a backpressured writer. Successful readiness is
    /// aborted on a blocking worker, retaining its join handle across cancelled
    /// abort waits as well. Cleanup failures remain typed rather than hidden by
    /// dropping readiness on a Tokio runtime thread.
    pub(in crate::remote) async fn abort(&mut self) -> Result<()> {
        self.sender = None;
        if self.worker.is_some() {
            match self.join().await {
                Ok(ready) => {
                    self.cleanup =
                        Some(tokio::task::spawn_blocking(move || ready.abort_blocking()));
                }
                Err(RemoteTransferError::MissingFileEnd) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        let Some(cleanup) = self.cleanup.as_mut() else {
            return Ok(());
        };
        let result = cleanup.await;
        self.cleanup = None;
        result.map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))?
    }

    async fn join(&mut self) -> Result<ReadyReconstruction> {
        let result = self
            .worker
            .as_mut()
            .ok_or(RemoteTransferError::ReconstructionStopped)?
            .await;
        self.worker = None;
        result.map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))?
    }
}

async fn receive_reconstruction(
    inbox: &mut StreamInbox,
    begin: &WireFileBegin,
    reconstruction_tx: &mpsc::Sender<ReconstructionOp>,
    metadata_captured: bool,
    pull_preservation: Option<crate::remote::fetch::FetchPreservationRequest>,
    rate_limiter: Option<&std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>,
    validate_end: impl Fn(&Frame) -> Result<()>,
) -> Result<()> {
    let stream_id = inbox.stream_id();
    let mut seen_xattrs = metadata_captured;
    let mut seen_acls = metadata_captured;
    // PULL sends its requested preservation before operations. PUSH captures
    // SOURCE preservation after byte production and sends it before FileEnd.
    let mut data_started = false;
    loop {
        let routed = inbox
            .recv()
            .await?
            .ok_or(RemoteTransferError::UnexpectedStreamEnd {
                stream_id: stream_id.get(),
            })?;
        let frame = routed.frame();
        require_stream(frame, stream_id)?;
        let op = match frame.kind() {
            FrameKind::Data => {
                data_started = true;
                let chunk = DataChunk::from_frame(frame)?;
                ReconstructionOp::Data(if rate_limiter.is_some() {
                    DataChunk::Plain(chunk.decode().await?)
                } else {
                    chunk
                })
            }
            FrameKind::DeltaCopy => {
                data_started = true;
                require_empty_flags(frame)?;
                if begin.basis().is_none() {
                    return Err(RemoteTransferError::CopyWithoutBasis);
                }
                ReconstructionOp::Copy(WireDeltaCopy::decode(frame.payload())?)
            }
            FrameKind::FileXattrs => {
                require_empty_flags(frame)?;
                if seen_xattrs
                    || (data_started && pull_preservation.is_some())
                    || pull_preservation.is_some_and(|request| !request.xattrs)
                {
                    return Err(RemoteTransferError::UnexpectedFrame {
                        expected: FrameKind::FileEnd,
                        actual: FrameKind::FileXattrs,
                    });
                }
                seen_xattrs = true;
                let entries = crate::protocol::WireXattrResult::decode(frame.payload())?;
                let xattrs = entries
                    .entries()
                    .iter()
                    .map(|entry| {
                        Ok((
                            crate::engine::native_path::decode(entry.name())?.into_os_string(),
                            entry.value().to_vec(),
                        ))
                    })
                    .collect::<std::result::Result<Vec<(OsString, Vec<u8>)>, std::io::Error>>()?;
                ReconstructionOp::Xattrs(xattrs)
            }
            FrameKind::FileAcls => {
                require_empty_flags(frame)?;
                if seen_acls
                    || (data_started && pull_preservation.is_some())
                    || pull_preservation.is_some_and(|request| !request.acls)
                {
                    return Err(RemoteTransferError::UnexpectedFrame {
                        expected: FrameKind::FileEnd,
                        actual: FrameKind::FileAcls,
                    });
                }
                seen_acls = true;
                ReconstructionOp::Acls(
                    crate::protocol::WireAcl::decode(frame.payload())?
                        .text()
                        .to_string(),
                )
            }
            FrameKind::FileEnd => {
                if let Some(request) = pull_preservation {
                    if request.xattrs && !seen_xattrs {
                        return Err(RemoteTransferError::MissingFetchPreservation("xattrs"));
                    }
                    if request.acls && !seen_acls {
                        return Err(RemoteTransferError::MissingFetchPreservation("ACLs"));
                    }
                }
                validate_end(frame)?;
                ReconstructionOp::End(WireFileEnd::decode(frame.payload())?)
            }
            actual => {
                return Err(RemoteTransferError::UnexpectedFrame {
                    expected: FrameKind::FileEnd,
                    actual,
                })
            }
        };
        if let Some(limiter) = rate_limiter {
            let bytes = match &op {
                ReconstructionOp::Data(DataChunk::Plain(bytes)) => bytes.len() as u64,
                _ => 0,
            };
            let sleep = limiter
                .lock()
                .map_err(|_| std::io::Error::other("rate limiter poisoned"))?
                .consume(bytes);
            if !sleep.is_zero() {
                tokio::time::sleep(sleep).await;
            }
        }
        let is_end = matches!(op, ReconstructionOp::End(_));
        reconstruction_tx
            .send(op)
            .await
            .map_err(|_| RemoteTransferError::ReconstructionStopped)?;
        drop(routed);
        if is_end {
            return Ok(());
        }
    }
}

pub(super) fn populate_reconstruction(
    mut prepared: PreparedReconstruction,
    begin: WireFileBegin,
    mut receiver: mpsc::Receiver<ReconstructionOp>,
) -> Result<ReadyReconstruction> {
    let mut hasher = blake3::Hasher::new();
    let mut file_size = 0_u64;
    let mut literal_bytes = 0_u64;
    let mut reused_bytes = 0_u64;
    let preservation = prepared.preservation.take().unwrap_or_default();
    let mut xattrs = preservation.xattrs;
    let mut acls = preservation.acl;

    while let Some(op) = receiver.blocking_recv() {
        match op {
            ReconstructionOp::Data(chunk) => {
                let bytes = chunk.decode_blocking()?;
                file_size = checked_output_size(file_size, bytes.len(), begin.file_size())?;
                prepared.staged.file_mut().write_all(&bytes)?;
                hasher.update(&bytes);
                literal_bytes = literal_bytes
                    .checked_add(
                        u64::try_from(bytes.len())
                            .map_err(|_| RemoteTransferError::ByteCountOverflow)?,
                    )
                    .ok_or(RemoteTransferError::ByteCountOverflow)?;
            }
            ReconstructionOp::Copy(copy) => {
                let expected = prepared
                    .copy_basis
                    .ok_or(RemoteTransferError::CopyWithoutBasis)?;
                if prepared.basis.is_none() {
                    let file = prepared.rooted.open_regular_blocking(&prepared.relative)?;
                    validate_basis(&prepared.rooted, &file, &prepared.relative, expected)?;
                    prepared.basis = Some(file);
                }
                let basis = prepared
                    .basis
                    .as_mut()
                    .ok_or(RemoteTransferError::CopyWithoutBasis)?;
                file_size = checked_output_size(
                    file_size,
                    usize::try_from(copy.copy_len())
                        .map_err(|_| RemoteTransferError::ByteCountOverflow)?,
                    begin.file_size(),
                )?;
                copy_basis_range(
                    basis,
                    expected,
                    copy,
                    prepared.staged.file_mut(),
                    &mut hasher,
                )?;
                reused_bytes = reused_bytes
                    .checked_add(u64::from(copy.copy_len()))
                    .ok_or(RemoteTransferError::ByteCountOverflow)?;
            }
            ReconstructionOp::Xattrs(values) => xattrs = Some(values),
            ReconstructionOp::Acls(text) => acls = Some(text),
            ReconstructionOp::End(end) => {
                if end.file_size() != begin.file_size() || file_size != begin.file_size() {
                    return Err(RemoteTransferError::SizeMismatch {
                        expected_size: begin.file_size(),
                        announced_size: end.file_size(),
                        actual_size: file_size,
                    });
                }
                let digest = *hasher.finalize().as_bytes();
                if digest != end.digest() {
                    return Err(RemoteTransferError::DigestMismatch);
                }
                let modified = begin
                    .modified()
                    .map(|(seconds, nanoseconds)| Timestamp::new(seconds, nanoseconds))
                    .transpose()
                    .map_err(|_| ProtocolError::InvalidField {
                        field: "modified_nanoseconds",
                        reason: "nanoseconds must be below 1,000,000,000",
                    })?;
                // Native owner preserves xattr -> mode/time -> ACL -> mode proof.
                // Flags remain context for post-publication finalization only.
                prepared.staged.apply_preservation_blocking(
                    begin.unix_mode(),
                    modified,
                    xattrs.as_deref(),
                    acls.as_deref(),
                )?;
                if prepared.staged.staged_hash_blocking()?.as_bytes() != &digest {
                    return Err(RemoteTransferError::DigestMismatch);
                }
                prepared.validate_blocking()?;
                let final_flags = begin.bsd_flags().or(prepared.retained_flags);
                return Ok(ReadyReconstruction {
                    prepared,
                    summary: TransferSummary {
                        file_size,
                        digest,
                        literal_bytes,
                        reused_bytes,
                    },
                    final_flags,
                });
            }
        }
    }
    Err(RemoteTransferError::MissingFileEnd)
}

#[cfg(test)]
pub(super) fn reconstruct_file(
    prepared: PreparedReconstruction,
    begin: WireFileBegin,
    receiver: mpsc::Receiver<ReconstructionOp>,
    admission: &Arc<PublicationAdmission>,
) -> Result<(TransferSummary, crate::rooted_fs::PublishedFileProof)> {
    populate_reconstruction(prepared, begin, receiver)?.commit_blocking(admission)
}

fn copy_basis_range(
    basis: &mut File,
    expected: WireFileBasis,
    copy: WireDeltaCopy,
    destination: &mut File,
    hasher: &mut blake3::Hasher,
) -> Result<()> {
    let end = copy.end()?;
    if end > expected.file_size() {
        return Err(RemoteTransferError::CopyOutOfBounds {
            offset: copy.basis_offset(),
            end,
            basis_size: expected.file_size(),
        });
    }
    basis.seek(SeekFrom::Start(copy.basis_offset()))?;
    let mut remaining =
        usize::try_from(copy.copy_len()).map_err(|_| RemoteTransferError::ByteCountOverflow)?;
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];
    while remaining != 0 {
        let take = remaining.min(buffer.len());
        basis.read_exact(&mut buffer[..take])?;
        destination.write_all(&buffer[..take])?;
        hasher.update(&buffer[..take]);
        remaining -= take;
    }
    Ok(())
}
