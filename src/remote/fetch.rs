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

use crate::endpoint::{io::Preservation, Capabilities};
use crate::engine::compression::CompressionPolicy;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolVersion, StreamId, WireAcl, WireAclResult,
    WireData, WireFileEnd, WireFileFetchRequest, WireXattr, WireXattrResult, PROTOCOL_V3_2,
};
use crate::remote::path::{decode_relative_path, ensure_compatible_path_encoding};
use crate::remote::router::{IncomingStream, RouterSender, StreamInbox};
use crate::rooted_fs::RootedFs;

use crate::remote::transfer::{RemoteTransferError, TransferSummary, PRODUCER_QUEUE_DEPTH};
use bytes::Bytes;
use std::ffi::OsString;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Optional source preservation data requested as part of the same file-fetch
/// transaction. The server reads it from the validated open source handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchPreservationRequest {
    pub xattrs: bool,
    pub acls: bool,
}

/// A verified transfer stream and the preservation payload associated with it.
#[derive(Debug)]
pub struct FetchResult {
    pub stream_id: StreamId,
    pub summary: TransferSummary,
    pub preservation: Preservation,
}

/// Per-fetch policy, including capabilities/version needed to reject a
/// request that the peer cannot satisfy before staging begins.
pub struct FetchPolicy<'a> {
    pub compression: Option<CompressionPolicy>,
    pub rate_limiter: Option<&'a Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    pub preservation: FetchPreservationRequest,
    pub protocol_version: ProtocolVersion,
    pub capabilities: &'a Capabilities,
}

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
    policy: FetchPolicy<'_>,
) -> std::result::Result<FetchResult, RemoteTransferError> {
    ensure_compatible_path_encoding(peer)?;
    for (requested, supported, feature) in [
        (
            policy.preservation.xattrs,
            policy.capabilities.preserve_xattrs,
            "xattrs",
        ),
        (
            policy.preservation.acls,
            policy.capabilities.preserve_acls,
            "ACLs",
        ),
    ] {
        if requested && (!supported || policy.protocol_version < PROTOCOL_V3_2) {
            return Err(RemoteTransferError::PreservationUnavailable { feature });
        }
    }
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
        policy.compression.is_some(),
    )
    .with_preservation(policy.preservation.xattrs, policy.preservation.acls);

    let mut inbox = sender.open_stream().map_err(RemoteTransferError::Router)?;
    let stream_id = inbox.stream_id();
    let result = async {
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
        let mut data_started = false;
        let mut xattrs: Option<Vec<(OsString, Vec<u8>)>> = None;
        let mut acl: Option<String> = None;
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
                FrameKind::FileXattrs => {
                    if !frame.flags().is_empty() {
                        return Err(RemoteTransferError::FrameFlags {
                            kind: frame.kind(),
                            flags: frame.flags().bits(),
                        });
                    }
                    if !policy.preservation.xattrs || data_started || xattrs.is_some() {
                        return Err(RemoteTransferError::UnexpectedFetchPreservation("xattrs"));
                    }
                    let result = WireXattrResult::decode(frame.payload())?;
                    xattrs = Some(
                        result
                            .entries()
                            .iter()
                            .map(|entry| {
                                (
                                    crate::remote::xattr::os_string_from_name(entry.name()),
                                    entry.value().to_vec(),
                                )
                            })
                            .collect(),
                    );
                }
                FrameKind::FileAcls => {
                    if !frame.flags().is_empty() {
                        return Err(RemoteTransferError::FrameFlags {
                            kind: frame.kind(),
                            flags: frame.flags().bits(),
                        });
                    }
                    if !policy.preservation.acls || data_started || acl.is_some() {
                        return Err(RemoteTransferError::UnexpectedFetchPreservation("ACLs"));
                    }
                    acl = Some(
                        WireAclResult::decode(frame.payload())?
                            .acl()
                            .text()
                            .to_string(),
                    );
                }
                FrameKind::Data => {
                    data_started = true;
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
                    if let Some(limiter) = policy.rate_limiter {
                        let sleep = limiter
                            .lock()
                            .map_err(|_| {
                                RemoteTransferError::Io(std::io::Error::other(
                                    "rate limiter poisoned",
                                ))
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
                    if policy.preservation.xattrs && xattrs.is_none() {
                        return Err(RemoteTransferError::MissingFetchPreservation("xattrs"));
                    }
                    if policy.preservation.acls && acl.is_none() {
                        return Err(RemoteTransferError::MissingFetchPreservation("ACLs"));
                    }
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
                    // The caller owns metadata application and destination commit;
                    // it sends the acknowledgement only after both succeed.
                    return Ok(FetchResult {
                        stream_id,
                        summary: TransferSummary {
                            file_size,
                            digest: end.digest(),
                            literal_bytes: file_size,
                            reused_bytes: 0,
                        },
                        preservation: Preservation { xattrs, acl },
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
    .await;
    if result.is_err() {
        if let Err(error) = cancel_fetch(sender, stream_id).await {
            tracing::debug!(%error, stream_id = stream_id.get(), "failed to cancel aborted file fetch");
        }
    }
    result
}

/// Cancel a fetched file whose receiving transaction failed before publication.
pub async fn cancel_fetch(
    sender: &RouterSender,
    stream_id: StreamId,
) -> std::result::Result<(), RemoteTransferError> {
    sender
        .send(Frame::new(
            FrameKind::Cancel,
            FrameFlags::empty(),
            stream_id,
            Bytes::new(),
        )?)
        .await
        .map_err(RemoteTransferError::Router)
}

/// Acknowledge one fetched file only after its staged metadata and destination
/// commit have both succeeded. The stream ID is the transaction identity.
pub async fn acknowledge_fetch(
    sender: &RouterSender,
    stream_id: StreamId,
) -> std::result::Result<(), RemoteTransferError> {
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            stream_id,
            Bytes::new(),
        )?)
        .await
        .map_err(RemoteTransferError::Router)
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
    let preserve_xattrs = request.preserve_xattrs();
    let preserve_acls = request.preserve_acls();
    let relative_for_worker = relative.clone();
    let (mut source_file, xattrs, acl) = tokio::task::spawn_blocking(move || {
        let file = rooted.open_regular_blocking(&relative_for_worker)?;
        crate::remote::transfer::validate_source(&file, expected_identity, expected_size)?;
        let xattrs = if preserve_xattrs {
            Some(rooted.read_open_file_xattrs_blocking(&file, &relative_for_worker)?)
        } else {
            None
        };
        let acl = if preserve_acls {
            Some(
                rooted
                    .read_open_file_acl_blocking(&file, &relative_for_worker)?
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        // Metadata reads share the content handle and must not silently cross
        // an ordinary source-metadata race before bytes start flowing.
        crate::remote::transfer::validate_source(&file, expected_identity, expected_size)?;
        Ok::<_, RemoteTransferError>((file, xattrs, acl))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;

    if let Some(xattrs) = xattrs {
        let entries = xattrs
            .iter()
            .map(|(name, value)| {
                WireXattr::new(
                    crate::remote::xattr::name_bytes(name),
                    Bytes::copy_from_slice(value),
                )
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let payload = WireXattrResult::new(entries)?.encode()?;
        sender
            .send(Frame::new(
                FrameKind::FileXattrs,
                FrameFlags::empty(),
                stream_id,
                payload,
            )?)
            .await
            .map_err(RemoteTransferError::Router)?;
    }
    if let Some(acl) = acl {
        let payload = WireAclResult::new(WireAcl::new(acl)?).encode()?;
        sender
            .send(Frame::new(
                FrameKind::FileAcls,
                FrameFlags::empty(),
                stream_id,
                payload,
            )?)
            .await
            .map_err(RemoteTransferError::Router)?;
    }

    // The producer applies the negotiated per-chunk compression policy; the
    // COMPRESSED flag is set only on frames whose payload is actually zstd.
    let (producer_tx, mut producer_rx) =
        mpsc::channel::<crate::remote::transfer::ProducerItem>(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        let summary = crate::remote::transfer::produce_whole(
            &mut source_file,
            producer_tx,
            compression.as_ref(),
        )?;
        crate::remote::transfer::validate_source(&source_file, expected_identity, expected_size)?;
        if summary.file_size != expected_size {
            return Err(RemoteTransferError::SourceChanged {
                expected_size,
                actual_size: summary.file_size,
            });
        }
        Ok(summary)
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
    receive_fetch_ack(&mut inbox, stream_id).await?;
    Ok(summary)
}

async fn receive_fetch_ack(
    inbox: &mut StreamInbox,
    stream_id: StreamId,
) -> std::result::Result<(), RemoteTransferError> {
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
    if !frame.flags().is_empty() || !frame.payload().is_empty() {
        return Err(RemoteTransferError::FrameFlags {
            kind: frame.kind(),
            flags: frame.flags().bits(),
        });
    }
    match frame.kind() {
        FrameKind::Ack => Ok(()),
        FrameKind::Cancel => Err(RemoteTransferError::FetchCancelled {
            stream_id: stream_id.get(),
        }),
        actual => Err(RemoteTransferError::UnexpectedFrame {
            expected: FrameKind::Ack,
            actual,
        }),
    }
}
