use crate::engine::domain::RelativePath;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, SignatureBlockSize, StreamId,
    WireSignature, WireSignatureEnd, WireSignatureRequest, MAX_SIGNATURE_BLOCK_SIZE,
    MIN_SIGNATURE_BLOCK_SIZE, STRONG_SIGNATURE_LEN,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::rooted_fs::{RootedFs, RootedFsError};
use futures::{Stream, StreamExt};
use std::error::Error as StdError;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::sync::mpsc;

const TARGET_SIGNATURE_BLOCKS: u64 = 4096;
const PRODUCER_QUEUE_DEPTH: usize = 16;

pub type BoxSignatureError = Box<dyn StdError + Send + Sync + 'static>;
pub type SignatureStream =
    Pin<Box<dyn Stream<Item = std::result::Result<SignatureEvent, BoxSignatureError>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSignature {
    pub index: u64,
    pub size: u32,
    pub weak: u32,
    pub strong: [u8; STRONG_SIGNATURE_LEN],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureSummary {
    pub file_size: u64,
    pub block_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureEvent {
    Block(BlockSignature),
    End(SignatureSummary),
}

#[derive(Debug, thiserror::Error)]
pub enum SignatureProducerError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error("signature consumer closed while producer was still running")]
    ConsumerClosed,

    #[error("signature byte count overflow")]
    ByteCountOverflow,

    #[error("signature block count overflow")]
    BlockCountOverflow,
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteSignatureError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("frame router failed: {0}")]
    Router(SharedRouterError),

    #[error(transparent)]
    Path(#[from] RemotePathError),

    #[error(transparent)]
    RootedFs(#[from] RootedFsError),

    #[error(transparent)]
    Producer(#[from] SignatureProducerError),

    #[error("signature producer task failed: {0}")]
    ProducerJoin(String),

    #[error("expected signature frame {expected:?}, got {actual:?}")]
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },

    #[error("signature frame arrived on stream {actual}, expected {expected}")]
    StreamMismatch { expected: u32, actual: u32 },

    #[error("signature frame {kind:?} used unsupported flags 0x{flags:02x}")]
    FrameFlags { kind: FrameKind, flags: u8 },

    #[error("SignatureEnd must use ACK_REQUIRED and no other flags, got 0x{flags:02x}")]
    SignatureEndFlags { flags: u8 },

    #[error("signature acknowledgement payload must be empty")]
    NonEmptyAck,

    #[error("signature stream {stream_id} ended before {expected:?}")]
    UnexpectedStreamEnd { stream_id: u32, expected: FrameKind },

    #[error("signature index mismatch: expected {expected}, got {actual}")]
    SignatureIndex { expected: u64, actual: u64 },

    #[error("signature block {index} followed a short final block")]
    SignatureAfterShortBlock { index: u64 },

    #[error("signature block is too large: {size} bytes (requested {requested})")]
    SignatureBlockTooLarge { size: u32, requested: u32 },

    #[error("signature byte count overflow")]
    ByteCountOverflow,

    #[error("signature block count overflow")]
    BlockCountOverflow,

    #[error(
        "signature summary mismatch: received {actual_count} blocks/{actual_size} bytes, expected {expected_count} blocks/{expected_size} bytes"
    )]
    SummaryMismatch {
        expected_count: u64,
        actual_count: u64,
        expected_size: u64,
        actual_size: u64,
    },
}

impl From<SharedRouterError> for RemoteSignatureError {
    fn from(error: SharedRouterError) -> Self {
        Self::Router(error)
    }
}

pub type Result<T> = std::result::Result<T, RemoteSignatureError>;

/// Choose a power-of-two block size targeting roughly 4096 signatures per file.
/// The protocol bounds keep the common case between 4 KiB and 1 MiB.
pub fn choose_signature_block_size(file_size: u64) -> u32 {
    let ideal = file_size.div_ceil(TARGET_SIGNATURE_BLOCKS).max(1);
    let rounded = ideal.checked_next_power_of_two().unwrap_or(u64::MAX);
    rounded.clamp(
        u64::from(MIN_SIGNATURE_BLOCK_SIZE),
        u64::from(MAX_SIGNATURE_BLOCK_SIZE),
    ) as u32
}

