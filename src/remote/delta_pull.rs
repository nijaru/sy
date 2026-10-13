//! Read-only delta PULL: client-owned OLD signatures, server-owned SOURCE bytes.
#[cfg(all(test, unix))]
mod regression_tests;
#[cfg(all(test, unix))]
mod server_tests;
#[cfg(all(test, unix))]
mod tests;
use crate::endpoint::local::LocalEndpoint;
use crate::engine::domain::{Entry, RelativePath};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, SignatureBlockSize, WireDeltaFetchRequest,
    WireFetchCompression, WireFileBasis, WireFileBegin, WireFileFetchRequest, WireSignature,
    WireSignatureEnd,
};
use crate::remote::fetch::FetchPolicy;
use crate::remote::router::{RouterSender, StreamInbox};
use crate::remote::transfer::population::{
    prepare_pull_reconstruction_blocking, ReconstructionReceiver,
};
use crate::remote::transfer::{RemoteTransferError, Result, PRODUCER_QUEUE_DEPTH};
use crate::rooted_fs::PublishedFileProof;
use crate::transfer::delta::{
    BasisBlock, BasisIndex, BasisIndexBuilder, BasisIndexLimits, DEFAULT_MAX_BASIS_BLOCKS,
};
use tokio::sync::mpsc;

/// Conservative maximum resident index (65,536 blocks including HashMap/Vec
/// capacity), matcher window, literal queues and compression scratch. Router
/// frame/byte permits are independent and shared session-wide.
pub(crate) const SERVER_WORKING_SET: u64 = 24 * 1024 * 1024;

/// Select fallback before opening/signing OLD; transfer errors are terminal.
pub(crate) fn candidate(
    old: &Entry,
    minimum: u64,
    capabilities: &crate::endpoint::Capabilities,
) -> bool {
    old.is_file()
        && old.identity.is_some()
        && old.size >= minimum
        && capabilities.delta_pull
        && capabilities.blake3
        && old.size.div_ceil(u64::from(
            crate::remote::signature::choose_signature_block_size(old.size),
        )) <= DEFAULT_MAX_BASIS_BLOCKS as u64
}

pub(crate) struct PullTarget {
    pub endpoint: LocalEndpoint,
    pub path: RelativePath,
    pub old: Entry,
    pub unix_mode: u32,
    pub modified: Option<crate::engine::domain::Timestamp>,
    pub final_flags: Option<u32>,
}

