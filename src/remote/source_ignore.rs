//! Read-only remote source policy for completed local deletion preflight.

use crate::engine::domain::RelativePath;
use crate::engine::ignore_scope::SourceIgnoreScope;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, StreamId, WireSourceIgnoreRequest,
    WireSourceIgnoreResult,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use crate::rooted_fs::{RootedFs, RootedFsError};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum SourceIgnoreError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Path(#[from] RemotePathError),
    #[error(transparent)]
    Rooted(#[from] RootedFsError),
    #[error("source ignore router failed: {0}")]
    Router(#[from] SharedRouterError),
    #[error("source ignore worker failed: {0}")]
    Worker(String),
    #[error("source ignore stream ended before its {0:?}")]
    MissingFrame(FrameKind),
    #[error("source ignore policy read failed: {0}")]
    Policy(#[from] std::io::Error),
    #[error("source ignore policy lock is poisoned")]
    Poisoned,
}

type Result<T> = std::result::Result<T, SourceIgnoreError>;

#[cfg(all(test, unix))]
#[path = "source_ignore_tests.rs"]
mod tests;

/// Each policy has the same bounded rule cache as a local source walk. No
/// destination path, inode, or per-query result is retained after its reply.
#[derive(Clone)]
pub struct SourceIgnoreHandler {
    rooted: RootedFs,
    sender: RouterSender,
    peer: PlatformOs,
    scopes: Arc<Mutex<[SourceIgnoreScope; 2]>>,
}

impl SourceIgnoreHandler {
    pub(super) fn new(rooted: RootedFs, sender: RouterSender, peer: PlatformOs) -> Self {
        let scopes = [
            SourceIgnoreScope::with_remote_authority(rooted.clone(), false),
            SourceIgnoreScope::with_remote_authority(rooted.clone(), true),
        ];
        Self {
            rooted,
            sender,
            peer,
            scopes: Arc::new(Mutex::new(scopes)),
        }
    }

    pub async fn serve(&self, incoming: IncomingStream) -> Result<()> {
        ensure_compatible_path_encoding(self.peer)?;
        let IncomingStream { first, mut inbox } = incoming;
        let id = inbox.stream_id();
        require_frame(
            first.frame(),
            id,
            FrameKind::SourceIgnoreRequest,
            FrameFlags::FINAL,
        )?;
        let request = WireSourceIgnoreRequest::decode(first.frame().payload())?;
        let path = decode_relative_path(request.path, self.peer)?;
        drop(first);
        let rooted = self.rooted.clone();
        let scopes = self.scopes.clone();
        // Server supervision retains this future through the blocking read;
        // disconnect does not silently detach admitted native work.
        let protected = tokio::task::spawn_blocking(move || -> Result<bool> {
            rooted.verify_source_root_path_blocking()?;
            let mut scopes = scopes.lock().map_err(|_| SourceIgnoreError::Poisoned)?;
            let protected = scopes[usize::from(request.respect_gitignore)]
                .deletion_ignored(path.as_path(), request.is_directory)?;
            rooted.verify_source_root_path_blocking()?;
            Ok(protected)
        })
        .await
        .map_err(|error| SourceIgnoreError::Worker(error.to_string()))??;
        self.sender
            .send(Frame::new(
                FrameKind::SourceIgnoreResult,
                FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                id,
                WireSourceIgnoreResult { protected }.encode()?,
            )?)
            .await?;
        let ack = receive(&mut inbox, FrameKind::Ack).await?;
        require_frame(ack.frame(), id, FrameKind::Ack, FrameFlags::empty())?;
        if !ack.frame().payload().is_empty() {
            return Err(ProtocolError::InvalidMessage(
                "source ignore acknowledgement has a payload",
            )
            .into());
        }
        Ok(())
    }
}

pub(super) async fn request_source_ignore(
    sender: &RouterSender,
    path: &RelativePath,
    is_directory: bool,
    respect_gitignore: bool,
    peer: PlatformOs,
) -> Result<bool> {
    ensure_compatible_path_encoding(peer)?;
    let mut inbox = sender.open_stream()?;
    let id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::SourceIgnoreRequest,
            FrameFlags::FINAL,
            id,
            WireSourceIgnoreRequest {
                path: encode_relative_path(path.as_path())?,
                is_directory,
                respect_gitignore,
            }
            .encode()?,
        )?)
        .await?;
    let result = receive(&mut inbox, FrameKind::SourceIgnoreResult).await?;
    require_frame(
        result.frame(),
        id,
        FrameKind::SourceIgnoreResult,
        FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
    )?;
    let protected = WireSourceIgnoreResult::decode(result.frame().payload())?.protected;
    drop(result);
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            id,
            bytes::Bytes::new(),
        )?)
        .await?;
    Ok(protected)
}

async fn receive(
    inbox: &mut StreamInbox,
    expected: FrameKind,
) -> Result<crate::remote::router::RoutedFrame> {
    inbox
        .recv()
        .await?
        .ok_or(SourceIgnoreError::MissingFrame(expected))
}

fn require_frame(frame: &Frame, id: StreamId, kind: FrameKind, flags: FrameFlags) -> Result<()> {
    if frame.stream_id() != id || frame.kind() != kind || frame.flags() != flags {
        return Err(ProtocolError::InvalidMessage("invalid source ignore frame or flags").into());
    }
    Ok(())
}