/// Request destination rolling signatures without materializing the signature
/// set. The returned stream validates order, block sizing, and final byte/count
/// totals before acknowledging completion.
pub async fn request_signatures(
    sender: &RouterSender,
    path: &RelativePath,
    file_size: u64,
    peer: PlatformOs,
) -> Result<(u32, SignatureStream)> {
    ensure_compatible_path_encoding(peer)?;
    let block_size = choose_signature_block_size(file_size);
    let block_size_wire = SignatureBlockSize::new(block_size)?;
    let inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();

    let request = WireSignatureRequest {
        path: encode_relative_path(path.as_path())?,
        block_size: block_size_wire,
    };
    let frame = Frame::new(
        FrameKind::SignatureRequest,
        FrameFlags::empty(),
        stream_id,
        request.encode(),
    )?;
    sender.send(frame).await?;

    Ok((
        block_size,
        remote_signature_stream(inbox, sender.clone(), block_size)?,
    ))
}

/// Serve one peer-opened destination signature request.
///
/// The root is pinned before peer-controlled relative-path resolution. File
/// opening and hashing run on a blocking worker; every relative component is
/// opened beneath the held root with no-follow semantics. A small bounded
/// channel is the only producer queue, followed by the router's byte/frame
/// budgets.
pub async fn serve_incoming_signatures(
    root: &Path,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
) -> Result<()> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream_id = inbox.stream_id();
    let first_frame = first.frame();
    require_stream(first_frame, stream_id)?;
    let request = decode_signature_request(first_frame)?;
    let relative = decode_relative_path(request.path, peer)?;
    let block_size = request.block_size;
    drop(first);

    let rooted = RootedFs::open(root.to_path_buf()).await?;
    let (producer_tx, mut producer_rx) = mpsc::channel(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        produce_signatures(rooted, relative, block_size, producer_tx)
    });

    while let Some(signature) = producer_rx.recv().await {
        let frame = Frame::new(
            FrameKind::Signature,
            FrameFlags::empty(),
            stream_id,
            signature.encode(),
        )?;
        sender.send(frame).await?;
    }

    let summary = producer
        .await
        .map_err(|error| RemoteSignatureError::ProducerJoin(error.to_string()))??;
    let end = WireSignatureEnd::new(summary.file_size, summary.block_count)?;
    let frame = Frame::new(
        FrameKind::SignatureEnd,
        FrameFlags::ACK_REQUIRED,
        stream_id,
        end.encode(),
    )?;
    sender.send(frame).await?;
    receive_signature_ack(&mut inbox, stream_id).await
}

fn produce_signatures(
    rooted: RootedFs,
    relative: RelativePath,
    block_size: SignatureBlockSize,
    sender: mpsc::Sender<WireSignature>,
) -> std::result::Result<SignatureSummary, SignatureProducerError> {
    let mut file = rooted.open_regular_blocking(&relative)?;
    let block_size = block_size.get() as usize;
    let mut buffer = vec![0_u8; block_size];
    let mut file_size = 0_u64;
    let mut block_count = 0_u64;

    loop {
        let mut read = 0_usize;
        while read < block_size {
            match file.read(&mut buffer[read..]) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if read == 0 {
            break;
        }

        let block = &buffer[..read];
        let weak = crate::delta::Adler32::hash(block);
        let digest = blake3::hash(block);
        let mut strong = [0_u8; STRONG_SIGNATURE_LEN];
        strong.copy_from_slice(&digest.as_bytes()[..STRONG_SIGNATURE_LEN]);
        let size = u32::try_from(read).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "signature block size exceeds u32",
            )
        })?;
        let signature = WireSignature::new(block_count, size, weak, strong)?;
        sender
            .blocking_send(signature)
            .map_err(|_| SignatureProducerError::ConsumerClosed)?;

        file_size = file_size
            .checked_add(u64::from(size))
            .ok_or(SignatureProducerError::ByteCountOverflow)?;
        block_count = block_count
            .checked_add(1)
            .ok_or(SignatureProducerError::BlockCountOverflow)?;

        if read < block_size {
            break;
        }
    }

    Ok(SignatureSummary {
        file_size,
        block_count,
    })
}

