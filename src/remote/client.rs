use super::metadata::request_metadata;
use super::mutation::{request_create_directory, request_remove, request_replace_symlink};
use super::{ClientRemoteSession, RemoteSessionError, Result};
use crate::engine::domain::{Entry, EntryKind, RelativePath, Timestamp};
use crate::engine::reconcile::EntryStream;
use crate::engine::scan::ScanRequest;
use crate::protocol::{CapabilitySet, FrameKind, Operation, PlatformOs, SessionReady};
use crate::remote::router::RouterSender;
use crate::remote::scan::request_scan;
use crate::remote::signature::{
    choose_signature_block_size, request_signatures, RemoteSignatureError, SignatureEvent,
    SignatureStream,
};
use crate::remote::transfer::{request_file_transfer, RemoteDeltaBasis, TransferSummary};
use crate::transfer::delta::{
    BasisBlock, BasisIndex, BasisIndexBuilder, BasisIndexError, BasisIndexLimits,
};
use futures::StreamExt;
use std::path::{Path, PathBuf};

/// Cloneable v3 request authority for one already-negotiated remote session.
///
/// This deliberately does not own the frame router, incoming-stream receiver,
/// transport, or SSH child. `ClientRemoteSession` remains the sole lifecycle
/// owner while scheduler tasks clone this lightweight handle to open independent
/// multiplexed request streams through the shared bounded `RouterSender`.
#[derive(Clone)]
pub struct ClientRemoteHandle {
    operation: Operation,
    peer: PlatformOs,
    ready: SessionReady,
    sender: RouterSender,
}

impl ClientRemoteSession {
    pub fn request_handle(&self) -> ClientRemoteHandle {
        ClientRemoteHandle {
            operation: self.operation,
            peer: self.server.platform.os,
            ready: self.ready,
            sender: self.router.sender(),
        }
    }
}

impl ClientRemoteHandle {
    pub const fn operation(&self) -> Operation {
        self.operation
    }

    pub const fn peer_platform(&self) -> PlatformOs {
        self.peer
    }

    pub const fn ready(&self) -> SessionReady {
        self.ready
    }

    pub async fn scan(&self, request: ScanRequest) -> crate::remote::scan::Result<EntryStream> {
        request_scan(&self.sender, request, self.peer).await
    }

    pub async fn signatures(
        &self,
        basis: &Entry,
    ) -> crate::remote::signature::Result<(u32, SignatureStream)> {
        if !self
            .ready
            .capabilities
            .contains(CapabilitySet::ROLLING_SIGNATURES)
        {
            return Err(RemoteSignatureError::UnsupportedByPeer);
        }
        if !basis.is_file() {
            return Err(RemoteSignatureError::InvalidBasis);
        }
        let identity = basis
            .identity
            .ok_or(RemoteSignatureError::MissingBasisIdentity)?;
        request_signatures(
            &self.sender,
            &basis.path,
            basis.size,
            identity,
            self.peer,
        )
        .await
    }

    pub async fn delta_basis(
        &self,
        basis: &Entry,
        limits: BasisIndexLimits,
    ) -> Result<Option<BasisIndex>> {
        if !self
            .ready
            .capabilities
            .contains(CapabilitySet::ROLLING_SIGNATURES)
        {
            return Err(RemoteSignatureError::UnsupportedByPeer.into());
        }
        if !basis.is_file() {
            return Err(RemoteSignatureError::InvalidBasis.into());
        }
        if basis.identity.is_none() {
            return Err(RemoteSignatureError::MissingBasisIdentity.into());
        }

        let block_size = choose_signature_block_size(basis.size);
        let mut builder = BasisIndexBuilder::new(block_size, limits)?;
        let max_blocks = u64::try_from(limits.max_blocks).unwrap_or(u64::MAX);
        let expected_blocks = basis.size.div_ceil(u64::from(block_size));
        if expected_blocks > max_blocks {
            return Ok(None);
        }

        let (actual_block_size, mut signatures) = self.signatures(basis).await?;
        if actual_block_size != block_size {
            return Err(RemoteSessionError::SignatureBlockSizeMismatch {
                expected: block_size,
                actual: actual_block_size,
            });
        }

        let mut over_limit = false;
        loop {
            let Some(event) = signatures.next().await else {
                return Err(RemoteSessionError::MissingSignatureEnd);
            };
            let event =
                event.map_err(|error| RemoteSessionError::SignatureStream(error.to_string()))?;
            match event {
                SignatureEvent::Block(block) if !over_limit => {
                    let block = BasisBlock {
                        index: block.index,
                        size: block.size,
                        weak: block.weak,
                        strong: block.strong,
                    };
                    match builder.push(block) {
                        Ok(()) => {}
                        Err(BasisIndexError::TooManyBlocks { .. }) => over_limit = true,
                        Err(error) => return Err(error.into()),
                    }
                }
                SignatureEvent::Block(_) => {}
                SignatureEvent::End(_) => break,
            }
        }

        if over_limit {
            Ok(None)
        } else {
            Ok(Some(builder.finish()))
        }
    }

    pub async fn transfer_file(
        &self,
        source_root: PathBuf,
        source: Entry,
        delta_basis: Option<RemoteDeltaBasis>,
    ) -> Result<TransferSummary> {
        self.require_push(FrameKind::FileBegin)?;
        request_file_transfer(
            &self.sender,
            source_root,
            source,
            delta_basis,
            self.peer,
        )
        .await
        .map_err(Into::into)
    }

    pub async fn apply_metadata(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    ) -> Result<()> {
        self.require_push(FrameKind::Metadata)?;
        request_metadata(&self.sender, path, kind, unix_mode, modified, self.peer)
            .await
            .map_err(Into::into)
    }

    pub async fn create_directory(&self, path: &RelativePath) -> Result<()> {
        self.require_push(FrameKind::Mutation)?;
        request_create_directory(&self.sender, path, self.peer)
            .await
            .map_err(Into::into)
    }

    pub async fn replace_symlink(&self, path: &RelativePath, target: &Path) -> Result<()> {
        self.require_push(FrameKind::Mutation)?;
        request_replace_symlink(&self.sender, path, target, self.peer)
            .await
            .map_err(Into::into)
    }

    pub async fn remove(&self, path: &RelativePath, is_directory: bool) -> Result<()> {
        self.require_push(FrameKind::Mutation)?;
        request_remove(&self.sender, path, is_directory, self.peer)
            .await
            .map_err(Into::into)
    }

    fn require_push(&self, kind: FrameKind) -> Result<()> {
        if self.operation == Operation::Push {
            Ok(())
        } else {
            Err(RemoteSessionError::OperationMismatch {
                operation: self.operation,
                kind,
            })
        }
    }
}
