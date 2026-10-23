use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::engine::reconcile::BoxError;
use crate::engine::work::TransferSummary;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireData, WireDeltaCopy,
    WireFileBasis, WireFileBegin, WireFileEnd, MAX_TRANSFER_DATA_SIZE,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::remote::source_file::{SourceCompletionError, SourceFile, SourceMetadataRequest};
use crate::rooted_fs::{RootedFs, RootedFsError};
use crate::transfer::delta::{match_delta, BasisIndex, DeltaMatchError, DeltaOp};
use bytes::Bytes;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
#[cfg(test)]
use std::io::{Seek, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub(crate) const PRODUCER_QUEUE_DEPTH: usize = 8;
pub(super) mod population;
use population::{prepare_reconstruction, ReconstructionReceiver};
#[cfg(test)]
use population::{reconstruct_file, ReconstructionOp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferPreservationRequest {
    /// Capture xattrs from the held source file used for the byte stream.
    pub xattrs: bool,
    /// Capture ACLs from the held source file used for the byte stream.
    pub acls: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TransferStreamPolicy {
    pub preservation: TransferPreservationRequest,
    pub compression: Option<crate::engine::compression::CompressionPolicy>,
    pub final_flags: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransferMetadata {
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
    /// Complete xattr set applied to staging before commit when `-X` is on.
    pub xattrs: Option<Vec<(OsString, Vec<u8>)>>,
    /// Complete exacl-unified ACL text applied to staging before commit when
    /// `-A` is on (empty text clears).
    pub acls: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteTransferError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error(transparent)]
    Local(#[from] crate::error::SyncError),

    #[error(transparent)]
    Delta(#[from] DeltaMatchError),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("file transfer requires a regular-file source entry")]
    InvalidSource,

    #[error("compressed Data payload failed to decompress: {0}")]
    Decompression(String),

    #[error("file transfer requires a scanned source identity")]
    MissingSourceIdentity,

    #[error("peer did not negotiate transaction-bound {feature} preservation for file transfer")]
    PreservationUnavailable { feature: &'static str },

    #[error("file fetch omitted requested {0} preservation metadata")]
    MissingFetchPreservation(&'static str),

    #[error("peer cancelled file fetch stream {stream_id} before commit acknowledgement")]
    FetchCancelled { stream_id: u32 },

    #[error("file fetch supplied unrequested {0} preservation metadata")]
    UnexpectedFetchPreservation(&'static str),

    #[error("delta transfer requires a regular-file basis at the destination path")]
    InvalidBasis,

    #[error("invalid delta PULL signature phase: {0}")]
    InvalidDeltaSignatures(&'static str),

    #[error("file fetch received premature {0:?} before its completion footer")]
    PrematureFetchFrame(FrameKind),

    #[error(
        "opened source changed since scan (expected {expected_size} bytes, observed {actual_size} bytes)"
    )]
    SourceChanged {
        expected_size: u64,
        actual_size: u64,
    },

    #[error(
        "opened destination basis changed since scan (expected {expected_size} bytes, observed {actual_size} bytes)"
    )]
    BasisChanged {
        expected_size: u64,
        actual_size: u64,
    },

    #[error("destination entry appeared during transfer: {path} must remain absent until commit")]
    UnexpectedDestination { path: PathBuf },

    #[error("destination changed since scan: {path} was replaced or modified before commit")]
    DestinationChanged { path: PathBuf },

    #[error("opened file did not provide a stable endpoint identity")]
    MissingOpenedIdentity,

    #[error("file transfer producer task failed: {0}")]
    ProducerJoin(String),

    #[error("file reconstruction worker failed: {0}")]
    ReconstructionJoin(String),

    #[error("file reconstruction worker stopped before transfer completion")]
    ReconstructionStopped,

    #[error(
        "file reconstruction failed ({operation}); private staging abort also failed ({abort})"
    )]
    ReconstructionAbortFailed {
        operation: Box<RemoteTransferError>,
        abort: Box<RemoteTransferError>,
    },

    #[error("file reconstruction requires an empty private staging file")]
    NonemptyReconstructionStage,

    #[error("file transfer stream {stream_id} ended before FileEnd")]
    UnexpectedStreamEnd { stream_id: u32 },

    #[error("expected file-transfer frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("file-transfer frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("file-transfer frame {kind:?} used unsupported flags 0x{flags:02x}")]
    FrameFlags { kind: FrameKind, flags: u8 },

    #[error("FileEnd must use FINAL|ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    FileEndFlags { flags: u8 },

    #[error("delta copy was received for a whole-file transfer")]
    CopyWithoutBasis,

    #[error("delta copy range {offset}..{end} exceeds basis size {basis_size}")]
    CopyOutOfBounds {
        offset: u64,
        end: u64,
        basis_size: u64,
    },

    #[error("reconstructed file exceeded announced size {expected_size}")]
    ReconstructionTooLarge { expected_size: u64 },

    #[error(
        "file size mismatch at transfer end: announced {announced_size}, reconstructed {actual_size}, expected {expected_size}"
    )]
    SizeMismatch {
        expected_size: u64,
        announced_size: u64,
        actual_size: u64,
    },

    #[error("reconstructed file digest does not match FileEnd digest")]
    DigestMismatch,

    #[error("file transfer ended without FileEnd")]
    MissingFileEnd,

    #[error("file-transfer byte count overflow")]
    ByteCountOverflow,

    #[error("pulled source digest mismatch: server reported {expected:02x?}, staged bytes hash to {actual:02x?}")]
    FetchDigestMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
}

impl From<SharedRouterError> for RemoteTransferError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteTransferError>;

pub(crate) enum ProducerItem {
    Data(Bytes),
    /// Compressed with zstd level `ZSTD_FAST_LEVEL`; the frame loop must set
    /// `FrameFlags::COMPRESSED` so the receiver knows to decompress.
    CompressedData(Bytes),
    Copy(WireDeltaCopy),
}

/// Prove the destination still carries the state the client scanned.
///
/// `None` expectation is a create: the destination must remain absent. This is
/// stat-based race detection over separate syscalls, with the same documented
/// limits as local transfers (see `endpoint::transfer`).
fn validate_destination_state(
    rooted: &RootedFs,
    relative: &RelativePath,
    expectation: Option<WireFileBasis>,
) -> Result<()> {
    let expected_identity = expectation
        .map(|expected| {
            rooted.retired_destination_identity_blocking(EntryIdentity::from_bytes(
                expected.identity(),
            ))
        })
        .transpose()?;
    let observed = rooted.path_identity_blocking(relative)?;
    let path = relative.as_path().to_path_buf();
    match expected_identity {
        Some(expected) => match observed {
            Some((_, identity)) if identity == expected => Ok(()),
            _ => Err(RemoteTransferError::DestinationChanged { path }),
        },
        None if observed.is_some() => Err(RemoteTransferError::UnexpectedDestination { path }),
        None => Ok(()),
    }
}

/// The scanned destination state one transfer commits against.
///
/// An absent expectation is a create: the receiver requires the explicit
/// destination path to remain absent. Updates carry the scanned identity
/// expectation so a racing replacement aborts staging before commit; delta
/// transfers add the signature index produced from that same destination entry.
#[derive(Debug)]
pub struct TransferDestination {
    path: RelativePath,
    expectation: Option<WireFileBasis>,
    delta_index: Option<BasisIndex>,
}

impl TransferDestination {
    pub fn create(path: RelativePath) -> Self {
        Self {
            path,
            expectation: None,
            delta_index: None,
        }
    }

    pub fn whole(path: RelativePath, expectation: WireFileBasis) -> Self {
        Self {
            path,
            expectation: Some(expectation),
            delta_index: None,
        }
    }

    pub fn delta(path: RelativePath, expectation: WireFileBasis, delta_index: BasisIndex) -> Self {
        Self {
            path,
            expectation: Some(expectation),
            delta_index: Some(delta_index),
        }
    }
}

pub async fn request_file_transfer(
    sender: &RouterSender,
    source_root: crate::endpoint::source_root::SourceRoot,
    source: Entry,
    destination: TransferDestination,
    peer: PlatformOs,
) -> Result<TransferSummary> {
    request_file_transfer_with_metadata(
        sender,
        source_root,
        source,
        destination,
        TransferMetadata::default(),
        peer,
    )
    .await
}

pub async fn request_file_transfer_with_metadata(
    sender: &RouterSender,
    source_root: crate::endpoint::source_root::SourceRoot,
    source: Entry,
    destination: TransferDestination,
    metadata: TransferMetadata,
    peer: PlatformOs,
) -> Result<TransferSummary> {
    request_file_transfer_with_policy(
        sender,
        source_root,
        source,
        destination,
        metadata,
        peer,
        None,
    )
    .await
}

pub async fn request_file_transfer_with_policy(
    sender: &RouterSender,
    source_root: crate::endpoint::source_root::SourceRoot,
    source: Entry,
    destination: TransferDestination,
    metadata: TransferMetadata,
    peer: PlatformOs,
    compression: Option<crate::engine::compression::CompressionPolicy>,
) -> Result<TransferSummary> {
    request_file_transfer_with_stream_policy(
        sender,
        source_root,
        source,
        destination,
        metadata,
        peer,
        TransferStreamPolicy {
            preservation: TransferPreservationRequest::default(),
            compression,
            final_flags: None,
        },
    )
    .await
    .map(|(summary, _)| summary)
}

pub async fn request_file_transfer_with_stream_policy(
    sender: &RouterSender,
    source_root: crate::endpoint::source_root::SourceRoot,
    source: Entry,
    destination: TransferDestination,
    metadata: TransferMetadata,
    peer: PlatformOs,
    stream_policy: TransferStreamPolicy,
) -> Result<(TransferSummary, crate::rooted_fs::PublishedFileProof)> {
    ensure_compatible_path_encoding(peer)?;
    if !source.is_file() {
        return Err(RemoteTransferError::InvalidSource);
    }
    let expected_identity = source
        .identity
        .ok_or(RemoteTransferError::MissingSourceIdentity)?;
    let expected_size = source.size;
    let encoded_path = encode_relative_path(destination.path.as_path())?;
    let published_path = destination.path;
    let basis_index = destination.delta_index;
    let begin = match destination.expectation {
        Some(expectation) => WireFileBegin::delta(encoded_path, expected_size, expectation),
        None => WireFileBegin::whole(encoded_path, expected_size),
    }
    .with_metadata(
        metadata.unix_mode,
        metadata
            .modified
            .map(|value| (value.seconds(), value.nanoseconds())),
    )?
    .with_bsd_flags(stream_policy.final_flags);
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    let begin = Frame::new(
        FrameKind::FileBegin,
        FrameFlags::empty(),
        stream_id,
        begin.encode(),
    )?;

    let preservation_request = stream_policy.preservation;
    let compression = stream_policy.compression;
    let (source, preservation) = tokio::task::spawn_blocking(move || {
        let source = SourceFile::capture_blocking(
            source_root.rooted(),
            source.path,
            expected_identity,
            expected_size,
        )?;
        let (preservation, _) = source.read_metadata_blocking(SourceMetadataRequest {
            xattrs: preservation_request.xattrs,
            acls: preservation_request.acls,
            bsd_flags: false,
        })?;
        Ok::<_, RemoteTransferError>((source, preservation))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;
    let mut metadata = metadata;
    if preservation_request.xattrs {
        metadata.xattrs = preservation.xattrs;
    }
    if preservation_request.acls {
        metadata.acls = preservation.acl;
    }

    let begun = sender.send(begin).await;
    // Even a failed FileBegin releases the native owner on joined blocking work.
    let (source, reader) = tokio::task::spawn_blocking(move || {
        begun?;
        let reader = source.clone_reader_blocking()?;
        Ok::<_, RemoteTransferError>((source, reader))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;
    let (producer_tx, producer_rx) = mpsc::channel(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        produce_source(
            reader,
            expected_identity,
            expected_size,
            basis_index,
            producer_tx,
            compression,
        )
    });

    let streamed: Result<TransferSummary> = async {
        let summary = send_produced_file(sender, stream_id, producer_rx, producer).await?;
        // Requested preservation shares the DATA owner and private transfer stream.
        if let Some(xattrs) = &metadata.xattrs {
            let entries = xattrs
                .iter()
                .map(|(name, value)| {
                    crate::protocol::WireXattr::new(
                        crate::remote::xattr::name_bytes(name),
                        value.clone(),
                    )
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let payload = crate::protocol::WireXattrResult::new(entries)?;
            sender
                .send(Frame::new(
                    FrameKind::FileXattrs,
                    FrameFlags::empty(),
                    stream_id,
                    payload.encode()?,
                )?)
                .await?;
        }
        if let Some(acls) = &metadata.acls {
            let payload = crate::protocol::WireAcl::new(acls.clone())?;
            sender
                .send(Frame::new(
                    FrameKind::FileAcls,
                    FrameFlags::empty(),
                    stream_id,
                    payload.encode(),
                )?)
                .await?;
        }
        Ok(summary)
    }
    .await;
    let (source, summary) = tokio::task::spawn_blocking(move || {
        let summary = streamed?;
        source.validate_blocking()?;
        Ok::<_, RemoteTransferError>((source, summary))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;
    let acknowledged: Result<_> = async {
        sender
            .send(Frame::new(
                FrameKind::FileEnd,
                FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                stream_id,
                WireFileEnd::new(summary.file_size, summary.digest).encode(),
            )?)
            .await?;
        receive_ack(&mut inbox, stream_id).await
    }
    .await;
    // Retain the DATA owner through receipt. A later SOURCE failure cannot turn
    // acknowledged publication into rollback or source-removal permission.
    tokio::task::spawn_blocking(move || {
        let ack = acknowledged?;
        source
            .validate_blocking()
            .map_err(|source| io::Error::other(SourceCompletionError { source }))?;
        Ok((
            summary,
            crate::rooted_fs::PublishedFileProof {
                path: published_path,
                identity: EntryIdentity::from_bytes(ack.identity()),
            },
        ))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))?
}

/// Own the bounded producer queue and join its blocking worker on every send
/// outcome. Closing the receiver before joining wakes a backpressured producer
/// when the transport fails instead of leaving it detached.
pub(super) async fn send_produced_file(
    sender: &RouterSender,
    stream_id: StreamId,
    producer_rx: mpsc::Receiver<ProducerItem>,
    producer: tokio::task::JoinHandle<Result<TransferSummary>>,
) -> Result<TransferSummary> {
    send_produced_file_controlled(sender, stream_id, producer_rx, producer, None).await
}

/// A SOURCE fetch allows no peer frames between upload and completion. Retain
/// and join the native producer even when a phase error/cancel interrupts send.
pub(super) async fn send_produced_file_controlled(
    sender: &RouterSender,
    stream_id: StreamId,
    mut producer_rx: mpsc::Receiver<ProducerItem>,
    producer: tokio::task::JoinHandle<Result<TransferSummary>>,
    control: Option<&mut StreamInbox>,
) -> Result<TransferSummary> {
    let sent: Result<()> = tokio::select! {
        biased;
        error = sender.closed() => Err(RemoteTransferError::Router(error)),
        result = async {
            match control {
                Some(inbox) => {
                    let frame = inbox.recv().await?.ok_or(RemoteTransferError::UnexpectedStreamEnd { stream_id: stream_id.get() })?;
                    Err(RemoteTransferError::PrematureFetchFrame(frame.frame().kind()))
                }
                None => std::future::pending::<Result<()>>().await,
            }
        } => result,
        result = async {
            while let Some(item) = producer_rx.recv().await {
                let frame = match item {
                    ProducerItem::Data(bytes) => Frame::new(
                        FrameKind::Data,
                        FrameFlags::empty(),
                        stream_id,
                        WireData::new(bytes)?.into_bytes(),
                    )?,
                    // The COMPRESSED flag appears only on frames whose payload is
                    // actually zstd-compressed, per the protocol contract.
                    ProducerItem::CompressedData(bytes) => Frame::new(
                        FrameKind::Data,
                        FrameFlags::COMPRESSED,
                        stream_id,
                        WireData::new(bytes)?.into_bytes(),
                    )?,
                    ProducerItem::Copy(copy) => Frame::new(
                        FrameKind::DeltaCopy,
                        FrameFlags::empty(),
                        stream_id,
                        copy.encode(),
                    )?,
                };
                sender.send(frame).await?;
            }
            Ok(())
        } => result,
    };
    drop(producer_rx);
    let produced = producer
        .await
        .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))?;
    sent?;
    produced
}

/// Decode the existing FileBegin before preparing native staging. Group owners
/// can use this same boundary for a FileBegin received within their held inbox.
pub(super) fn decode_reconstruction_begin(
    frame: &Frame,
    stream_id: StreamId,
    peer: PlatformOs,
) -> Result<(WireFileBegin, RelativePath)> {
    require_stream(frame, stream_id)?;
    require_empty_flags(frame)?;
    if frame.kind() != FrameKind::FileBegin {
        return Err(RemoteTransferError::UnexpectedFrame {
            expected: FrameKind::FileBegin,
            actual: frame.kind(),
        });
    }
    let begin = WireFileBegin::decode(frame.payload())?;
    if begin.bsd_flags().is_some() && !cfg!(target_os = "macos") {
        return Err(RemoteTransferError::PreservationUnavailable {
            feature: "BSD flags",
        });
    }
    let relative = decode_relative_path(begin.path.clone(), peer)?;
    Ok((begin, relative))
}

/// Reconstruct one peer-opened file stream beneath the root pinned when the v3
/// session was opened. FileBegin may securely reopen a destination basis through
/// that descriptor, but the session root pathname is never resolved again.
pub async fn serve_incoming_file_rooted(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
) -> Result<TransferSummary> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream_id = inbox.stream_id();
    let (begin, relative) = decode_reconstruction_begin(first.frame(), stream_id, peer)?;
    drop(first);

    let basis = begin.basis();
    let prepared =
        tokio::task::spawn_blocking(move || prepare_reconstruction(rooted, &relative, basis))
            .await
            .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))??;

    let mut receiver = ReconstructionReceiver::start(prepared, begin);
    let ready = receiver.receive(&mut inbox, require_file_end_flags).await?;
    let admission = sender.publication_admission();
    let (summary, publication) =
        tokio::task::spawn_blocking(move || ready.commit_blocking(&admission))
            .await
            .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))??;
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            crate::protocol::WireFileAck::new(*publication.identity.as_bytes()).encode(),
        )?)
        .await?;
    Ok(summary)
}