async fn receive_signature_ack(inbox: &mut StreamInbox, stream_id: StreamId) -> Result<()> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteSignatureError::UnexpectedStreamEnd {
            stream_id: stream_id.get(),
            expected: FrameKind::Ack,
        })?;
    let frame = routed.frame();
    require_stream(frame, stream_id)?;
    require_empty_flags(frame)?;
    if frame.kind() != FrameKind::Ack {
        return Err(RemoteSignatureError::UnexpectedFrame {
            expected: FrameKind::Ack,
            actual: frame.kind(),
        });
    }
    if !frame.payload().is_empty() {
        return Err(RemoteSignatureError::NonEmptyAck);
    }
    Ok(())
}

fn decode_signature_request(frame: &Frame) -> Result<WireSignatureRequest> {
    require_empty_flags(frame)?;
    if frame.kind() != FrameKind::SignatureRequest {
        return Err(RemoteSignatureError::UnexpectedFrame {
            expected: FrameKind::SignatureRequest,
            actual: frame.kind(),
        });
    }
    WireSignatureRequest::decode(frame.payload()).map_err(Into::into)
}

struct SignatureReceiveState {
    inbox: StreamInbox,
    sender: RouterSender,
    stream_id: StreamId,
    block_size: u32,
    next_index: u64,
    byte_count: u64,
    short_block_seen: bool,
    done: bool,
}

fn remote_signature_stream(
    inbox: StreamInbox,
    sender: RouterSender,
    block_size: u32,
) -> Result<SignatureStream> {
    let stream_id = inbox.stream_id();
    let state = SignatureReceiveState {
        inbox,
        sender,
        stream_id,
        block_size,
        next_index: 0,
        byte_count: 0,
        short_block_seen: false,
        done: false,
    };

    let stream = futures::stream::try_unfold(state, |mut state| async move {
        if state.done {
            return Ok(None);
        }

        let routed =
            state
                .inbox
                .recv()
                .await?
                .ok_or(RemoteSignatureError::UnexpectedStreamEnd {
                    stream_id: state.stream_id.get(),
                    expected: FrameKind::SignatureEnd,
                })?;
        let frame = routed.frame();
        require_stream(frame, state.stream_id)?;

        let event = match frame.kind() {
            FrameKind::Signature => {
                require_empty_flags(frame)?;
                let signature = WireSignature::decode(frame.payload())?;
                if signature.index() != state.next_index {
                    return Err(RemoteSignatureError::SignatureIndex {
                        expected: state.next_index,
                        actual: signature.index(),
                    });
                }
                if state.short_block_seen {
                    return Err(RemoteSignatureError::SignatureAfterShortBlock {
                        index: signature.index(),
                    });
                }
                if signature.size() > state.block_size {
                    return Err(RemoteSignatureError::SignatureBlockTooLarge {
                        size: signature.size(),
                        requested: state.block_size,
                    });
                }

                state.short_block_seen = signature.size() < state.block_size;
                state.byte_count = state
                    .byte_count
                    .checked_add(u64::from(signature.size()))
                    .ok_or(RemoteSignatureError::ByteCountOverflow)?;
                state.next_index = state
                    .next_index
                    .checked_add(1)
                    .ok_or(RemoteSignatureError::BlockCountOverflow)?;

                SignatureEvent::Block(BlockSignature {
                    index: signature.index(),
                    size: signature.size(),
                    weak: signature.weak(),
                    strong: signature.strong(),
                })
            }
            FrameKind::SignatureEnd => {
                require_signature_end_flags(frame)?;
                let end = WireSignatureEnd::decode(frame.payload())?;
                if end.block_count() != state.next_index || end.file_size() != state.byte_count {
                    return Err(RemoteSignatureError::SummaryMismatch {
                        expected_count: state.next_index,
                        actual_count: end.block_count(),
                        expected_size: state.byte_count,
                        actual_size: end.file_size(),
                    });
                }

                let ack = Frame::new(
                    FrameKind::Ack,
                    FrameFlags::empty(),
                    state.stream_id,
                    bytes::Bytes::new(),
                )?;
                state.sender.send(ack).await?;
                state.done = true;
                SignatureEvent::End(SignatureSummary {
                    file_size: end.file_size(),
                    block_count: end.block_count(),
                })
            }
            actual => {
                return Err(RemoteSignatureError::UnexpectedFrame {
                    expected: FrameKind::Signature,
                    actual,
                });
            }
        };

        Ok(Some((event, state)))
    })
    .map(|result| result.map_err(|error| Box::new(error) as BoxSignatureError));

    Ok(Box::pin(stream))
}

