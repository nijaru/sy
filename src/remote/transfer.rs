use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::engine::reconcile::BoxError;
use crate::engine::work::TransferSummary;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireData, WireDeltaCopy,
    WireFileBasis, WireFileBegin, WireFileEnd, MAX_TRANSFER_DATA_SIZE,
};
use crate::remote::data::DataChunk;
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::rooted_fs::{RootedFs, RootedFsError, RootedStagedFile};
use crate::transfer::delta::{match_delta, BasisIndex, DeltaMatchError, DeltaOp};
use bytes::Bytes;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub(crate) const PRODUCER_QUEUE_DEPTH: usize = 8;
const RECONSTRUCTION_QUEUE_DEPTH: usize = 8;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

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

#[derive(Debug, Default)]
struct CapturedTransferPreservation {
    xattrs: Option<Vec<(OsString, Vec<u8>)>>,
    acls: Option<String>,
}

fn read_transfer_preservation(
    rooted: &RootedFs,
    file: &File,
    path: &RelativePath,
    request: TransferPreservationRequest,
) -> Result<CapturedTransferPreservation> {
    let xattrs = request
        .xattrs
        .then(|| rooted.read_open_file_xattrs_blocking(file, path))
        .transpose()?;
    let acls = request
        .acls
        .then(|| rooted.read_open_file_acl_blocking(file, path))
        .transpose()?
        .map(|acl| acl.unwrap_or_default());
    Ok(CapturedTransferPreservation { xattrs, acls })
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

    #[error("delta transfer requires a regular-file basis at the source path")]
    InvalidBasis,

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

    #[error("file transfer acknowledgement payload must be empty")]
    NonEmptyAck,

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

enum ReconstructionOp {
    Data(DataChunk),
    Copy(WireDeltaCopy),
    Xattrs(Vec<(OsString, Vec<u8>)>),
    Acls(String),
    End(WireFileEnd),
}

struct PreparedReconstruction {
    staged: RootedStagedFile,
    /// Copy-op basis: only present when the destination is a regular file.
    basis: Option<(File, WireFileBasis)>,
    rooted: RootedFs,
    relative: RelativePath,
    expectation: Option<WireFileBasis>,
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
    let observed = rooted.path_identity_blocking(relative)?;
    let path = relative.as_path().to_path_buf();
    match expectation {
        Some(expected) => match observed {
            Some((_, identity)) if identity.as_bytes() == &expected.identity() => Ok(()),
            _ => Err(RemoteTransferError::DestinationChanged { path }),
        },
        None if observed.is_some() => Err(RemoteTransferError::UnexpectedDestination { path }),
        None => Ok(()),
    }
}

/// The scanned destination state one transfer commits against.
///
/// `None` at the call site is a create: the receiver requires the destination
/// path to remain absent. Updates carry the scanned identity expectation so a
/// racing replacement aborts staging before commit; delta transfers add the
/// signature index produced from that same destination entry.
#[derive(Debug)]
pub struct TransferDestination {
    pub expectation: WireFileBasis,
    pub delta_index: Option<BasisIndex>,
}

impl TransferDestination {
    pub const fn whole(expectation: WireFileBasis) -> Self {
        Self {
            expectation,
            delta_index: None,
        }
    }

    pub const fn delta(expectation: WireFileBasis, delta_index: BasisIndex) -> Self {
        Self {
            expectation,
            delta_index: Some(delta_index),
        }
    }
}

pub async fn request_file_transfer(
    sender: &RouterSender,
    source_root: PathBuf,
    source: Entry,
    destination: Option<TransferDestination>,
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
    source_root: PathBuf,
    source: Entry,
    destination: Option<TransferDestination>,
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
    source_root: PathBuf,
    source: Entry,
    destination: Option<TransferDestination>,
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
        },
    )
    .await
}