#[cfg(test)]
async fn serve_incoming_file(
    root: PathBuf,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
) -> Result<TransferSummary> {
    let rooted = RootedFs::open(root).await?;
    serve_incoming_file_rooted(rooted, incoming, sender, peer).await
}

pub(super) fn produce_source(
    mut file: File,
    expected_identity: EntryIdentity,
    expected_size: u64,
    basis: Option<BasisIndex>,
    sender: mpsc::Sender<ProducerItem>,
    compression: Option<crate::engine::compression::CompressionPolicy>,
) -> Result<TransferSummary> {
    validate_source(&file, expected_identity, expected_size)?;
    let summary = if let Some(basis) = basis {
        let mut encoder = LiteralEncoder::new(compression.as_ref(), expected_size);
        let delta = match_delta(&mut file, &basis, |op| {
            let item = match op {
                DeltaOp::Literal(bytes) => encoder
                    .encode(bytes)
                    .map_err(|error| Box::new(error) as BoxError)?,
                DeltaOp::Copy { basis_offset, len } => ProducerItem::Copy(
                    WireDeltaCopy::new(basis_offset, len)
                        .map_err(|error| Box::new(error) as BoxError)?,
                ),
            };
            sender.blocking_send(item).map_err(|_| {
                Box::new(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "file transfer consumer closed",
                )) as BoxError
            })
        })?;
        TransferSummary {
            file_size: delta.source_bytes,
            digest: delta.source_digest,
            literal_bytes: delta.literal_bytes,
            reused_bytes: delta.reused_bytes,
        }
    } else {
        produce_whole(&mut file, expected_size, sender, compression.as_ref())?
    };

    validate_source(&file, expected_identity, expected_size)?;
    if summary.file_size != expected_size {
        return Err(RemoteTransferError::SourceChanged {
            expected_size,
            actual_size: summary.file_size,
        });
    }
    Ok(summary)
}