/// The native reconstruction owner receives the genuine OLD description used
/// for signatures. It validates OLD/binding, staged digest and metadata before
/// its existing publication admission/commit, then acknowledges SOURCE.
pub(crate) async fn fetch_delta(
    sender: &RouterSender,
    source: &Entry,
    peer: PlatformOs,
    target: PullTarget,
    policy: FetchPolicy<'_>,
) -> Result<(crate::engine::work::TransferSummary, PublishedFileProof)> {
    if !policy.capabilities.delta_pull || !policy.capabilities.blake3 {
        return Err(RemoteTransferError::PreservationUnavailable {
            feature: "delta PULL",
        });
    }
    if policy.compression.is_some() && !policy.capabilities.zstd {
        return Err(RemoteTransferError::PreservationUnavailable { feature: "zstd" });
    }
    crate::remote::preservation::PreservationRequest {
        xattrs: policy.preservation.xattrs,
        acls: policy.preservation.acls,
        flags: target.final_flags.is_some(),
        ..Default::default()
    }
    .validate(policy.capabilities)?;
    crate::remote::path::ensure_compatible_path_encoding(peer)?;
    if !source.is_file() || !target.old.is_file() {
        return Err(RemoteTransferError::InvalidSource);
    }
    let identity = source
        .identity
        .ok_or(RemoteTransferError::MissingSourceIdentity)?;
    let old_identity = target
        .old
        .identity
        .ok_or(RemoteTransferError::CopyWithoutBasis)?;
    let block_size = SignatureBlockSize::new(
        crate::remote::signature::choose_signature_block_size(target.old.size),
    )?;
    let request = WireDeltaFetchRequest {
        source: WireFileFetchRequest::new(
            crate::remote::path::encode_relative_path(source.path.as_path())?,
            source.size,
            *identity.as_bytes(),
            match policy.compression {
                None => WireFetchCompression::None,
                Some(crate::engine::compression::CompressionPolicy::Auto) => {
                    WireFetchCompression::Auto
                }
                Some(crate::engine::compression::CompressionPolicy::Always) => {
                    WireFetchCompression::Always
                }
            },
        )
        .with_preservation(policy.preservation.xattrs, policy.preservation.acls),
        block_size,
        basis_size: target.old.size,
        basis_identity: *old_identity.as_bytes(),
    };
    // Apply the same bounded announcement validation to local policy mistakes.
    WireDeltaFetchRequest::decode(&request.encode())?;
    let basis = WireFileBasis::new(target.old.size, *old_identity.as_bytes());
    let begin = WireFileBegin::delta(
        crate::remote::path::encode_relative_path(target.path.as_path())?,
        source.size,
        basis,
    )
    .with_metadata(
        Some(target.unix_mode),
        target
            .modified
            .map(|time| (time.seconds(), time.nanoseconds())),
    )?
    .with_bsd_flags(target.final_flags);
    let mut inbox = sender.open_stream()?;
    let stream_id = inbox.stream_id();
    let rooted = target.endpoint.rooted_fs(false).await?;
    let endpoint = target.endpoint;
    let destination_path = target.path.clone();
    let overrides = crate::endpoint::io::PreservationRequest {
        xattrs: policy.preservation.xattrs,
        acl: policy.preservation.acls,
    };
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_pull_reconstruction_blocking(
            rooted.as_ref().clone(),
            &target.path,
            basis,
            overrides,
            target.final_flags.is_some(),
        )
    })
    .await
    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))??;
    let admitted: Result<()> = async {
        sender
            .send(Frame::new(
                FrameKind::DeltaFetchRequest,
                FrameFlags::empty(),
                stream_id,
                request.encode(),
            )?)
            .await?;
        let ready = inbox
            .recv()
            .await?
            .ok_or(RemoteTransferError::UnexpectedStreamEnd {
                stream_id: stream_id.get(),
            })?;
        let frame = ready.frame();
        if frame.kind() != FrameKind::DeltaFetchReady {
            return Err(RemoteTransferError::PrematureFetchFrame(frame.kind()));
        }
        if !frame.flags().is_empty() {
            return Err(RemoteTransferError::FrameFlags {
                kind: frame.kind(),
                flags: frame.flags().bits(),
            });
        }
        if !frame.payload().is_empty() {
            return Err(crate::protocol::ProtocolError::InvalidField {
                field: "delta_fetch_ready",
                reason: "payload must be empty",
            }
            .into());
        }
        Ok(())
    }
    .await;
    if let Err(operation) = admitted {
        let aborted = tokio::task::spawn_blocking(move || prepared.abort_blocking())
            .await
            .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))
            .and_then(|result| result);
        let _ = crate::remote::fetch::cancel_fetch(sender, stream_id).await;
        return Err(match aborted {
            Ok(()) => operation,
            Err(abort) => RemoteTransferError::ReconstructionAbortFailed {
                operation: Box::new(operation),
                abort: Box::new(abort),
            },
        });
    }
    // Waiting requests carry no signature frames: otherwise router-global
    // inbound permits can starve the ACKs that release server admission.
    let (tx, mut rx) = mpsc::channel::<WireSignature>(PRODUCER_QUEUE_DEPTH);
    let producer = tokio::task::spawn_blocking(move || {
        prepared.produce_pull_signatures_blocking(block_size, tx)
    });
    let uploading = async {
        while let Some(signature) = rx.recv().await {
            sender
                .send(Frame::new(
                    FrameKind::Signature,
                    FrameFlags::empty(),
                    stream_id,
                    signature.encode(),
                )?)
                .await?;
        }
        Ok(())
    };
    let sent: Result<()> = tokio::select! {
        biased;
        error = sender.closed() => Err(RemoteTransferError::Router(error)),
        early = inbox.recv() => Err(match early {
            Ok(Some(early)) => RemoteTransferError::PrematureFetchFrame(early.frame().kind()),
            Ok(None) => RemoteTransferError::UnexpectedStreamEnd { stream_id: stream_id.get() },
            Err(error) => RemoteTransferError::Router(error),
        }),
        sent = uploading => sent,
    };
    drop(rx); // Unblock a backpressured producer before joining on every error.
    let produced = producer
        .await
        .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()))
        .and_then(|result| result);
    let result = async {
        let (prepared, end) = match produced {
            Ok(produced) => produced,
            Err(producer) => {
                return Err(match (sent, producer) {
                    (
                        Err(operation),
                        RemoteTransferError::ReconstructionAbortFailed { abort, .. },
                    ) => RemoteTransferError::ReconstructionAbortFailed {
                        operation: Box::new(operation),
                        abort,
                    },
                    (Err(operation), _) => operation,
                    (Ok(()), producer) => producer,
                });
            }
        };
        let sent = match sent {
            Ok(()) => {
                async {
                    sender
                        .send(Frame::new(
                            FrameKind::SignatureEnd,
                            FrameFlags::empty(),
                            stream_id,
                            end.encode(),
                        )?)
                        .await?;
                    Ok(())
                }
                .await
            }
            Err(error) => Err(error),
        };
        if let Err(operation) = sent {
            let aborted = tokio::task::spawn_blocking(move || prepared.abort_blocking())
                .await
                .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))
                .and_then(|result| result);
            return Err(match aborted {
                Ok(()) => operation,
                Err(abort) => RemoteTransferError::ReconstructionAbortFailed {
                    operation: Box::new(operation),
                    abort: Box::new(abort),
                },
            });
        }
        let mut receiver = ReconstructionReceiver::start(prepared, begin)
            .with_pull_policy(policy.preservation, policy.rate_limiter.cloned());
        let ready = receiver
            .receive(&mut inbox, |frame| {
                if frame.flags() != (FrameFlags::FINAL | FrameFlags::ACK_REQUIRED) {
                    return Err(RemoteTransferError::FileEndFlags {
                        flags: frame.flags().bits(),
                    });
                }
                Ok(())
            })
            .await?;
        let admission = sender.publication_admission();
        let (summary, proof) = tokio::task::spawn_blocking(move || {
            let (staged, summary, flags) = ready.into_staged_blocking()?;
            let proof = endpoint.commit_reconstruction_blocking(
                &destination_path,
                staged,
                flags,
                &admission,
            )?;
            Ok::<_, RemoteTransferError>((summary, proof))
        })
        .await
        .map_err(|error| RemoteTransferError::ReconstructionJoin(error.to_string()))??;
        crate::remote::fetch::acknowledge_fetch(sender, stream_id)
            .await
            .map_err(|error| {
                RemoteTransferError::Io(std::io::Error::other(format!(
                    "delta PULL destination committed, but ACK failed: {error}"
                )))
            })?;
        Ok((summary, proof))
    }
    .await;
    if result.is_err() {
        if let Err(error) = crate::remote::fetch::cancel_fetch(sender, stream_id).await {
            tracing::debug!(%error, "failed to cancel delta PULL");
        }
    }
    result
}

