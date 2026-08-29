use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath};
use crate::engine::reconcile::BoxError;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireData, WireDeltaCopy,
    WireFileBasis, WireFileBegin, WireFileEnd, MAX_TRANSFER_DATA_SIZE,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::rooted_fs::{RootedFs, RootedFsError, RootedStagedFile};
use crate::transfer::delta::{match_delta, BasisIndex, DeltaMatchError, DeltaOp};
use bytes::Bytes;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;

const PRODUCER_QUEUE_DEPTH: usize = 8;
const RECONSTRUCTION_QUEUE_DEPTH: usize = 8;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug)]
pub struct RemoteDeltaBasis {
    pub entry: Entry,
    pub index: BasisIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferSummary {
    pub file_size: u64,
    pub digest: [u8; 32],
    pub literal_bytes: u64,
    pub reused_bytes: u64,
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

    #[error("file transfer requires a scanned source identity")]
    MissingSourceIdentity,

    #[error("delta transfer requires a regular-file basis at the source path")]
    InvalidBasis,

    #[error("delta transfer requires a scanned destination basis identity")]
    MissingBasisIdentity,

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
}

impl From<SharedRouterError> for RemoteTransferError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteTransferError>;

enum ProducerItem {
    Data(Bytes),
    Copy(WireDeltaCopy),
}

enum ReconstructionOp {
    Data(Bytes),
    Copy(WireDeltaCopy),
    End(WireFileEnd),
}

struct PreparedReconstruction {
    staged: RootedStagedFile,
    basis: Option<(File, WireFileBasis)>,
}

/// Stream one local regular file to a negotiated peer without materializing the
/// file or a delta plan in memory.
///
/// The source is opened beneath a pinned local root and checked against its scan
/// identity before any FileBegin is sent. A blocking producer feeds a small
/// bounded channel; the router supplies the second bounded backpressure layer.
/// Delta mode consumes the already-bounded signature index and emits Data or
/// DeltaCopy frames directly. The source handle is revalidated after the single
/// read/match pass before FileEnd is sent.
pub async fn request_file_transfer(
    sender: &RouterSender,
    source_root: PathBuf,
    source: Entry,
    delta_basis: Option<RemoteDeltaBasis>,
    peer: PlatformOs,
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
    let source_file = tokio::task::spawn_blocking(move || {
        let file = rooted.open_regular_blocking(&source_path)?;
        validate_source(&file, expected_identity, expected_size)?;
        Ok::<_, RemoteTransferError>(file)
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;

    let encoded_path = encode_relative_path(source.path.as_path())?;
    let (begin, basis_index) = match delta_basis {
        Some(delta) => {
            if !delta.entry.is_file() || delta.entry.path != source.path {
                return Err(RemoteTransferError::InvalidBasis);
            }
            let identity = delta
                .entry
                .identity
                .ok_or(RemoteTransferError::MissingBasisIdentity)?;
            let basis = WireFileBasis::new(delta.entry.size, *identity.as_bytes());
            (
                WireFileBegin::delta(encoded_path, source.size, basis),
                Some(delta.index),
            )
        }
        None => (WireFileBegin::whole(encoded_path, source.size), None),
    };

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

    let (producer_tx, mut producer_rx) = mpsc::channel(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        produce_source(
            source_file,
            expected_identity,
            expected_size,
            basis_index,
            producer_tx,
        )
    });

    while let Some(item) = producer_rx.recv().await {
        let frame = match item {
            ProducerItem::Data(bytes) => Frame::new(
                FrameKind::Data,
                FrameFlags::empty(),
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

    let summary = producer
        .await
        .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;
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

/// Reconstruct one peer-opened file stream into a same-directory staging file.
///
/// FileBegin is decoded and any delta basis is securely opened and identity-
/// checked before follow-up frames are accepted. The blocking reconstruction
/// worker owns the pinned basis and staged writer. FileEnd size and BLAKE3 are
/// verified before atomic rename, and ACK is emitted only after that commit.
pub async fn serve_incoming_file(
    root: PathBuf,
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

    let rooted = RootedFs::open(root).await?;
    let basis = begin.basis();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_reconstruction(rooted, &relative, basis)
    })
    .await
    .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))??;

    let (reconstruction_tx, reconstruction_rx) = mpsc::channel(RECONSTRUCTION_QUEUE_DEPTH);
    let begin_for_worker = begin.clone();
    let mut worker = Some(tokio::task::spawn_blocking(move || {
        reconstruct_file(prepared, begin_for_worker, reconstruction_rx)
    }));

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
                require_empty_flags(frame)?;
                ReconstructionOp::Data(WireData::decode(frame.payload())?.into_bytes())
            }
            FrameKind::DeltaCopy => {
                require_empty_flags(frame)?;
                if begin.basis().is_none() {
                    return Err(RemoteTransferError::CopyWithoutBasis);
                }
                ReconstructionOp::Copy(WireDeltaCopy::decode(frame.payload())?)
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
        if reconstruction_tx.send(op).await.is_err() {
            let worker = worker.take().expect("reconstruction worker exists");
            return match await_reconstruction(worker).await {
                Ok(_) => Err(RemoteTransferError::ReconstructionStopped),
                Err(error) => Err(error),
            };
        }
        drop(routed);

        if is_end {
            drop(reconstruction_tx);
            let summary = await_reconstruction(
                worker
                    .take()
                    .expect("reconstruction worker exists at FileEnd"),
            )
            .await?;
            sender
                .send(Frame::new(
                    FrameKind::Ack,
                    FrameFlags::empty(),
                    stream_id,
                    Bytes::new(),
                )?)
                .await?;
            return Ok(summary);
        }
    }
}

fn produce_source(
    mut file: File,
    expected_identity: EntryIdentity,
    expected_size: u64,
    basis: Option<BasisIndex>,
    sender: mpsc::Sender<ProducerItem>,
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
        produce_whole(&mut file, sender)?
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

fn produce_whole(file: &mut File, sender: mpsc::Sender<ProducerItem>) -> Result<TransferSummary> {
    let mut buffer = vec![0_u8; MAX_TRANSFER_DATA_SIZE];
    let mut hasher = blake3::Hasher::new();
    let mut file_size = 0_u64;

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
        sender
            .blocking_send(ProducerItem::Data(Bytes::copy_from_slice(bytes)))
            .map_err(|_| {
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
    basis: Option<WireFileBasis>,
) -> Result<PreparedReconstruction> {
    let basis = match basis {
        Some(expected) => {
            let file = rooted.open_regular_blocking(relative)?;
            validate_basis(&file, expected)?;
            Some((file, expected))
        }
        None => None,
    };
    let staged = rooted.begin_staged_file_blocking(relative)?;
    Ok(PreparedReconstruction { staged, basis })
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

    while let Some(op) = receiver.blocking_recv() {
        match op {
            ReconstructionOp::Data(bytes) => {
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
                    usize::try_from(copy.len())
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
                    .checked_add(u64::from(copy.len()))
                    .ok_or(RemoteTransferError::ByteCountOverflow)?;
            }
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
    let mut remaining = usize::try_from(copy.len())
        .map_err(|_| RemoteTransferError::ByteCountOverflow)?;
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

fn validate_source(file: &File, expected: EntryIdentity, expected_size: u64) -> Result<()> {
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
    if metadata.len() != expected.file_size()
        || identity.as_bytes() != &expected.identity()
    {
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

async fn receive_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
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
    use crate::engine::domain::Timestamp;
    use crate::engine::rolling::WeakChecksum;
    use crate::protocol::Operation;
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
    use crate::remote::{client_handshake, server_handshake};
    use crate::transfer::delta::{BasisBlock, BasisIndexLimits};
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
        BasisIndex::new(
            block_size as u32,
            blocks,
            BasisIndexLimits::default(),
        )
        .unwrap()
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
            serve_incoming_file(
                opened.root,
                incoming,
                &sender,
                opened.client.platform.os,
            )
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
            None,
            session.server.platform.os,
        )
        .await
        .unwrap();
        let received = server.await.unwrap();

        assert_eq!(summary, received);
        assert_eq!(summary.file_size, data.len() as u64);
        assert_eq!(summary.literal_bytes, data.len() as u64);
        assert_eq!(summary.reused_bytes, 0);
        assert_eq!(std::fs::read(destination_root.path().join("file.bin")).unwrap(), data);
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
        let delta = RemoteDeltaBasis {
            entry: basis_entry,
            index: basis_index(destination, 4),
        };

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
            serve_incoming_file(
                opened.root,
                incoming,
                &sender,
                opened.client.platform.os,
            )
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
            Some(delta),
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
    async fn bad_digest_drops_stage_and_preserves_destination() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("file"), b"old").unwrap();
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
        let (tx, rx) = mpsc::channel(2);
        let worker = tokio::task::spawn_blocking(move || reconstruct_file(prepared, begin, rx));
        tx.send(ReconstructionOp::Data(Bytes::from_static(b"new")))
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
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
