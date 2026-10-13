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
    Frame, FrameFlags, FrameKind, PlatformOs, StreamId, WireAcl, WireAclResult,
    WireFetchCompression, WireFileEnd, WireFileFetchRequest, WireXattr, WireXattrResult,
};
use crate::remote::data::DataChunk;
use crate::remote::path::{decode_relative_path, ensure_compatible_path_encoding};
use crate::remote::router::{IncomingStream, RouterSender, StreamInbox};
use crate::remote::source_file::{SourceFile, SourceFileError, SourceMetadataRequest};
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
    super::preservation::PreservationRequest {
        xattrs: policy.preservation.xattrs,
        acls: policy.preservation.acls,
        ..Default::default()
    }
    .validate(policy.capabilities)?;
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

        receive_payload(
            &mut inbox,
            source.size,
            staged,
            policy.preservation,
            policy.rate_limiter.map(|limiter| limiter.as_ref()),
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
        )
        .await
    }
    .await;
    if result.is_err() {
        if let Err(error) = cancel_fetch(sender, stream_id).await {
            tracing::debug!(%error, stream_id = stream_id.get(), "failed to cancel aborted file fetch");
        }
    }
    result
}

/// Receive the existing bounded whole-file payload while its caller retains the
/// inbox and any source lease. Completion flags belong to the outer lifecycle.
pub(crate) async fn receive_payload(
    inbox: &mut StreamInbox,
    expected_size: u64,
    staged: &mut dyn crate::endpoint::io::StagedWriter,
    preservation: FetchPreservationRequest,
    rate_limiter: Option<&std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>,
    expected_flags: FrameFlags,
) -> std::result::Result<FetchResult, RemoteTransferError> {
    let stream_id = inbox.stream_id();
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
                if !preservation.xattrs || data_started || xattrs.is_some() {
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
                if !preservation.acls || data_started || acl.is_some() {
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
                if file_size > expected_size {
                    return Err(RemoteTransferError::SourceChanged {
                        expected_size,
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
                if preservation.xattrs && xattrs.is_none() {
                    return Err(RemoteTransferError::MissingFetchPreservation("xattrs"));
                }
                if preservation.acls && acl.is_none() {
                    return Err(RemoteTransferError::MissingFetchPreservation("ACLs"));
                }
                if frame.flags() != expected_flags {
                    return Err(RemoteTransferError::FileEndFlags {
                        flags: frame.flags().bits(),
                    });
                }
                let end = WireFileEnd::decode(frame.payload())?;
                if end.file_size() != file_size || file_size != expected_size {
                    return Err(RemoteTransferError::SourceChanged {
                        expected_size,
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
    let decoded = (|| {
        let frame = first.frame();
        if frame.stream_id() != stream_id {
            return Err(RemoteTransferError::StreamMismatch {
                expected: stream_id.get(),
                actual: frame.stream_id().get(),
            });
        }
        if !frame.flags().is_empty() {
            return Err(RemoteTransferError::FrameFlags {
                kind: frame.kind(),
                flags: frame.flags().bits(),
            });
        }
        if frame.kind() != FrameKind::FileFetchRequest {
            return Err(RemoteTransferError::UnexpectedFrame {
                expected: FrameKind::FileFetchRequest,
                actual: frame.kind(),
            });
        }
        let request = WireFileFetchRequest::decode(frame.payload())?;
        let relative = decode_relative_path(request.path.clone(), peer)?;
        Ok((request, relative))
    })();
    drop(first);

    // Invalid requests also release their held root on the joined worker.
    let (source, preservation, request) = tokio::task::spawn_blocking(move || {
        let (request, relative) = decoded?;
        let source = SourceFile::capture_blocking(
            rooted,
            relative,
            crate::engine::domain::EntryIdentity::from_bytes(request.identity()),
            request.file_size(),
        )?;
        let (preservation, _) = source.read_metadata_blocking(SourceMetadataRequest {
            xattrs: request.preserve_xattrs(),
            acls: request.preserve_acls(),
            bsd_flags: false, // Ordinary fetch has no inline flags payload.
        })?;
        Ok::<_, RemoteTransferError>((source, preservation, request))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;
    let expected_size = request.file_size();
    let expected_identity = crate::engine::domain::EntryIdentity::from_bytes(request.identity());
    // The client selects compression; ZSTD was negotiated at handshake.
    let compression = match request.compression() {
        WireFetchCompression::None => None,
        WireFetchCompression::Auto => Some(CompressionPolicy::Auto),
        WireFetchCompression::Always => Some(CompressionPolicy::Always),
    };

    let metadata_sent: std::result::Result<(), RemoteTransferError> = async {
        if let Some(xattrs) = preservation.xattrs {
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
        if let Some(acl) = preservation.acl {
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

        Ok(())
    }
    .await;

    // Even a metadata-send failure releases the original FD/root on a joined
    // blocking worker, not by unwinding a filesystem owner on a Tokio thread.
    let (source, mut source_file) = tokio::task::spawn_blocking(move || {
        metadata_sent?;
        let reader = source.clone_reader_blocking()?;
        Ok::<_, RemoteTransferError>((source, reader))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;

    // The producer applies the negotiated per-chunk compression policy; the
    // COMPRESSED flag is set only on frames whose payload is actually zstd.
    let (producer_tx, producer_rx) =
        mpsc::channel::<crate::remote::transfer::ProducerItem>(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        crate::remote::transfer::validate_source(&source_file, expected_identity, expected_size)?;
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

    let produced =
        crate::remote::transfer::send_produced_file(sender, stream_id, producer_rx, producer).await;
    let (source, summary) = tokio::task::spawn_blocking(move || {
        let summary = produced?;
        source.validate_blocking()?;
        Ok::<_, RemoteTransferError>((source, summary))
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;

    let acknowledged: std::result::Result<(), RemoteTransferError> = async {
        sender
            .send(Frame::new(
                FrameKind::FileEnd,
                FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                stream_id,
                WireFileEnd::new(summary.file_size, summary.digest).encode(),
            )?)
            .await
            .map_err(RemoteTransferError::Router)?;
        receive_fetch_ack(&mut inbox, stream_id).await
    }
    .await;
    // The client ACK is a publication receipt. A later observation failure is
    // committed/partial completion, never permission to claim a rollback.
    tokio::task::spawn_blocking(move || {
        acknowledged?;
        source.validate_blocking().map_err(|source| {
            RemoteTransferError::Io(std::io::Error::other(FetchCompletionError { source }))
        })?;
        Ok(summary)
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))?
}

#[derive(Debug, thiserror::Error)]
#[error("destination publication acknowledged, but source completion validation failed; destination is committed and source completion is uncertain: {source}")]
struct FetchCompletionError {
    #[source]
    source: SourceFileError,
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
            .into_iter()
            .find(|entry| entry.is_file())
            .unwrap()
    }

    #[tokio::test]
    async fn valid_short_footer_cannot_replace_the_scanned_source_size() {
        let source_root = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        std::fs::write(source_root.path().join("file"), b"ten bytes!").unwrap();
        std::fs::write(destination_root.path().join("file"), b"old").unwrap();
        let source = scanned_file(source_root.path()).await;
        let original = scanned_file(destination_root.path()).await;
        let endpoint = LocalEndpoint::new(destination_root.path().to_path_buf());
        let mut staged = endpoint
            .begin_write(
                Path::new("file"),
                ExpectedDestination::Unchanged(original.identity.unwrap()),
            )
            .await
            .unwrap();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (mut sr, mut sw) = tokio::io::split(server_io);
        let client =
            FrameRouter::start(cr, cw, RouterRole::Client, RouterConfig::default()).unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let request = read_frame(&mut sr).await.unwrap();
            assert_eq!(request.kind(), FrameKind::FileFetchRequest);
            let data = b"short";
            for frame in [
                Frame::new(
                    FrameKind::Data,
                    FrameFlags::empty(),
                    request.stream_id(),
                    crate::protocol::WireData::new(Bytes::from_static(data))
                        .unwrap()
                        .into_bytes(),
                )
                .unwrap(),
                Frame::new(
                    FrameKind::FileEnd,
                    FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                    request.stream_id(),
                    WireFileEnd::new(data.len() as u64, *blake3::hash(data).as_bytes()).encode(),
                )
                .unwrap(),
            ] {
                write_frame(&mut sw, &frame).await.unwrap();
            }
            released.await.unwrap();
        });
        let result = fetch_file(
            &client.sender(),
            &source,
            Platform::current().os,
            staged.as_mut(),
            FetchPolicy {
                compression: None,
                rate_limiter: None,
                preservation: FetchPreservationRequest::default(),
                capabilities: endpoint.capabilities(),
            },
        )
        .await;
        staged.abort().await.unwrap();
        release.send(()).unwrap();
        server.await.unwrap();
        assert!(matches!(
            result,
            Err(RemoteTransferError::SourceChanged {
                expected_size: 10,
                actual_size: 5,
            })
        ));
        assert_eq!(
            std::fs::read(destination_root.path().join("file")).unwrap(),
            b"old"
        );
        assert_eq!(
            scanned_file(destination_root.path()).await.identity,
            original.identity
        );
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
            crate::rooted_fs::test_source::scan(&rooted).await;
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
    async fn source_completion_after_ack_is_committed_not_rolled_back() {
        for replace_ancestor in [false, true] {
            let source_root = tempfile::tempdir().unwrap();
            let destination_root = tempfile::tempdir().unwrap();
            std::fs::create_dir(source_root.path().join("dir")).unwrap();
            let source_path = source_root.path().join("dir/file");
            std::fs::write(&source_path, b"original").unwrap();
            let source = scanned_file(source_root.path()).await;
            let rooted = RootedFs::open(source_root.path().to_path_buf())
                .await
                .unwrap();
            crate::rooted_fs::test_source::scan(&rooted).await;
            let peer = Platform::current().os;
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
                serve_incoming_file_fetch(rooted, incoming, &router.sender(), peer).await
            });
            let endpoint = LocalEndpoint::new(destination_root.path().to_path_buf());
            let mut staged = endpoint
                .begin_write(Path::new("file"), ExpectedDestination::Absent)
                .await
                .unwrap();
            let sender = client.sender();
            let result = fetch_file(
                &sender,
                &source,
                peer,
                staged.as_mut(),
                FetchPolicy {
                    compression: None,
                    rate_limiter: None,
                    preservation: FetchPreservationRequest::default(),
                    capabilities: endpoint.capabilities(),
                },
            )
            .await
            .unwrap();
            staged.prepare_publication().await.unwrap();
            staged.commit().await.unwrap().finalize(None).await.unwrap();
            assert!(!server.is_finished()); // FileEnd is NOT completion.
            if replace_ancestor {
                std::fs::rename(
                    source_root.path().join("dir"),
                    source_root.path().join("old-dir"),
                )
                .unwrap();
                std::fs::create_dir(source_root.path().join("dir")).unwrap();
                std::fs::write(&source_path, b"foreign!").unwrap();
                let original = std::fs::metadata(source_root.path().join("old-dir/file")).unwrap();
                assert_eq!(
                    crate::endpoint::local_identity::metadata_identity(
                        &original,
                        crate::engine::domain::EntryKind::File
                    ),
                    source.identity
                );
            } else {
                std::fs::write(&source_path, b"changed!").unwrap();
            }
            acknowledge_fetch(&sender, result.stream_id).await.unwrap();
            let error = tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            let RemoteTransferError::Io(error) = error else {
                panic!("unexpected error: {error}");
            };
            let completion = error
                .get_ref()
                .unwrap()
                .downcast_ref::<FetchCompletionError>()
                .unwrap();
            assert!(matches!(completion.source, SourceFileError::Changed { .. }));
            assert_eq!(
                std::fs::read(destination_root.path().join("file")).unwrap(),
                b"original"
            );
        }
    }

    #[tokio::test]
    async fn source_name_change_during_production_refuses_file_end() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        // More than the bounded producer queue + one outbound frame: after
        // the first received frame, production cannot already have completed.
        let data = vec![b'x'; (PRODUCER_QUEUE_DEPTH + 4) * MAX_TRANSFER_DATA_SIZE];
        std::fs::write(root.path().join("dir/file"), &data).unwrap();
        let source = scanned_file(root.path()).await;
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        crate::rooted_fs::test_source::scan(&rooted).await;
        let peer = Platform::current().os;
        let (mut client_io, server_io) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(server_io);
        let mut server = tokio::spawn(async move {
            let mut router = FrameRouter::start(
                sr,
                sw,
                RouterRole::Server,
                RouterConfig {
                    max_outbound_frames: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            let incoming = router.incoming().recv().await.unwrap().unwrap();
            serve_incoming_file_fetch(rooted, incoming, &router.sender(), peer).await
        });
        let stream_id = StreamId::new(1);
        let request = WireFileFetchRequest::new(
            crate::remote::path::encode_relative_path(source.path.as_path()).unwrap(),
            source.size,
            *source.identity.unwrap().as_bytes(),
            WireFetchCompression::None,
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
        assert_eq!(
            read_frame(&mut client_io).await.unwrap().kind(),
            FrameKind::Data
        );
        std::fs::rename(root.path().join("dir"), root.path().join("old-dir")).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"foreign!").unwrap();
        let original = std::fs::metadata(root.path().join("old-dir/file")).unwrap();
        assert_eq!(
            crate::endpoint::local_identity::metadata_identity(
                &original,
                crate::engine::domain::EntryKind::File
            ),
            source.identity
        );
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut server => break result.unwrap(),
                    frame = read_frame(&mut client_io) => {
                        match frame {
                            Ok(frame) => assert_eq!(frame.kind(), FrameKind::Data, "changed source must not authorize publication with FileEnd"),
                            Err(_) => break server.await.unwrap(),
                        }
                    }
                }
            }
        }).await.unwrap();
        let RemoteTransferError::Io(error) = outcome.unwrap_err() else {
            panic!("expected source lease failure");
        };
        assert!(matches!(
            error.get_ref().unwrap().downcast_ref::<SourceFileError>(),
            Some(SourceFileError::Changed { .. })
        ));
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
            crate::rooted_fs::test_source::scan(&rooted).await;
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