/// Consume only the declared exact signature phase. No remote file is opened
/// from the opaque OLD identity; the uploaded index is bounded before ingestion.
pub(crate) async fn receive_basis(
    inbox: &mut StreamInbox,
    request: &WireDeltaFetchRequest,
) -> Result<BasisIndex> {
    let mut builder = BasisIndexBuilder::new(request.block_size.get(), BasisIndexLimits::default())
        .map_err(std::io::Error::other)?;
    let expected_count = request
        .basis_size
        .div_ceil(u64::from(request.block_size.get()));
    let mut size = 0_u64;
    loop {
        let routed = inbox
            .recv()
            .await?
            .ok_or(RemoteTransferError::MissingFileEnd)?;
        let frame = routed.frame();
        if !frame.flags().is_empty() {
            return Err(RemoteTransferError::FrameFlags {
                kind: frame.kind(),
                flags: frame.flags().bits(),
            });
        }
        match frame.kind() {
            FrameKind::Signature => {
                let block = WireSignature::decode(frame.payload())?;
                if block.index() >= expected_count {
                    return Err(RemoteTransferError::InvalidDeltaSignatures("extra block"));
                }
                let expected_size = (request.basis_size
                    - block.index() * u64::from(request.block_size.get()))
                .min(u64::from(request.block_size.get()));
                if u64::from(block.size()) != expected_size {
                    return Err(RemoteTransferError::InvalidDeltaSignatures(
                        "wrong block length",
                    ));
                }
                size = size
                    .checked_add(u64::from(block.size()))
                    .ok_or(RemoteTransferError::ByteCountOverflow)?;
                builder
                    .push(BasisBlock {
                        index: block.index(),
                        size: block.size(),
                        weak: block.weak(),
                        strong: block.strong(),
                    })
                    .map_err(std::io::Error::other)?;
            }
            FrameKind::SignatureEnd => {
                let end = WireSignatureEnd::decode(frame.payload())?;
                if end.file_size() != request.basis_size
                    || size != request.basis_size
                    || end.block_count() != expected_count
                    || builder.block_count() as u64 != expected_count
                    || end.basis_identity() != request.basis_identity
                {
                    return Err(RemoteTransferError::InvalidDeltaSignatures(
                        "incomplete or mismatched footer",
                    ));
                }
                return tokio::task::spawn_blocking(move || builder.finish())
                    .await
                    .map_err(|error| RemoteTransferError::ProducerJoin(error.to_string()));
            }
            actual => {
                return Err(RemoteTransferError::UnexpectedFrame {
                    expected: FrameKind::SignatureEnd,
                    actual,
                })
            }
        }
    }
}