pub async fn request_file_transfer_with_stream_policy(
    sender: &RouterSender,
    source_root: PathBuf,
    source: Entry,
    destination: Option<TransferDestination>,
    metadata: TransferMetadata,
    peer: PlatformOs,
    stream_policy: TransferStreamPolicy,
) -> Result<TransferSummary> {
    ensure_compatible_path_encoding(peer)?;
    if !source.is_file() {
        return Err(RemoteTransferError::InvalidSource);
    }
    let expected_identity = source
        .identity
        .ok_or(RemoteTransferError::MissingSourceIdentity)?;

    let rooted = RootedFs::open(source_root).await?;
    let source_path = source.path.clone();
    let expected_size = source.size;
    let preservation_request = stream_policy.preservation;
    let compression = stream_policy.compression;
    let (source_file, captured_preservation) = tokio::task::spawn_blocking(move || {
        let file = rooted.open_regular_blocking(&source_path)?;
        validate_source(&file, expected_identity, expected_size)?;
        let preservation =
            read_transfer_preservation(&rooted, &file, &source_path, preservation_request)?;
        Ok::<_, RemoteTransferError>((file, preservation))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;
    let mut metadata = metadata;
    if preservation_request.xattrs {
        metadata.xattrs = captured_preservation.xattrs;
    }
    if preservation_request.acls {
        metadata.acls = captured_preservation.acls;
    }

    let encoded_path = encode_relative_path(source.path.as_path())?;
    let (begin, basis_index) = match destination {
        // The expectation is the scanned destination identity; delta
        // transfers reuse it as their copy-op basis. The receiver refuses
        // racing replacements before commit in both cases.
        Some(destination) => (
            WireFileBegin::delta(encoded_path, source.size, destination.expectation),
            destination.delta_index,
        ),
        // A create: the receiver requires the path to remain absent.
        None => (WireFileBegin::whole(encoded_path, source.size), None),
    };
    let begin = begin.with_metadata(
        metadata.unix_mode,
        metadata
            .modified
            .map(|value| (value.seconds(), value.nanoseconds())),
    )?;

    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::FileBegin,
            FrameFlags::empty(),
            stream_id,
            begin.encode(),
        )?)
        .await?;

    let (producer_tx, producer_rx) = mpsc::channel(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        produce_source(
            source_file,
            expected_identity,
            expected_size,
            basis_index,
            producer_tx,
            compression,
        )
    });

    let summary = send_produced_file(sender, stream_id, producer_rx, producer).await?;
    // Preservation rides the transfer stream so the server can apply it to
    // private staging before commit; a failure there aborts the replacement.
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
    sender
        .send(Frame::new(
            FrameKind::FileEnd,
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            stream_id,
            WireFileEnd::new(summary.file_size, summary.digest).encode(),
        )?)
        .await?;
    receive_ack(&mut inbox, stream_id).await?;
    Ok(summary)
}

/// Own the bounded producer queue and join its blocking worker on every send
/// outcome. Closing the receiver before joining wakes a backpressured producer
/// when the transport fails instead of leaving it detached.
pub(super) async fn send_produced_file(
    sender: &RouterSender,
    stream_id: StreamId,
    mut producer_rx: mpsc::Receiver<ProducerItem>,
    producer: tokio::task::JoinHandle<Result<TransferSummary>>,
) -> Result<TransferSummary> {
    let sent: Result<()> = async {
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
    }
    .await;
    drop(producer_rx);
    let produced = producer
        .await
        .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))?;
    sent?;
    produced
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
    let first_frame = first.frame();
    require_stream(first_frame, stream_id)?;
    require_empty_flags(first_frame)?;
    if first_frame.kind() != FrameKind::FileBegin {
        return Err(RemoteTransferError::UnexpectedFrame {
            expected: FrameKind::FileBegin,
            actual: first_frame.kind(),
        });
    }
    let begin = WireFileBegin::decode(first_frame.payload())?;
    let relative = decode_relative_path(begin.path.clone(), peer)?;
    drop(first);

    let basis = begin.basis();
    let prepared =
        tokio::task::spawn_blocking(move || prepare_reconstruction(rooted, &relative, basis))
            .await
            .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))??;

    let (reconstruction_tx, reconstruction_rx) = mpsc::channel(RECONSTRUCTION_QUEUE_DEPTH);
    let begin_for_worker = begin.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        reconstruct_file(prepared, begin_for_worker, reconstruction_rx)
    });

    // A malformed frame closes admission and joins the staging owner before
    // returning. Also observe worker failure while waiting for peer input: a
    // rejected chunk must not require another frame to wake the receiver.
    let summary = tokio::select! {
        received = receive_reconstruction(&mut inbox, &begin, &reconstruction_tx) => {
            drop(reconstruction_tx);
            let reconstructed = await_reconstruction(worker).await;
            match (received, reconstructed) {
                (Ok(()), result) => result?,
                (Err(RemoteTransferError::ReconstructionStopped), Err(error)) => return Err(error),
                (Err(error), _) => return Err(error),
            }
        }
        reconstructed = &mut worker => {
            drop(reconstruction_tx);
            reconstructed
                .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))??
        }
    };
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            Bytes::new(),
        )?)
        .await?;
    Ok(summary)
}

