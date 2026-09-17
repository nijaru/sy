//! Whole-file source fetch for pull sessions.
//!
//! Push streams client→server (`FileBegin` … `FileEnd` on a client-opened
//! stream, server ACKs after staged reconstruction commits). Pull inverts the
//! direction with the same frame vocabulary and the same ordering rule: the
//! client requests a scanned source entry, the server validates the entry
//! identity (a source changed since the scan must fail loudly rather than
//! deliver mixed bytes), streams `Data` frames server→client, and terminates
//! with `FileEnd` carrying the BLAKE3 digest of the uncompressed source bytes.
//! The client acknowledges only after its staged copy verifies against that
//! digest and commits — the ack-after-commit ordering that keeps a failing
//! receiver from being reported as transferred.

use crate::engine::compression::CompressionPolicy;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, WireData, WireFileEnd, WireFileFetchRequest,
};
use crate::remote::path::{decode_relative_path, ensure_compatible_path_encoding};
use crate::remote::router::{IncomingStream, RouterSender};
use crate::rooted_fs::RootedFs;

use crate::remote::transfer::{RemoteTransferError, TransferSummary, PRODUCER_QUEUE_DEPTH};
use bytes::Bytes;
use tokio::sync::mpsc;

/// Client side: request one source file and stream it into `staged`.
///
/// `staged` receives the decompressed source bytes and must be positioned at
/// write start; `expected_size` and `identity` come from the source scan so
/// the server can detect a changed source before any bytes flow. The staged
/// bytes are verified against the server-reported BLAKE3 digest before this
/// returns; a mismatch is a transfer failure, not a soft warning.
pub async fn fetch_file(
    sender: &RouterSender,
    source: &crate::engine::domain::Entry,
    peer: PlatformOs,
    staged: &mut dyn crate::endpoint::io::StagedWriter,
    compression: Option<CompressionPolicy>,
    rate_limiter: Option<&std::sync::Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
) -> std::result::Result<TransferSummary, RemoteTransferError> {
    ensure_compatible_path_encoding(peer)?;
    if !source.is_file() {
        return Err(RemoteTransferError::InvalidSource);
    }
    let identity = source
        .identity
        .ok_or(RemoteTransferError::MissingSourceIdentity)?;
    let encoded = crate::remote::path::encode_relative_path(source.path.as_path())?;
    let request = WireFileFetchRequest::new(
        encoded,
        source.size,
        *identity.as_bytes(),
        compression.is_some(),
    );

    let mut inbox = sender.open_stream().map_err(RemoteTransferError::Router)?;
    let stream_id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::FileFetchRequest,
            FrameFlags::empty(),
            stream_id,
            request.encode(),
        )?)
        .await
        .map_err(RemoteTransferError::Router)?;

    let mut hasher = blake3::Hasher::new();
    let mut file_size = 0_u64;
    loop {
        let routed = inbox
            .recv()
            .await
            .map_err(RemoteTransferError::Router)?
            .ok_or(RemoteTransferError::UnexpectedStreamEnd {
                stream_id: stream_id.get(),
            })?;
        let frame = routed.frame();
        if frame.stream_id() != stream_id {
            return Err(RemoteTransferError::StreamMismatch {
                expected: stream_id.get(),
                actual: frame.stream_id().get(),
            });
        }
        match frame.kind() {
            FrameKind::Data => {
                let bytes = decode_data_frame(frame)?;
                file_size = file_size
                    .checked_add(
                        u64::try_from(bytes.len())
                            .map_err(|_| RemoteTransferError::ByteCountOverflow)?,
                    )
                    .ok_or(RemoteTransferError::ByteCountOverflow)?;
                if file_size > source.size {
                    return Err(RemoteTransferError::SourceChanged {
                        expected_size: source.size,
                        actual_size: file_size,
                    });
                }
                hasher.update(&bytes);
                // --bwlimit paces the pull at the userspace staging write:
                // slowing here backpressures the router's bounded inbound
                // queue, which stops the frame reader, which fills the SSH
                // transport window — the server cannot run ahead of the
                // client's configured rate.
                if let Some(limiter) = rate_limiter {
                    let sleep = limiter
                        .lock()
                        .map_err(|_| {
                            RemoteTransferError::Io(std::io::Error::other("rate limiter poisoned"))
                        })?
                        .consume(bytes.len() as u64);
                    if !sleep.is_zero() {
                        tokio::time::sleep(sleep).await;
                    }
                }
                staged.write(&bytes).await.map_err(|error| {
                    let message = error.to_string();
                    RemoteTransferError::Io(std::io::Error::other(message))
                })?;
            }
            FrameKind::FileEnd => {
                let expected_flags = FrameFlags::FINAL | FrameFlags::ACK_REQUIRED;
                if frame.flags() != expected_flags {
                    return Err(RemoteTransferError::FileEndFlags {
                        flags: frame.flags().bits(),
                    });
                }
                let end = WireFileEnd::decode(frame.payload())?;
                if end.file_size() != file_size {
                    return Err(RemoteTransferError::SourceChanged {
                        expected_size: end.file_size(),
                        actual_size: file_size,
                    });
                }
                let actual = *hasher.finalize().as_bytes();
                if actual != end.digest() {
                    return Err(RemoteTransferError::FetchDigestMismatch {
                        expected: end.digest(),
                        actual,
                    });
                }
                // Ack after the staged bytes are known good. The caller
                // commits the staging; this mirrors the push direction's
                // server-side ordering (reconstruct+verify, then Ack).
                sender
                    .send(Frame::new(
                        FrameKind::Ack,
                        FrameFlags::empty(),
                        stream_id,
                        Bytes::new(),
                    )?)
                    .await
                    .map_err(RemoteTransferError::Router)?;
                return Ok(TransferSummary {
                    file_size,
                    digest: end.digest(),
                    literal_bytes: file_size,
                    reused_bytes: 0,
                });
            }
            actual => {
                return Err(RemoteTransferError::UnexpectedFrame {
                    expected: FrameKind::FileEnd,
                    actual,
                });
            }
        }
    }
}