/// Compress a bounded source chunk. Incompressible input can expand beyond
/// the wire limit; the producer must then send the original bytes instead.
fn compress_chunk(bytes: &[u8]) -> Result<Bytes> {
    let compressed = zstd::bulk::compress(bytes, crate::engine::compression::ZSTD_FAST_LEVEL)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(Bytes::from(compressed))
}

pub(crate) fn produce_whole(
    file: &mut File,
    expected_size: u64,
    sender: mpsc::Sender<ProducerItem>,
    compression: Option<&crate::engine::compression::CompressionPolicy>,
) -> Result<TransferSummary> {
    let mut buffer = vec![0_u8; MAX_TRANSFER_DATA_SIZE];
    let mut hasher = blake3::Hasher::new();
    let mut file_size = 0_u64;

    let mut encoder = LiteralEncoder::new(compression, expected_size);

    loop {
        let read = loop {
            match file.read(&mut buffer) {
                Ok(read) => break read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        };
        if read == 0 {
            break;
        }
        let bytes = &buffer[..read];
        hasher.update(bytes);
        file_size = file_size
            .checked_add(u64::try_from(read).map_err(|_| RemoteTransferError::ByteCountOverflow)?)
            .ok_or(RemoteTransferError::ByteCountOverflow)?;

        let item = encoder.encode(Bytes::copy_from_slice(bytes))?;
        sender.blocking_send(item).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "file transfer consumer closed")
        })?;
    }

    Ok(TransferSummary {
        file_size,
        digest: *hasher.finalize().as_bytes(),
        literal_bytes: file_size,
        reused_bytes: 0,
    })
}

/// Whole-file chunks and delta literals share the same actual-payload policy.
/// Auto samples the first literal, not a COPY or signature.
struct LiteralEncoder {
    decision: crate::engine::compression::CompressionChoice,
    first_chunk: bool,
    expected_size: u64,
}

impl LiteralEncoder {
    fn new(
        policy: Option<&crate::engine::compression::CompressionPolicy>,
        expected_size: u64,
    ) -> Self {
        use crate::engine::compression::{CompressionChoice, CompressionPolicy};
        Self {
            decision: if policy.is_some() {
                CompressionChoice::ZstdFast
            } else {
                CompressionChoice::None
            },
            first_chunk: policy == Some(&CompressionPolicy::Auto),
            expected_size,
        }
    }