async fn receive_reconstruction(
    inbox: &mut StreamInbox,
    begin: &WireFileBegin,
    reconstruction_tx: &mpsc::Sender<ReconstructionOp>,
) -> Result<()> {
    let stream_id = inbox.stream_id();
    let mut seen_xattrs = false;
    let mut seen_acls = false;

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
            FrameKind::Data => ReconstructionOp::Data(DataChunk::from_frame(frame)?),
            FrameKind::DeltaCopy => {
                require_empty_flags(frame)?;
                if begin.basis().is_none() {
                    return Err(RemoteTransferError::CopyWithoutBasis);
                }
                ReconstructionOp::Copy(WireDeltaCopy::decode(frame.payload())?)
            }
            FrameKind::FileXattrs => {
                require_empty_flags(frame)?;
                if seen_xattrs {
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
                if seen_acls {
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
                require_file_end_flags(frame)?;
                ReconstructionOp::End(WireFileEnd::decode(frame.payload())?)
            }
            actual => {
                return Err(RemoteTransferError::UnexpectedFrame {
                    expected: FrameKind::FileEnd,
                    actual,
                });
            }
        };
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

fn produce_source(
    mut file: File,
    expected_identity: EntryIdentity,
    expected_size: u64,
    basis: Option<BasisIndex>,
    sender: mpsc::Sender<ProducerItem>,
    compression: Option<crate::engine::compression::CompressionPolicy>,
) -> Result<TransferSummary> {
    let summary = if let Some(basis) = basis {
        let delta = match_delta(&mut file, &basis, |op| {
            let item = match op {
                DeltaOp::Literal(bytes) => ProducerItem::Data(bytes),
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
    use crate::engine::compression::{
        choose_for_min_elapsed, ByteRate, CompressionChoice, CompressionPolicy, CompressionSample,
        CompressionTiming, DEFAULT_LINK_RATE_BYTES_PER_SEC,
    };

    let mut buffer = vec![0_u8; MAX_TRANSFER_DATA_SIZE];
    let mut hasher = blake3::Hasher::new();
    let mut file_size = 0_u64;

    // Auto samples once to choose whether later chunks are worth attempting.
    // Even Always sends raw bytes when a particular chunk would expand.
    let mut decision = match compression {
        Some(CompressionPolicy::Always) | Some(CompressionPolicy::Auto) => {
            CompressionChoice::ZstdFast
        }
        None => CompressionChoice::None,
    };
    let mut first_chunk = compression == Some(&CompressionPolicy::Auto);

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

        let compressed = if decision == CompressionChoice::ZstdFast {
            Some(compress_chunk(bytes)?)
        } else {
            None
        };
        if first_chunk {
            first_chunk = false;
            let compressed = compressed.as_ref().expect("Auto samples the first chunk");
            let sample = CompressionSample::new(read as u64, compressed.len() as u64)
                .expect("chunk read is non-zero");
            let timing = CompressionTiming::new(
                ByteRate::new(DEFAULT_LINK_RATE_BYTES_PER_SEC).expect("constant is non-zero"),
                // Initial codec-rate estimates, not measurements of this host.
                // The model selects representation; it does not provide a
                // throughput guarantee or change transfer integrity.
                ByteRate::new(800 * 1024 * 1024).unwrap(),
                ByteRate::new(1600 * 1024 * 1024).unwrap(),
                std::num::NonZeroU32::new(MAX_TRANSFER_DATA_SIZE as u32)
                    .expect("chunk size is non-zero"),
            );
            decision = choose_for_min_elapsed(expected_size, sample, timing);
        }

        let item = match compressed {
            Some(compressed)
                if decision == CompressionChoice::ZstdFast && compressed.len() < read =>
            {
                ProducerItem::CompressedData(compressed)
            }
            _ => ProducerItem::Data(Bytes::copy_from_slice(bytes)),
        };
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

fn prepare_reconstruction(
    rooted: RootedFs,
    relative: &RelativePath,
    expectation: Option<WireFileBasis>,
) -> Result<PreparedReconstruction> {
    let observed = rooted.path_identity_blocking(relative)?;
    let path = relative.as_path().to_path_buf();
    let basis = match expectation {
        Some(expected) => match observed {
            Some((kind, identity)) if identity.as_bytes() == &expected.identity() => {
                if kind == EntryKind::File {
                    // The copy-op basis is the old destination itself; the
                    // opened handle is revalidated at completion.
                    let file = rooted.open_regular_blocking(relative)?;
                    validate_basis(&file, expected)?;
                    Some((file, expected))
                } else {
                    // Non-regular destination (type transition): whole bytes
                    // only, with the identity expectation still enforced.
                    None
                }
            }
            _ => return Err(RemoteTransferError::DestinationChanged { path }),
        },
        None => {
            if observed.is_some() {
                return Err(RemoteTransferError::UnexpectedDestination { path });
            }
            None
        }
    };
    let staged = rooted.begin_staged_file_blocking(relative)?;
    Ok(PreparedReconstruction {
        staged,
        basis,
        rooted,
        relative: relative.clone(),
        expectation,
    })
}

fn reconstruct_file(
    mut prepared: PreparedReconstruction,
    begin: WireFileBegin,
    mut receiver: mpsc::Receiver<ReconstructionOp>,
) -> Result<TransferSummary> {
    let mut hasher = blake3::Hasher::new();
    let mut file_size = 0_u64;
    let mut literal_bytes = 0_u64;
    let mut reused_bytes = 0_u64;
    // Preservation frames buffer as bounded sets and are applied to staging
    // between metadata and commit, matching the local transaction lifecycle.
    let mut xattrs: Option<Vec<(OsString, Vec<u8>)>> = None;
    let mut acls: Option<String> = None;

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
                let Some((basis, expected)) = prepared.basis.as_mut() else {
                    return Err(RemoteTransferError::CopyWithoutBasis);
                };
                file_size = checked_output_size(
                    file_size,
                    usize::try_from(copy.copy_len())
                        .map_err(|_| RemoteTransferError::ByteCountOverflow)?,
                    begin.file_size(),
                )?;
                copy_basis_range(
                    basis,
                    *expected,
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
                prepared
                    .staged
                    .apply_metadata_blocking(begin.unix_mode(), modified)?;
                prepared.staged.apply_preservation_blocking(
                    xattrs.as_deref(),
                    acls.as_deref(),
                    begin.unix_mode(),
                )?;
                if prepared.staged.staged_hash_blocking()?.as_bytes() != &digest {
                    return Err(RemoteTransferError::DigestMismatch);
                }
                // The scanned destination state must still hold at commit.
                validate_destination_state(
                    &prepared.rooted,
                    &prepared.relative,
                    prepared.expectation,
                )?;
                if let Some((basis, expected)) = prepared.basis.as_ref() {
                    validate_basis(basis, *expected)?;
                }
                prepared.staged.commit()?;
                return Ok(TransferSummary {
                    file_size,
                    digest,
                    literal_bytes,
                    reused_bytes,
                });
            }
        }
    }

    Err(RemoteTransferError::MissingFileEnd)
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

fn validate_basis(file: &File, expected: WireFileBasis) -> Result<()> {
    let metadata = file.metadata()?;
    let identity = opened_identity(&metadata)?;
    if metadata.len() != expected.file_size() || identity.as_bytes() != &expected.identity() {
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

pub(crate) async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
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
    if !frame.payload().is_empty() {
        return Err(RemoteTransferError::NonEmptyAck);
    }
    Ok(())
}

async fn await_reconstruction(
    worker: tokio::task::JoinHandle<Result<TransferSummary>>,
) -> Result<TransferSummary> {
    worker
        .await
        .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))?
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
mod tests {
    use super::*;
    use crate::engine::rolling::WeakChecksum;
    use crate::protocol::Operation;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
    use crate::remote::{client_handshake, server_handshake};
    use crate::transfer::delta::{BasisBlock, BasisIndexLimits};
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
    async fn whole_file_stream_commits_only_after_verified_file_end() {
        let source_root = tempfile::TempDir::new().unwrap();
        let destination_root = tempfile::TempDir::new().unwrap();
        let data = vec![0x5a_u8; MAX_TRANSFER_DATA_SIZE * 2 + 17];
        std::fs::write(source_root.path().join("file.bin"), &data).unwrap();
        std::fs::write(destination_root.path().join("file.bin"), b"old").unwrap();
        let source = file_entry(source_root.path(), "file.bin");

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
            source_root.path().to_path_buf(),
            source,
            Some(TransferDestination::whole(destination_expectation)),
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
        let source = file_entry(source_root.path(), "file.bin");
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
            source_root.path().to_path_buf(),
            source,
            Some(TransferDestination::delta(expectation, delta_index)),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn push_preservation_capture_stays_on_held_source_after_path_replacement() {
        let source_root = tempfile::TempDir::new().unwrap();
        let source_path = source_root.path().join("file.bin");
        let replacement_path = source_root.path().join("replacement.bin");
        std::fs::write(&source_path, b"held source").unwrap();
        std::fs::write(&replacement_path, b"replacement").unwrap();
        xattr::set(&source_path, "user.sy_test", b"held").unwrap();
        xattr::set(&replacement_path, "user.sy_test", b"replacement").unwrap();

        let rooted = RootedFs::open(source_root.path().to_path_buf())
            .await
            .unwrap();
        let relative = RelativePath::new(PathBuf::from("file.bin")).unwrap();
        let (rooted, file, relative) = tokio::task::spawn_blocking(move || {
            let file = rooted.open_regular_blocking(&relative)?;
            validate_source(
                &file,
                crate::endpoint::local_identity::metadata_identity(
                    &file.metadata()?,
                    EntryKind::File,
                )
                .ok_or(RemoteTransferError::MissingOpenedIdentity)?,
                b"held source".len() as u64,
            )?;
            Ok::<_, RemoteTransferError>((rooted, file, relative))
        })
        .await
        .unwrap()
        .unwrap();
        std::fs::rename(&replacement_path, &source_path).unwrap();

        let preservation = tokio::task::spawn_blocking(move || {
            read_transfer_preservation(
                &rooted,
                &file,
                &relative,
                TransferPreservationRequest {
                    xattrs: true,
                    acls: false,
                },
            )
        })
        .await
        .unwrap()
        .unwrap();

        let xattrs = preservation.xattrs.unwrap();
        assert_eq!(
            xattrs
                .iter()
                .find(|(name, _)| name == "user.sy_test")
                .map(|(_, value)| value.as_slice()),
            Some(&b"held"[..])
        );
        assert!(preservation.acls.is_none());
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
        let worker = tokio::task::spawn_blocking(move || reconstruct_file(prepared, begin, rx));
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
        let worker = tokio::task::spawn_blocking(move || reconstruct_file(prepared, begin, rx));
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
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
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
        let worker = tokio::task::spawn_blocking(move || reconstruct_file(prepared, begin, rx));
        tx.send(ReconstructionOp::Data(DataChunk::Plain(
            Bytes::from_static(b"new"),
        )))
        .await
        .unwrap();
        tx.send(ReconstructionOp::Xattrs(vec![(
            OsString::from("user.sy_test"),
            b"v".to_vec(),
        )]))
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
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("file")).unwrap();
        let prepared = tokio::task::spawn_blocking({
            let relative = relative.clone();
            move || prepare_reconstruction(rooted, &relative, None)
        })
        .await
        .unwrap()
        .unwrap();

        let wire_path = encode_relative_path(relative.as_path()).unwrap();
        let begin = WireFileBegin::whole(wire_path, 3);
        let (tx, rx) = mpsc::channel(4);
        let worker = tokio::task::spawn_blocking(move || reconstruct_file(prepared, begin, rx));
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

        assert!(worker.await.unwrap().is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
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
        let worker = tokio::task::spawn_blocking(move || reconstruct_file(prepared, begin, rx));
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