fn decode_data_frame(frame: &Frame) -> std::result::Result<Bytes, RemoteTransferError> {
    if frame.flags() == FrameFlags::COMPRESSED {
        let compressed = WireData::decode(frame.payload())?.into_bytes();
        let decompressed = zstd::stream::decode_all(compressed.as_ref())
            .map_err(|error| RemoteTransferError::Decompression(error.to_string()))?;
        Ok(Bytes::from(decompressed))
    } else if frame.flags().is_empty() {
        Ok(WireData::decode(frame.payload())?.into_bytes())
    } else {
        Err(RemoteTransferError::FrameFlags {
            kind: frame.kind(),
            flags: frame.flags().bits(),
        })
    }
}

/// Server side: serve one peer-opened file fetch stream.
///
/// The source entry is validated against the scan identity before streaming;
/// `FileEnd` carries the source digest; the client's final `Ack` is awaited so
/// the request task only finishes once the peer confirmed receipt, mirroring
/// the push-side reconstruction ordering.
pub async fn serve_incoming_file_fetch(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
) -> std::result::Result<TransferSummary, RemoteTransferError> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream_id = inbox.stream_id();
    let first_frame = first.frame();
    if first_frame.stream_id() != stream_id {
        return Err(RemoteTransferError::StreamMismatch {
            expected: stream_id.get(),
            actual: first_frame.stream_id().get(),
        });
    }
    if !first_frame.flags().is_empty() {
        return Err(RemoteTransferError::FrameFlags {
            kind: first_frame.kind(),
            flags: first_frame.flags().bits(),
        });
    }
    if first_frame.kind() != FrameKind::FileFetchRequest {
        return Err(RemoteTransferError::UnexpectedFrame {
            expected: FrameKind::FileFetchRequest,
            actual: first_frame.kind(),
        });
    }
    let request = WireFileFetchRequest::decode(first_frame.payload())?;
    // The per-request compression policy comes from the client; the
    // negotiated ZSTD capability was checked at handshake. Compression is
    // only applied when the client asked for it.
    let compression = request.compressed().then_some(CompressionPolicy::Auto);
    let relative = decode_relative_path(request.path.clone(), peer)?;
    drop(first);

    let expected_size = request.file_size();
    let expected_identity = crate::engine::domain::EntryIdentity::from_bytes(request.identity());
    let relative_for_worker = relative.clone();
    let source_file = tokio::task::spawn_blocking(move || {
        let file = rooted.open_regular_blocking(&relative_for_worker)?;
        crate::remote::transfer::validate_source(&file, expected_identity, expected_size)?;
        Ok::<_, RemoteTransferError>(file)
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;

    // The producer applies the negotiated per-chunk compression policy; the
    // COMPRESSED flag is set only on frames whose payload is actually zstd.
    let (producer_tx, mut producer_rx) =
        mpsc::channel::<crate::remote::transfer::ProducerItem>(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        crate::remote::transfer::produce_whole(
            &mut { source_file },
            producer_tx,
            compression.as_ref(),
        )
    });

    while let Some(item) = producer_rx.recv().await {
        let frame = match item {
            crate::remote::transfer::ProducerItem::Data(bytes) => Frame::new(
                FrameKind::Data,
                FrameFlags::empty(),
                stream_id,
                WireData::new(bytes)?.into_bytes(),
            )?,
            crate::remote::transfer::ProducerItem::CompressedData(bytes) => Frame::new(
                FrameKind::Data,
                FrameFlags::COMPRESSED,
                stream_id,
                WireData::new(bytes)?.into_bytes(),
            )?,
            crate::remote::transfer::ProducerItem::Copy(_) => {
                return Err(RemoteTransferError::InvalidBasis);
            }
        };
        sender
            .send(frame)
            .await
            .map_err(RemoteTransferError::Router)?;
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
        .await
        .map_err(RemoteTransferError::Router)?;
    crate::remote::transfer::receive_ack(&mut inbox, stream_id).await?;
    Ok(summary)
}