    fn encode(&mut self, bytes: Bytes) -> Result<ProducerItem> {
        use crate::engine::compression::{
            choose_for_min_elapsed, ByteRate, CompressionChoice, CompressionSample,
            CompressionTiming, DEFAULT_LINK_RATE_BYTES_PER_SEC,
        };
        let compressed = if self.decision == CompressionChoice::ZstdFast {
            Some(compress_chunk(&bytes)?)
        } else {
            None
        };
        if self.first_chunk {
            self.first_chunk = false;
            let compressed = compressed
                .as_ref()
                .expect("Auto samples the first nonempty literal");
            let sample = CompressionSample::new(bytes.len() as u64, compressed.len() as u64)
                .expect("literal is nonempty");
            // Initial codec-rate estimates, not host throughput measurements.
            let timing = CompressionTiming::new(
                ByteRate::new(DEFAULT_LINK_RATE_BYTES_PER_SEC).expect("constant is non-zero"),
                ByteRate::new(800 * 1024 * 1024).expect("constant is non-zero"),
                ByteRate::new(1600 * 1024 * 1024).expect("constant is non-zero"),
                std::num::NonZeroU32::new(MAX_TRANSFER_DATA_SIZE as u32)
                    .expect("chunk size is non-zero"),
            );
            self.decision = choose_for_min_elapsed(self.expected_size, sample, timing);
        }
        Ok(match compressed {
            Some(compressed)
                if self.decision == CompressionChoice::ZstdFast
                    && compressed.len() < bytes.len() =>
            {
                ProducerItem::CompressedData(compressed)
            }
            _ => ProducerItem::Data(bytes),
        })
    }
}

fn checked_output_size(current: u64, added: usize, expected_size: u64) -> Result<u64> {
    let added = u64::try_from(added).map_err(|_| RemoteTransferError::ByteCountOverflow)?;
    let next = current
        .checked_add(added)
        .ok_or(RemoteTransferError::ByteCountOverflow)?;
    if next > expected_size {
        return Err(RemoteTransferError::ReconstructionTooLarge { expected_size });
    }
    Ok(next)
}

pub(crate) fn validate_source(
    file: &File,
    expected: EntryIdentity,
    expected_size: u64,
) -> Result<()> {
    let metadata = file.metadata()?;
    let identity = opened_identity(&metadata)?;
    if metadata.len() != expected_size || identity != expected {
        return Err(RemoteTransferError::SourceChanged {
            expected_size,
            actual_size: metadata.len(),
        });
    }
    Ok(())
}

fn validate_basis(
    rooted: &RootedFs,
    file: &File,
    relative: &RelativePath,
    expected: WireFileBasis,
) -> Result<()> {
    // Always resolve the ORIGINAL scanned identity through exact own retirement
    // records. A fresh opened stat is not permission to adopt a different basis.
    let metadata = rooted.observed_regular_metadata_blocking(
        file,
        relative,
        EntryIdentity::from_bytes(expected.identity()),
    )?;
    if metadata.len() != expected.file_size() {
        return Err(RemoteTransferError::BasisChanged {
            expected_size: expected.file_size(),
            actual_size: metadata.len(),
        });
    }
    Ok(())
}

fn opened_identity(metadata: &std::fs::Metadata) -> Result<EntryIdentity> {
    crate::endpoint::local_identity::metadata_identity(metadata, EntryKind::File)
        .ok_or(RemoteTransferError::MissingOpenedIdentity)
}

pub(crate) async fn receive_ack(
    inbox: &mut StreamInbox,
    stream_id: StreamId,
) -> Result<crate::protocol::WireFileAck> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteTransferError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    require_empty_flags(frame)?;
    if frame.kind() != FrameKind::Ack {
        return Err(RemoteTransferError::UnexpectedFrame {
            expected: FrameKind::Ack,
            actual: frame.kind(),
        });
    }
    Ok(crate::protocol::WireFileAck::decode(frame.payload())?)
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteTransferError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_empty_flags(frame: &Frame) -> Result<()> {
    if frame.flags().is_empty() {
        Ok(())
    } else {
        Err(RemoteTransferError::FrameFlags {
            kind: frame.kind(),
            flags: frame.flags().bits(),
        })
    }
}

fn require_file_end_flags(frame: &Frame) -> Result<()> {
    let expected = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
    if frame.flags() == expected {
        Ok(())
    } else {
        Err(RemoteTransferError::FileEndFlags {
            flags: frame.flags().bits(),
        })
    }
}