fn require_stream(frame: &Frame, stream_id: StreamId) -> Result<()> {
    if frame.stream_id() == stream_id {
        Ok(())
    } else {
        Err(RemoteSignatureError::StreamMismatch {
            expected: stream_id.get(),
            actual: frame.stream_id().get(),
        })
    }
}

fn require_empty_flags(frame: &Frame) -> Result<()> {
    if frame.flags().is_empty() {
        Ok(())
    } else {
        Err(RemoteSignatureError::FrameFlags {
            kind: frame.kind(),
            flags: frame.flags().bits(),
        })
    }
}

fn require_signature_end_flags(frame: &Frame) -> Result<()> {
    if frame.flags() == FrameFlags::ACK_REQUIRED {
        Ok(())
    } else {
        Err(RemoteSignatureError::SignatureEndFlags {
            flags: frame.flags().bits(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Operation, Platform};
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
    use crate::remote::{client_handshake, server_handshake};

    #[test]
    fn adaptive_block_size_targets_bounded_power_of_two_blocks() {
        assert_eq!(choose_signature_block_size(0), 4 * 1024);
        assert_eq!(choose_signature_block_size(16 * 1024 * 1024), 4 * 1024);
        assert_eq!(choose_signature_block_size(16 * 1024 * 1024 + 1), 8 * 1024);
        assert_eq!(choose_signature_block_size(100 * 1024 * 1024), 32 * 1024);
        assert_eq!(
            choose_signature_block_size(4 * 1024 * 1024 * 1024),
            1024 * 1024
        );
        assert_eq!(choose_signature_block_size(u64::MAX), 1024 * 1024);
    }

    #[tokio::test]
    async fn handshake_and_routed_signatures_stream_without_materializing() {
        let root = tempfile::TempDir::new().unwrap();
        let data = (0..10_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(root.path().join("data.bin"), &data).unwrap();

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
            serve_incoming_signatures(&opened.root, incoming, &sender, opened.client.platform.os)
                .await
                .unwrap();
        });

        let session = client_handshake(
            &mut client_reader,
            &mut client_writer,
            Operation::Push,
            root.path(),
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
        let sender = router.sender();
        let path = RelativePath::new(PathBuf::from("data.bin")).unwrap();
        let (block_size, mut signatures) = request_signatures(
            &sender,
            &path,
            data.len() as u64,
            session.server.platform.os,
        )
        .await
        .unwrap();
        assert_eq!(block_size, 4 * 1024);

        let mut blocks = Vec::new();
        let mut summary = None;
        while let Some(event) = signatures.next().await {
            match event.unwrap() {
                SignatureEvent::Block(block) => blocks.push(block),
                SignatureEvent::End(end) => summary = Some(end),
            }
        }
        server.await.unwrap();

        assert_eq!(blocks.len(), 3);
        assert_eq!(
            blocks.iter().map(|block| block.size).collect::<Vec<_>>(),
            vec![4096, 4096, 1808]
        );
        assert_eq!(
            summary,
            Some(SignatureSummary {
                file_size: 10_000,
                block_count: 3
            })
        );
        assert_eq!(blocks[0].weak, crate::delta::Adler32::hash(&data[..4096]));
        let digest = blake3::hash(&data[..4096]);
        assert_eq!(blocks[0].strong, digest.as_bytes()[..STRONG_SIGNATURE_LEN]);
        assert_eq!(Platform::current().os, session.server.platform.os);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signature_basis_refuses_parent_symlink_escape() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let relative = RelativePath::new(PathBuf::from("escape/secret")).unwrap();
        let block_size = SignatureBlockSize::new(4 * 1024).unwrap();
        let (sender, _receiver) = mpsc::channel(1);

        let error = tokio::task::spawn_blocking(move || {
            produce_signatures(rooted, relative, block_size, sender)
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(matches!(error, SignatureProducerError::RootedFs(_)));
    }
}
