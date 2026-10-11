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
use crate::engine::work::TransferSummary;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolVersion, StreamId, WireAcl, WireAclResult,
    WireFetchCompression, WireFileEnd, WireFileFetchRequest, WireXattr, WireXattrResult,
    PROTOCOL_V3_2,
};
use crate::remote::data::DataChunk;
use crate::remote::path::{decode_relative_path, ensure_compatible_path_encoding};
use crate::remote::router::{IncomingStream, RouterSender, StreamInbox};
use crate::rooted_fs::RootedFs;

use crate::remote::transfer::{RemoteTransferError, PRODUCER_QUEUE_DEPTH};
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
        match policy.compression {
            None => WireFetchCompression::None,
            Some(CompressionPolicy::Auto) => WireFetchCompression::Auto,
            Some(CompressionPolicy::Always) => WireFetchCompression::Always,
        },
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
                    let bytes = DataChunk::from_frame(frame)?.decode().await?;
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
    let compression = match request.compression() {
        WireFetchCompression::None => None,
        WireFetchCompression::Auto => Some(CompressionPolicy::Auto),
        WireFetchCompression::Always => Some(CompressionPolicy::Always),
    };
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
    let (producer_tx, producer_rx) =
        mpsc::channel::<crate::remote::transfer::ProducerItem>(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        let summary = crate::remote::transfer::produce_whole(
            &mut source_file,
            expected_size,
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

    let summary =
        crate::remote::transfer::send_produced_file(sender, stream_id, producer_rx, producer)
            .await?;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::endpoint::{local::LocalEndpoint, Endpoint, ExpectedDestination};
    use crate::engine::domain::Entry;
    use crate::protocol::{
        read_frame, write_frame, Platform, MAX_FRAME_PAYLOAD, MAX_TRANSFER_DATA_SIZE,
        SUPPORTED_VERSIONS,
    };
    use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
    use futures::TryStreamExt;
    use std::path::Path;

    async fn scanned_file(root: &Path) -> Entry {
        crate::endpoint::source_root::SourceRoot::open(root.to_path_buf())
            .await
            .unwrap()
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0)
    }

    #[tokio::test]
    async fn fetch_client_preserves_policy_through_verified_staging() {
        let source_root = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        let data = vec![b'x'; 2048];
        std::fs::write(source_root.path().join("file"), &data).unwrap();
        let source = scanned_file(source_root.path()).await;
        let peer = Platform::current().os;
        let endpoint = LocalEndpoint::new(destination_root.path().to_path_buf());

        for (compression, wire) in [
            (None, WireFetchCompression::None),
            (Some(CompressionPolicy::Auto), WireFetchCompression::Auto),
            (
                Some(CompressionPolicy::Always),
                WireFetchCompression::Always,
            ),
        ] {
            let rooted = RootedFs::open(source_root.path().to_path_buf())
                .await
                .unwrap();
            let (client_io, server_io) = tokio::io::duplex(4096);
            let (cr, cw) = tokio::io::split(client_io);
            let (sr, sw) = tokio::io::split(server_io);
            let client =
                FrameRouter::start(cr, cw, RouterRole::Client, RouterConfig::default()).unwrap();
            let server = tokio::spawn(async move {
                let mut router =
                    FrameRouter::start(sr, sw, RouterRole::Server, RouterConfig::default())
                        .unwrap();
                let incoming = router.incoming().recv().await.unwrap().unwrap();
                let request =
                    WireFileFetchRequest::decode(incoming.first.frame().payload()).unwrap();
                assert_eq!(request.compression(), wire);
                serve_incoming_file_fetch(rooted, incoming, &router.sender(), peer)
                    .await
                    .unwrap()
            });
            let destination = Path::new(match wire {
                WireFetchCompression::None => "none",
                WireFetchCompression::Auto => "auto",
                WireFetchCompression::Always => "always",
            });
            let mut staged = endpoint
                .begin_write(destination, ExpectedDestination::Absent)
                .await
                .unwrap();
            let sender = client.sender();
            let result = fetch_file(
                &sender,
                &source,
                peer,
                staged.as_mut(),
                FetchPolicy {
                    compression,
                    rate_limiter: None,
                    preservation: FetchPreservationRequest::default(),
                    protocol_version: SUPPORTED_VERSIONS.max,
                    capabilities: endpoint.capabilities(),
                },
            )
            .await
            .unwrap();
            let digest = blake3::hash(&data);
            assert_eq!(result.summary.digest, *digest.as_bytes());
            assert_eq!(staged.staged_hash().await.unwrap(), Some(digest));
            staged.prepare_publication().await.unwrap();
            staged.commit().await.unwrap().finalize(None).await.unwrap();
            acknowledge_fetch(&sender, result.stream_id).await.unwrap();
            assert_eq!(server.await.unwrap(), result.summary);
            assert_eq!(
                std::fs::read(destination_root.path().join(destination)).unwrap(),
                data
            );
        }
        assert_eq!(
            std::fs::read(source_root.path().join("file")).unwrap(),
            data
        );
        assert_eq!(
            scanned_file(source_root.path()).await.identity,
            source.identity
        );
    }

    #[tokio::test]
    async fn fetch_auto_declines_after_sample_but_always_compresses_later_chunk() {
        let root = tempfile::tempdir().unwrap();
        // A non-compressible first chunk makes Auto decline the entire transfer;
        // the second chunk is compressible, so Always has an observable wire win.
        let mut data = Vec::with_capacity(2 * MAX_TRANSFER_DATA_SIZE);
        let mut state = 0x8d12_e3a5_6b7c_90f1_u64;
        for _ in 0..MAX_TRANSFER_DATA_SIZE {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push(state as u8);
        }
        assert!(
            zstd::bulk::compress(&data, crate::engine::compression::ZSTD_FAST_LEVEL)
                .unwrap()
                .len()
                >= data.len()
        );
        data.resize(2 * MAX_TRANSFER_DATA_SIZE, b'x');
        std::fs::write(root.path().join("file"), &data).unwrap();
        let source = scanned_file(root.path()).await;
        let peer = Platform::current().os;
        for compression in [
            WireFetchCompression::None,
            WireFetchCompression::Auto,
            WireFetchCompression::Always,
        ] {
            let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
            let (mut client_io, server_io) = tokio::io::duplex(4096);
            let (sr, sw) = tokio::io::split(server_io);
            let server = tokio::spawn(async move {
                let mut router =
                    FrameRouter::start(sr, sw, RouterRole::Server, RouterConfig::default())
                        .unwrap();
                let incoming = router.incoming().recv().await.unwrap().unwrap();
                serve_incoming_file_fetch(rooted, incoming, &router.sender(), peer)
                    .await
                    .unwrap()
            });
            let stream_id = StreamId::new(1);
            let request = WireFileFetchRequest::new(
                crate::remote::path::encode_relative_path(source.path.as_path()).unwrap(),
                source.size,
                *source.identity.unwrap().as_bytes(),
                compression,
            );
            write_frame(
                &mut client_io,
                &Frame::new(
                    FrameKind::FileFetchRequest,
                    FrameFlags::empty(),
                    stream_id,
                    request.encode(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let mut received = Vec::new();
            let mut compressed_chunks = Vec::new();
            let end = loop {
                let frame = read_frame(&mut client_io).await.unwrap();
                assert_eq!(frame.stream_id(), stream_id);
                assert!(frame.payload().len() <= MAX_FRAME_PAYLOAD);
                if frame.kind() == FrameKind::FileEnd {
                    assert_eq!(frame.flags(), FrameFlags::FINAL | FrameFlags::ACK_REQUIRED);
                    break WireFileEnd::decode(frame.payload()).unwrap();
                }
                assert_eq!(frame.kind(), FrameKind::Data);
                let decoded = DataChunk::from_frame(&frame)
                    .unwrap()
                    .decode()
                    .await
                    .unwrap();
                assert!(decoded.len() <= MAX_TRANSFER_DATA_SIZE);
                let compressed = frame.flags().contains(FrameFlags::COMPRESSED);
                if compressed {
                    assert!(frame.payload().len() < decoded.len());
                } else {
                    assert_eq!(frame.payload().as_ref(), decoded.as_ref());
                }
                compressed_chunks.push(compressed);
                received.extend_from_slice(&decoded);
            };
            assert_eq!(
                compressed_chunks,
                [false, compression == WireFetchCompression::Always]
            );
            assert_eq!(received, data);
            assert_eq!(end.file_size(), source.size);
            assert_eq!(end.digest(), *blake3::hash(&data).as_bytes());
            write_frame(
                &mut client_io,
                &Frame::new(FrameKind::Ack, FrameFlags::empty(), stream_id, Bytes::new()).unwrap(),
            )
            .await
            .unwrap();
            let summary = server.await.unwrap();
            assert_eq!(summary.digest, end.digest());
            assert_eq!(summary.file_size, source.size);
        }
        assert_eq!(scanned_file(root.path()).await.identity, source.identity);
    }
}