#[cfg(all(test, unix))]
mod population_tests;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::engine::rolling::WeakChecksum;
    use crate::protocol::Operation;
    use crate::remote::data::DataChunk;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
    use crate::remote::{client_handshake, server_handshake};
    use crate::transfer::delta::{BasisBlock, BasisIndexLimits};
    use futures::TryStreamExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    fn file_entry(root: &Path, relative: &str) -> Entry {
        let path = root.join(relative);
        let metadata = std::fs::metadata(&path).unwrap();
        let identity =
            crate::endpoint::local_identity::metadata_identity(&metadata, EntryKind::File).unwrap();
        let mut entry = Entry::file(
            RelativePath::new(PathBuf::from(relative)).unwrap(),
            metadata.len(),
            Timestamp::UNIX_EPOCH,
        );
        entry.identity = Some(identity);
        entry
    }

    fn basis_index(data: &[u8], block_size: usize) -> BasisIndex {
        let blocks = data
            .chunks(block_size)
            .enumerate()
            .map(|(index, bytes)| {
                let digest = blake3::hash(bytes);
                let mut strong = [0_u8; 16];
                strong.copy_from_slice(&digest.as_bytes()[..16]);
                BasisBlock {
                    index: index as u64,
                    size: bytes.len() as u32,
                    weak: WeakChecksum::hash(bytes),
                    strong,
                }
            })
            .collect::<Vec<_>>();
        BasisIndex::new(block_size as u32, blocks, BasisIndexLimits::default()).unwrap()
    }

    #[test]
    fn compression_keeps_every_encoded_and_decoded_chunk_within_the_wire_limit() {
        use crate::engine::compression::CompressionPolicy;

        let mut data = vec![b'x'; MAX_TRANSFER_DATA_SIZE];
        // Deterministic high-entropy input, not a repeating byte ramp which
        // would compress well and fail to exercise zstd header expansion.
        let mut state = 0x8d12_e3a5_6b7c_90f1_u64;
        for _ in 0..MAX_TRANSFER_DATA_SIZE {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push(state as u8);
        }
        for policy in [
            None,
            Some(CompressionPolicy::Auto),
            Some(CompressionPolicy::Always),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(&data).unwrap();
            file.rewind().unwrap();
            // The fixture has only two chunks; it fits the production queue.
            let (tx, mut rx) = mpsc::channel(PRODUCER_QUEUE_DEPTH);
            let summary = produce_whole(&mut file, data.len() as u64, tx, policy.as_ref()).unwrap();
            let mut received = Vec::new();
            let mut compressed_chunks = 0;
            while let Ok(item) = rx.try_recv() {
                let chunk = match item {
                    ProducerItem::Data(bytes) => {
                        assert!(bytes.len() <= MAX_TRANSFER_DATA_SIZE);
                        DataChunk::Plain(bytes)
                    }
                    ProducerItem::CompressedData(bytes) => {
                        assert!(bytes.len() < MAX_TRANSFER_DATA_SIZE);
                        compressed_chunks += 1;
                        DataChunk::Zstd(bytes)
                    }
                    ProducerItem::Copy(_) => panic!("whole transfer emitted a basis copy"),
                };
                let decoded = chunk.decode_blocking().unwrap();
                assert!(decoded.len() <= MAX_TRANSFER_DATA_SIZE);
                received.extend_from_slice(&decoded);
            }
            assert_eq!(received, data);
            assert_eq!(summary.digest, *blake3::hash(&data).as_bytes());
            assert_eq!(compressed_chunks, usize::from(policy.is_some()));
        }
    }

    #[tokio::test]
    async fn file_ack_rejects_malformed_proofs_and_flags_on_the_router_path() {
        for (flags, payload) in [
            (FrameFlags::empty(), Bytes::new()),
            (FrameFlags::empty(), Bytes::from_static(&[1; 31])),
            (FrameFlags::empty(), Bytes::from_static(&[1; 33])),
            (FrameFlags::FINAL, Bytes::from_static(&[1; 32])),
        ] {
            let (client_io, server_io) = tokio::io::duplex(4096);
            let (cr, cw) = tokio::io::split(client_io);
            let (sr, sw) = tokio::io::split(server_io);
            let client =
                FrameRouter::start(cr, cw, RouterRole::Client, RouterConfig::default()).unwrap();
            let mut server =
                FrameRouter::start(sr, sw, RouterRole::Server, RouterConfig::default()).unwrap();
            let mut inbox = client.sender().open_stream().unwrap();
            let id = inbox.stream_id();
            client
                .sender()
                .send(
                    Frame::new(
                        FrameKind::FileBegin,
                        FrameFlags::empty(),
                        id,
                        WireFileBegin::whole(encode_relative_path(Path::new("file")).unwrap(), 0)
                            .encode(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            let _incoming = server.incoming().recv().await.unwrap().unwrap();
            server
                .sender()
                .send(Frame::new(FrameKind::Ack, flags, id, payload).unwrap())
                .await
                .unwrap();
            assert!(receive_ack(&mut inbox, id).await.is_err());
        }
    }

    #[tokio::test]
    async fn rejected_receive_drains_staging_without_waiting_for_another_frame() {
        let compressed =
            zstd::bulk::compress(&vec![b'x'; MAX_TRANSFER_DATA_SIZE * 32], -5).unwrap();
        for (flags, payload) in [
            (FrameFlags::COMPRESSED, Bytes::from(compressed)),
            (FrameFlags::COMPRESSED, Bytes::from_static(b"invalid zstd")),
            (FrameFlags::FINAL, Bytes::from_static(b"invalid flags")),
        ] {
            let root = tempfile::TempDir::new().unwrap();
            std::fs::write(root.path().join("file"), b"old").unwrap();
            let destination = file_entry(root.path(), "file");
            let basis =
                WireFileBasis::new(destination.size, *destination.identity.unwrap().as_bytes());
            let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (client_reader, client_writer) = tokio::io::split(client_io);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let client = FrameRouter::start(
                client_reader,
                client_writer,
                RouterRole::Client,
                RouterConfig::default(),
            )
            .unwrap();
            let mut server_router = FrameRouter::start(
                server_reader,
                server_writer,
                RouterRole::Server,
                RouterConfig::default(),
            )
            .unwrap();
            let server = tokio::spawn(async move {
                let incoming = server_router.incoming().recv().await.unwrap().unwrap();
                serve_incoming_file_rooted(
                    rooted,
                    incoming,
                    &server_router.sender(),
                    crate::protocol::Platform::current().os,
                )
                .await
            });
            let sender = client.sender();
            let inbox = sender.open_stream().unwrap();
            let id = inbox.stream_id();
            let path = encode_relative_path(Path::new("file")).unwrap();
            sender
                .send(
                    Frame::new(
                        FrameKind::FileBegin,
                        FrameFlags::empty(),
                        id,
                        WireFileBegin::delta(path, (MAX_TRANSFER_DATA_SIZE * 32) as u64, basis)
                            .encode(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            sender
                .send(Frame::new(FrameKind::Data, flags, id, payload).unwrap())
                .await
                .unwrap();
            // Keep the transport open but send no FileEnd. Neither malformed
            // input nor worker failure may wait for the peer to send more data.
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .expect("failed transfer waited for more peer input")
                .unwrap();
            assert!(matches!(
                result,
                Err(RemoteTransferError::Decompression(_))
                    | Err(RemoteTransferError::FrameFlags { .. })
            ));
            assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"old");
            assert_eq!(
                std::fs::read_dir(root.path()).unwrap().count(),
                1,
                "receive returned before private staging was removed"
            );
        }
    }

    #[tokio::test]
    async fn selected_followed_source_stream_retains_owner_through_publication_receipt() {
        use crate::endpoint::source_root::SourceRoot;
        use crate::rooted_fs::operand::{BoundSource, SourceShape};
        for late_change in [false, true] {
            let source_root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let target = outside.path().join("target");
            let selected = source_root.path().join("selected");
            std::fs::write(&target, b"original DATA").unwrap();
            xattr::set(&target, "user.sy-push-owner", b"original").unwrap();
            std::os::unix::fs::symlink(&target, &selected).unwrap();
            let stamp = |path: &Path| {
                let metadata = std::fs::symlink_metadata(path).unwrap();
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.nlink(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            };
            let originals = [stamp(&selected), stamp(&target)];
            let bound = BoundSource::open_local_following(selected.clone(), false)
                .await
                .unwrap();
            let SourceShape::Leaf { name } = bound.shape else {
                panic!("not selected")
            };
            let authority = SourceRoot::from_rooted(bound.rooted).unwrap();
            let source = authority
                .selected_entries(
                    name,
                    crate::engine::scan::ScanRequest {
                        follow_symlinks: true,
                        ..Default::default()
                    },
                    false,
                )
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .remove(0);
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (mut client_reader, mut client_writer) = tokio::io::split(client_io);
            let (mut server_reader, mut server_writer) = tokio::io::split(server_io);
            let raced_target = target.clone();
            let server = tokio::spawn(async move {
                let opened = server_handshake(&mut server_reader, &mut server_writer)
                    .await
                    .unwrap();
                let mut router = FrameRouter::start(
                    server_reader,
                    server_writer,
                    RouterRole::Server,
                    RouterConfig::default(),
                )
                .unwrap();
                let IncomingStream { first, mut inbox } =
                    router.incoming().recv().await.unwrap().unwrap();
                let sender = router.sender();
                let stream_id = inbox.stream_id();
                let (begin, relative) = decode_reconstruction_begin(
                    first.frame(),
                    stream_id,
                    opened.client.platform.os,
                )
                .unwrap();
                drop(first);
                let rooted = RootedFs::open(opened.root).await.unwrap();
                let basis = begin.basis();
                let prepared = tokio::task::spawn_blocking(move || {
                    prepare_reconstruction(rooted, &relative, basis)
                })
                .await
                .unwrap()
                .unwrap();
                let mut receiver = ReconstructionReceiver::start(prepared, begin);
                let ready = receiver
                    .receive(&mut inbox, require_file_end_flags)
                    .await
                    .unwrap();
                let admission = sender.publication_admission();
                let (summary, publication) = tokio::task::spawn_blocking(move || {
                    let result = ready.commit_blocking(&admission)?;
                    if late_change {
                        // External SOURCE change after real DEST commit but before ACK.
                        std::fs::write(raced_target, b"changed DATA")?;
                    }
                    Ok::<_, RemoteTransferError>(result)
                })
                .await
                .unwrap()
                .unwrap();
                sender
                    .send(
                        Frame::new(
                            FrameKind::Ack,
                            FrameFlags::empty(),
                            stream_id,
                            crate::protocol::WireFileAck::new(*publication.identity.as_bytes())
                                .encode(),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                summary
            });
            let session = client_handshake(
                &mut client_reader,
                &mut client_writer,
                Operation::Push,
                destination.path(),
            )
            .await
            .unwrap();
            let router = FrameRouter::start(
                client_reader,
                client_writer,
                RouterRole::Client,
                RouterConfig::default(),
            )
            .unwrap();
            let result = request_file_transfer_with_stream_policy(
                &router.sender(),
                authority,
                source,
                TransferDestination::create(RelativePath::new("copied").unwrap()),
                TransferMetadata::default(),
                session.server.platform.os,
                TransferStreamPolicy {
                    preservation: TransferPreservationRequest {
                        xattrs: true,
                        acls: false,
                    },
                    ..Default::default()
                },
            )
            .await;
            if late_change {
                let Err(RemoteTransferError::Io(error)) = result else {
                    panic!("expected committed SOURCE completion error: {result:?}")
                };
                assert!(error
                    .get_ref()
                    .unwrap()
                    .downcast_ref::<SourceCompletionError>()
                    .is_some());
                server.await.unwrap();
                assert_eq!(std::fs::read(&target).unwrap(), b"changed DATA");
            } else {
                let (summary, publication) = result.unwrap();
                assert_eq!(summary, server.await.unwrap());
                assert_eq!(publication.path, RelativePath::new("copied").unwrap());
                assert_eq!([stamp(&selected), stamp(&target)], originals);
            }
            assert_eq!(std::fs::read_link(&selected).unwrap(), target);
            assert_eq!(
                std::fs::read(destination.path().join("copied")).unwrap(),
                b"original DATA"
            );
            assert_eq!(
                xattr::get(destination.path().join("copied"), "user.sy-push-owner").unwrap(),
                Some(b"original".to_vec())
            );
            assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn whole_file_stream_commits_only_after_verified_file_end() {
        let source_root = tempfile::TempDir::new().unwrap();
        let destination_root = tempfile::TempDir::new().unwrap();
        let data = vec![0x5a_u8; MAX_TRANSFER_DATA_SIZE * 2 + 17];
        std::fs::write(source_root.path().join("file.bin"), &data).unwrap();
        std::fs::write(destination_root.path().join("file.bin"), b"old").unwrap();
        let authority =
            crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                .await
                .unwrap();
        let source = authority
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_io);

        let server = tokio::spawn(async move {
            let opened = server_handshake(&mut server_reader, &mut server_writer)
                .await
                .unwrap();
            let mut router = FrameRouter::start(
                server_reader,
                server_writer,
                RouterRole::Server,
                RouterConfig::default(),
            )
            .unwrap();
            let incoming = router.incoming().recv().await.unwrap().unwrap();
            let sender = router.sender();
            serve_incoming_file(opened.root, incoming, &sender, opened.client.platform.os)
                .await
                .unwrap()
        });

        let session = client_handshake(
            &mut client_reader,
            &mut client_writer,
            Operation::Push,
            destination_root.path(),
        )
        .await
        .unwrap();
        let router = FrameRouter::start(
            client_reader,
            client_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();
        let modified = Timestamp::new(1_600_000_030, 0).unwrap();
        let destination = file_entry(destination_root.path(), "file.bin");
        let destination_expectation =
            WireFileBasis::new(destination.size, *destination.identity.unwrap().as_bytes());
        let summary = request_file_transfer_with_metadata(
            &router.sender(),
            authority,
            source,
            TransferDestination::whole(destination.path, destination_expectation),
            TransferMetadata {
                unix_mode: Some(0o640),
                modified: Some(modified),
                xattrs: None,
                acls: None,
            },
            session.server.platform.os,
        )
        .await
        .unwrap();
        let received = server.await.unwrap();

        assert_eq!(summary, received);
        assert_eq!(summary.file_size, data.len() as u64);
        assert_eq!(summary.literal_bytes, data.len() as u64);
        assert_eq!(summary.reused_bytes, 0);
        assert_eq!(
            std::fs::read(destination_root.path().join("file.bin")).unwrap(),
            data
        );
        let metadata = std::fs::metadata(destination_root.path().join("file.bin")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o640);
        assert_eq!(metadata.mtime(), modified.seconds());
    }

    #[tokio::test]
    async fn delta_stream_reuses_pinned_destination_basis() {
        let source_root = tempfile::TempDir::new().unwrap();
        let destination_root = tempfile::TempDir::new().unwrap();
        let destination = b"abcdefghijkl";
        let source_data = b"Xabcdefghijkl";
        std::fs::write(source_root.path().join("file.bin"), source_data).unwrap();
        std::fs::write(destination_root.path().join("file.bin"), destination).unwrap();
        let authority =
            crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                .await
                .unwrap();
        let source = authority
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        let basis_entry = file_entry(destination_root.path(), "file.bin");
        let expectation =
            WireFileBasis::new(basis_entry.size, *basis_entry.identity.unwrap().as_bytes());
        let delta_index = basis_index(destination, 4);

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mut client_reader, mut client_writer) = tokio::io::split(client_io);
        let (mut server_reader, mut server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let opened = server_handshake(&mut server_reader, &mut server_writer)
                .await
                .unwrap();
            let mut router = FrameRouter::start(
                server_reader,
                server_writer,
                RouterRole::Server,
                RouterConfig::default(),
            )
            .unwrap();
            let incoming = router.incoming().recv().await.unwrap().unwrap();
            let sender = router.sender();
            serve_incoming_file(opened.root, incoming, &sender, opened.client.platform.os)
                .await
                .unwrap()
        });

        let session = client_handshake(
            &mut client_reader,
            &mut client_writer,
            Operation::Push,
            destination_root.path(),
        )
        .await
        .unwrap();
        let router = FrameRouter::start(
            client_reader,
            client_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();
        let summary = request_file_transfer(
            &router.sender(),
            authority,
            source,
            TransferDestination::delta(
                RelativePath::new("file.bin").unwrap(),
                expectation,
                delta_index,
            ),
            session.server.platform.os,
        )
        .await
        .unwrap();
        let received = server.await.unwrap();

        assert_eq!(summary, received);
        assert_eq!(summary.literal_bytes, 1);
        assert_eq!(summary.reused_bytes, destination.len() as u64);
        assert_eq!(
            std::fs::read(destination_root.path().join("file.bin")).unwrap(),
            source_data
        );
    }

    #[tokio::test]
    async fn terminal_state_fences_file_end_unless_publication_was_admitted() {
        use crate::remote::router::{FrameRouter, RouterConfig, RouterError, RouterRole};
        use crate::rooted_fs::{PublicationPause, PublicationPausePoint};
        use tokio::io::AsyncWriteExt;

        for (eof, already_admitted) in [(true, false), (false, false), (false, true)] {
            let root = tempfile::TempDir::new().unwrap();
            let path = root.path().join("file");
            std::fs::write(&path, b"old").unwrap();
            let original = std::fs::metadata(&path).unwrap();
            let original_inode = original.ino();
            let identity =
                crate::endpoint::local_identity::metadata_identity(&original, EntryKind::File)
                    .unwrap();
            let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
            let relative = RelativePath::new(PathBuf::from("file")).unwrap();
            let mut prepared = tokio::task::spawn_blocking({
                let relative = relative.clone();
                move || {
                    prepare_reconstruction(
                        rooted,
                        &relative,
                        Some(WireFileBasis::new(original.len(), *identity.as_bytes())),
                    )
                }
            })
            .await
            .unwrap()
            .unwrap();

            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            prepared.staged.pause_publication(PublicationPause {
                point: if already_admitted {
                    PublicationPausePoint::AfterAdmission
                } else {
                    PublicationPausePoint::BeforeAdmission
                },
                reached: reached_tx,
                resume: resume_rx,
            });
            let (router_io, mut peer) = tokio::io::duplex(4096);
            let (reader, writer) = tokio::io::split(router_io);
            let mut router =
                FrameRouter::start(reader, writer, RouterRole::Server, RouterConfig::default())
                    .unwrap();
            let sender = router.sender();
            let admission = sender.publication_admission();
            let begin = WireFileBegin::whole(encode_relative_path(relative.as_path()).unwrap(), 3);
            let (tx, rx) = mpsc::channel(2);
            let worker = tokio::task::spawn_blocking(move || {
                reconstruct_file(prepared, begin, rx, &admission)
            });
            tx.send(ReconstructionOp::Data(DataChunk::Plain(
                Bytes::from_static(b"new"),
            )))
            .await
            .unwrap();
            tx.send(ReconstructionOp::End(WireFileEnd::new(
                3,
                *blake3::hash(b"new").as_bytes(),
            )))
            .await
            .unwrap();
            drop(tx);

            let paused = tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx).await;
            let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                if eof {
                    peer.shutdown().await.unwrap();
                } else {
                    sender.fail(std::sync::Arc::new(RouterError::WriterClosed));
                }
                sender.closed().await
            })
            .await;
            let remained_private = std::fs::read(&path).unwrap() == b"old";
            let worker_still_owned = !worker.is_finished();
            // Release before asserting so regressions cannot strand blocking work.
            let _ = resume_tx.send(());
            let result = worker.await.unwrap();
            router.shutdown().await.unwrap();

            assert!(matches!(paused, Ok(Ok(()))));
            assert!(
                terminal.is_ok(),
                "terminal actor blocked behind native publication"
            );
            assert!(remained_private);
            assert!(worker_still_owned);
            if already_admitted {
                assert!(
                    result.is_ok(),
                    "admitted native work may complete after closure"
                );
                assert_eq!(std::fs::read(&path).unwrap(), b"new");
            } else {
                assert!(matches!(
                    result,
                    Err(RemoteTransferError::RootedFs(
                        RootedFsError::CommitCancelled
                    ))
                ));
                assert_eq!(std::fs::read(&path).unwrap(), b"old");
                assert_eq!(std::fs::metadata(&path).unwrap().ino(), original_inode);
            }
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn reconstruction_opens_basis_only_for_copy_and_keeps_original_expectation() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no pointer arguments or side effects.
        assert_ne!(unsafe { libc::geteuid() }, 0);
        for case in [
            "unreadable-literal",
            "unreadable-copy",
            "raced-copy",
            "readable-copy",
        ] {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("file");
            std::fs::write(&target, b"old").unwrap();
            let unreadable = case.starts_with("unreadable");
            if unreadable {
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o0)).unwrap();
                assert_eq!(
                    File::open(&target).unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied
                );
            }
            let original = std::fs::metadata(&target).unwrap();
            let identity = opened_identity(&original).unwrap();
            let expectation = WireFileBasis::new(original.len(), *identity.as_bytes());
            let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
            let relative = RelativePath::new("file").unwrap();
            let prepared = tokio::task::spawn_blocking({
                let relative = relative.clone();
                move || prepare_reconstruction(rooted, &relative, Some(expectation))
            })
            .await
            .unwrap()
            .unwrap();
            if case == "raced-copy" {
                let foreign = root.path().join("foreign");
                std::fs::write(&foreign, b"foreign").unwrap();
                std::fs::rename(foreign, &target).unwrap();
            }
            let before = opened_identity(&std::fs::metadata(&target).unwrap()).unwrap();
            let literal = case == "unreadable-literal";
            let bytes = if literal { b"new" } else { b"old" };
            // HAS_BASIS still carries the overwrite expectation even when the
            // actual stream contains only Data. No wire-format change is needed.
            let begin = WireFileBegin::delta(
                encode_relative_path(relative.as_path()).unwrap(),
                3,
                expectation,
            )
            .with_metadata(Some(0o600), None)
            .unwrap();
            let (tx, rx) = mpsc::channel(2);
            tx.send(if literal {
                ReconstructionOp::Data(DataChunk::Plain(Bytes::from_static(bytes)))
            } else {
                ReconstructionOp::Copy(WireDeltaCopy::new(0, 3).unwrap())
            })
            .await
            .unwrap();
            tx.send(ReconstructionOp::End(WireFileEnd::new(
                3,
                *blake3::hash(bytes).as_bytes(),
            )))
            .await
            .unwrap();
            drop(tx);
            let result = tokio::task::spawn_blocking(move || {
                reconstruct_file(
                    prepared,
                    begin,
                    rx,
                    &std::sync::Arc::new(
                        crate::endpoint::publication::PublicationAdmission::default(),
                    ),
                )
            })
            .await
            .unwrap();
            match case {
                "unreadable-copy" => {
                    assert!(
                        matches!(result, Err(RemoteTransferError::RootedFs(RootedFsError::Io(error))) if error.kind() == io::ErrorKind::PermissionDenied)
                    );
                    assert_eq!(
                        opened_identity(&std::fs::metadata(&target).unwrap()).unwrap(),
                        before
                    );
                }
                "raced-copy" => {
                    assert!(matches!(
                        result,
                        Err(RemoteTransferError::RootedFs(
                            RootedFsError::DestinationChanged(_)
                        ))
                    ));
                    assert_eq!(std::fs::read(&target).unwrap(), b"foreign");
                    assert_eq!(
                        opened_identity(&std::fs::metadata(&target).unwrap()).unwrap(),
                        before
                    );
                }
                _ => {
                    let (summary, _) = result.unwrap();
                    assert_eq!(summary.reused_bytes, if literal { 0 } else { 3 });
                    assert_eq!(std::fs::read(&target).unwrap(), bytes);
                }
            }
            assert_eq!(
                std::fs::read_dir(root.path()).unwrap().count(),
                1,
                "stage must be drained"
            );
        }
    }

    #[tokio::test]
    async fn bad_digest_drops_stage_and_preserves_destination() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let original = std::fs::metadata(root.path().join("file")).unwrap();
        let original_mode = original.mode();
        let original_mtime = original.mtime();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();
        let destination_identity =
            crate::endpoint::local_identity::metadata_identity(&original, EntryKind::File).unwrap();
        let expectation = WireFileBasis::new(original.len(), *destination_identity.as_bytes());
        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, Some(expectation))
        })
        .await
        .unwrap()
        .unwrap();
        let wire_path = encode_relative_path(relative.as_path()).unwrap();
        let begin = WireFileBegin::whole(wire_path, 3)
            .with_metadata(Some(0o600), Some((1_600_000_040, 0)))
            .unwrap();
        let (tx, rx) = mpsc::channel(2);
        let worker = tokio::task::spawn_blocking(move || {
            population::populate_reconstruction(prepared, begin, rx)
        });
        tx.send(ReconstructionOp::Data(DataChunk::Plain(
            Bytes::from_static(b"new"),
        )))
        .await
        .unwrap();
        tx.send(ReconstructionOp::End(WireFileEnd::new(3, [0; 32])))
            .await
            .unwrap();
        drop(tx);

        assert!(matches!(
            worker.await.unwrap(),
            Err(RemoteTransferError::DigestMismatch)
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"old");
        let preserved = std::fs::metadata(root.path().join("file")).unwrap();
        assert_eq!(preserved.mode(), original_mode);
        assert_eq!(preserved.mtime(), original_mtime);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    /// A create declares no destination expectation: the server must refuse a
    /// destination that appeared instead of silently replacing it.
    #[tokio::test]
    async fn whole_create_refuses_existing_destination() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"keep").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();

        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, None)
        })
        .await
        .unwrap();

        assert!(matches!(
            prepared,
            Err(RemoteTransferError::UnexpectedDestination { .. })
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"keep");
    }

    /// The expectation must describe the scanned destination exactly.
    #[tokio::test]
    async fn update_with_wrong_destination_expectation_refuses() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let decoy = root.path().join("decoy");
        std::fs::write(&decoy, b"decoy content").unwrap();
        let decoy_meta = std::fs::metadata(&decoy).unwrap();
        let decoy_identity =
            crate::endpoint::local_identity::metadata_identity(&decoy_meta, EntryKind::File)
                .unwrap();
        let wrong = WireFileBasis::new(decoy_meta.len(), *decoy_identity.as_bytes());

        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();
        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, Some(wrong))
        })
        .await
        .unwrap();

        assert!(matches!(
            prepared,
            Err(RemoteTransferError::DestinationChanged { .. })
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"old");
    }

    /// A destination replaced after staging but before FileEnd aborts the
    /// commit; the concurrent replacement survives untouched.
    #[tokio::test]
    async fn destination_replaced_before_commit_refuses() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
        let original = std::fs::metadata(root.path().join("file")).unwrap();
        let identity =
            crate::endpoint::local_identity::metadata_identity(&original, EntryKind::File).unwrap();
        let expectation = WireFileBasis::new(original.len(), *identity.as_bytes());
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();
        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, Some(expectation))
        })
        .await
        .unwrap()
        .unwrap();

        let wire_path = encode_relative_path(relative.as_path()).unwrap();
        let begin = WireFileBegin::whole(wire_path, 3);
        let (tx, rx) = mpsc::channel(2);
        let worker = tokio::task::spawn_blocking(move || {
            reconstruct_file(
                prepared,
                begin,
                rx,
                &std::sync::Arc::new(crate::endpoint::publication::PublicationAdmission::default()),
            )
        });
        tx.send(ReconstructionOp::Data(DataChunk::Plain(
            Bytes::from_static(b"new"),
        )))
        .await
        .unwrap();

        // A concurrent writer replaces the destination while bytes are staged.
        std::fs::write(root.path().join("file"), b"RACED").unwrap();

        let digest = blake3::hash(b"new");
        tx.send(ReconstructionOp::End(WireFileEnd::new(
            3,
            *digest.as_bytes(),
        )))
        .await
        .unwrap();
        drop(tx);

        assert!(matches!(
            worker.await.unwrap(),
            Err(RemoteTransferError::DestinationChanged { .. })
        ));
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"RACED");
        // Only the preserved destination remains: staging is gone.
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    /// Preservation frames land on the staged inode before commit, so the
    /// committed file already carries the source's attributes.
    #[tokio::test]
    async fn staged_preservation_applies_before_commit() {
        let root = tempfile::TempDir::new().unwrap();
        let source_path = root.path().join("source");
        std::fs::write(&source_path, b"new").unwrap();
        xattr::set(&source_path, "user.sy_test", b"v").unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let source_relative = RelativePath::new(PathBuf::from("source")).unwrap();
        let source = rooted.open_regular_blocking(&source_relative).unwrap();
        let source_xattrs = rooted
            .read_open_file_xattrs_blocking(&source, &source_relative)
            .unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();
        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, None)
        })
        .await
        .unwrap()
        .unwrap();

        let wire_path = encode_relative_path(relative.as_path()).unwrap();
        let begin = WireFileBegin::whole(wire_path, 3)
            .with_metadata(Some(0o640), None)
            .unwrap();
        let (tx, rx) = mpsc::channel(4);
        let worker = tokio::task::spawn_blocking(move || {
            reconstruct_file(
                prepared,
                begin,
                rx,
                &std::sync::Arc::new(crate::endpoint::publication::PublicationAdmission::default()),
            )
        });
        tx.send(ReconstructionOp::Data(DataChunk::Plain(
            Bytes::from_static(b"new"),
        )))
        .await
        .unwrap();
        tx.send(ReconstructionOp::Xattrs(source_xattrs))
            .await
            .unwrap();
        let digest = blake3::hash(b"new");
        tx.send(ReconstructionOp::End(WireFileEnd::new(
            3,
            *digest.as_bytes(),
        )))
        .await
        .unwrap();
        drop(tx);

        worker.await.unwrap().unwrap();
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"new");
        let value = xattr::get(root.path().join("file"), "user.sy_test").unwrap();
        assert_eq!(value.as_deref(), Some(&b"v"[..]));
    }

    /// A preservation payload that cannot be applied aborts the
    /// reconstruction before commit.
    #[cfg(all(unix, feature = "acl"))]
    #[tokio::test]
    async fn preservation_failure_aborts_reconstruction() {
        let root = tempfile::TempDir::new().unwrap();
        let target = root.path().join("file");
        std::fs::write(&target, b"old").unwrap();
        let original = std::fs::metadata(&target).unwrap();
        let expectation = WireFileBasis::new(
            original.len(),
            *opened_identity(&original).unwrap().as_bytes(),
        );
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();
        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, Some(expectation))
        })
        .await
        .unwrap()
        .unwrap();

        let wire_path = encode_relative_path(relative.as_path()).unwrap();
        let begin = WireFileBegin::whole(wire_path, 3);
        let (tx, rx) = mpsc::channel(4);
        let worker = tokio::task::spawn_blocking(move || {
            population::populate_reconstruction(prepared, begin, rx)
        });
        tx.send(ReconstructionOp::Data(DataChunk::Plain(
            Bytes::from_static(b"new"),
        )))
        .await
        .unwrap();
        tx.send(ReconstructionOp::Acls("not an acl entry".to_string()))
            .await
            .unwrap();
        let digest = blake3::hash(b"new");
        tx.send(ReconstructionOp::End(WireFileEnd::new(
            3,
            *digest.as_bytes(),
        )))
        .await
        .unwrap();
        drop(tx);

        assert!(matches!(
            worker.await.unwrap(),
            Err(RemoteTransferError::RootedFs(_))
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        let preserved = std::fs::metadata(&target).unwrap();
        assert_eq!(preserved.ino(), original.ino());
        assert_eq!(preserved.mode(), original.mode());
        assert_eq!(preserved.mtime(), original.mtime());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    /// Whole-file replacement of a scanned symlink is a legal type transition:
    /// the identity expectation is kind-tagged, and non-regular destinations
    /// receive whole bytes without a copy basis.
    #[cfg(unix)]
    #[tokio::test]
    async fn type_transition_with_matching_expectation_commits() {
        let root = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink("elsewhere", root.path().join("file")).unwrap();
        let link_meta = std::fs::symlink_metadata(root.path().join("file")).unwrap();
        let identity =
            crate::endpoint::local_identity::metadata_identity(&link_meta, EntryKind::Symlink)
                .unwrap();
        let expectation = WireFileBasis::new(link_meta.len(), *identity.as_bytes());
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();
        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, Some(expectation))
        })
        .await
        .unwrap()
        .unwrap();

        let wire_path = encode_relative_path(relative.as_path()).unwrap();
        let begin = WireFileBegin::whole(wire_path, 3);
        let (tx, rx) = mpsc::channel(2);
        let worker = tokio::task::spawn_blocking(move || {
            reconstruct_file(
                prepared,
                begin,
                rx,
                &std::sync::Arc::new(crate::endpoint::publication::PublicationAdmission::default()),
            )
        });
        tx.send(ReconstructionOp::Data(DataChunk::Plain(
            Bytes::from_static(b"new"),
        )))
        .await
        .unwrap();
        let digest = blake3::hash(b"new");
        tx.send(ReconstructionOp::End(WireFileEnd::new(
            3,
            *digest.as_bytes(),
        )))
        .await
        .unwrap();
        drop(tx);

        worker.await.unwrap().unwrap();
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), b"new");
    }
}
