//! Read-only destination qualification. Native effects retain the server's
//! original catalogue; a returned profile is only collision-preflight input.
use crate::engine::namespace::{NamespaceParent, NamespaceQualification, NamespaceSemantics};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, ResolvedOperand, StreamId,
    WireNamespaceParent, WireNamespaceRequest, WireNamespaceResult,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding, RemotePathError,
};
use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum NamespaceRpcError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Path(#[from] RemotePathError),
    #[error(transparent)]
    Native(#[from] crate::rooted_fs::RootedFsError),
    #[error(transparent)]
    Qualification(#[from] crate::engine::namespace::NamespacePreflightError),
    #[error("namespace router failed: {0}")]
    Router(#[from] SharedRouterError),
    #[error("namespace stream ended before {0:?}")]
    MissingFrame(FrameKind),
}

type Result<T> = std::result::Result<T, NamespaceRpcError>;

#[derive(Clone)]
pub(super) struct NamespaceHandler {
    pub(super) authority: Arc<dyn NamespaceQualification>,
    pub(super) sender: RouterSender,
    pub(super) peer: PlatformOs,
    pub(super) resolved: ResolvedOperand,
}

impl NamespaceHandler {
    pub(super) async fn serve(&self, incoming: IncomingStream) -> Result<()> {
        let (request, inbox) = decode_request(incoming)?;
        let backup = matches!(&request, WireNamespaceRequest::QualifyBackup(_));
        let reply = match request {
            WireNamespaceRequest::Begin => {
                return Err(ProtocolError::InvalidMessage(
                    "namespace begin bypassed drain barrier",
                )
                .into());
            }
            WireNamespaceRequest::Qualify(parent) | WireNamespaceRequest::QualifyBackup(parent) => {
                let parent = match parent {
                    WireNamespaceParent::Root => NamespaceParent::Root,
                    WireNamespaceParent::Directory(path) => {
                        if !backup && matches!(self.resolved, ResolvedOperand::Entry { .. }) {
                            return Err(ProtocolError::InvalidMessage(
                                "selected leaf namespace query escaped its containing root",
                            )
                            .into());
                        }
                        NamespaceParent::Directory(decode_relative_path(path, self.peer)?)
                    }
                };
                WireNamespaceResult::Qualified(self.authority.qualify(parent).await?.into())
            }
            WireNamespaceRequest::ValidateBackupAddress(path) => {
                self.authority
                    .validate_backup_address(decode_relative_path(path, self.peer)?)
                    .await?;
                WireNamespaceResult::BackupAddressValidated
            }
            WireNamespaceRequest::Seal => {
                self.authority.seal().await?;
                WireNamespaceResult::Sealed
            }
        };
        reply_and_ack(&self.sender, inbox, reply).await
    }
}

/// Decode and release the opener's shared frame/byte permits before waiting
/// for filesystem work or for other handlers at a run boundary.
pub(super) fn decode_request(
    incoming: IncomingStream,
) -> Result<(WireNamespaceRequest, StreamInbox)> {
    let IncomingStream { first, inbox } = incoming;
    require_frame(
        first.frame(),
        inbox.stream_id(),
        FrameKind::NamespaceRequest,
        FrameFlags::FINAL,
    )?;
    let request = WireNamespaceRequest::decode(first.frame().payload())?;
    drop(first);
    Ok((request, inbox))
}

pub(super) async fn reply_and_ack(
    sender: &RouterSender,
    mut inbox: StreamInbox,
    reply: WireNamespaceResult,
) -> Result<()> {
    let id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::NamespaceResult,
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            id,
            reply.encode(),
        )?)
        .await?;
    let ack = receive(&mut inbox, FrameKind::Ack).await?;
    require_frame(ack.frame(), id, FrameKind::Ack, FrameFlags::empty())?;
    if !ack.frame().payload().is_empty() {
        return Err(
            ProtocolError::InvalidMessage("namespace acknowledgement has a payload").into(),
        );
    }
    Ok(())
}

pub(super) async fn begin(
    sender: RouterSender,
    peer: PlatformOs,
) -> Result<RemoteNamespaceQualification> {
    if request(&sender, WireNamespaceRequest::Begin).await? != WireNamespaceResult::Begun {
        return Err(ProtocolError::InvalidMessage("namespace begin got wrong-phase reply").into());
    }
    Ok(RemoteNamespaceQualification {
        sender,
        peer,
        backup: false,
    })
}

/// The owning ClientRemoteSession must outlive preflight and execution. This
/// handle cannot establish or refresh native destination observations itself.
pub(super) struct RemoteNamespaceQualification {
    sender: RouterSender,
    peer: PlatformOs,
    backup: bool,
}

pub(super) fn backup(sender: RouterSender, peer: PlatformOs) -> RemoteNamespaceQualification {
    RemoteNamespaceQualification {
        sender,
        peer,
        backup: true,
    }
}

impl NamespaceQualification for RemoteNamespaceQualification {
    fn qualify<'a>(
        &'a self,
        parent: NamespaceParent,
    ) -> futures::future::BoxFuture<'a, crate::engine::namespace::Result<NamespaceSemantics>> {
        Box::pin(async move {
            let result = async {
                ensure_compatible_path_encoding(self.peer)?;
                let parent = match parent {
                    NamespaceParent::Root => WireNamespaceParent::Root,
                    NamespaceParent::Directory(path) => {
                        WireNamespaceParent::Directory(encode_relative_path(path.as_path())?)
                    }
                };
                let message = if self.backup {
                    WireNamespaceRequest::QualifyBackup(parent)
                } else {
                    WireNamespaceRequest::Qualify(parent)
                };
                match request(&self.sender, message).await? {
                    WireNamespaceResult::Qualified(profile) => Ok(profile.into()),
                    _ => Err(ProtocolError::InvalidMessage(
                        "namespace qualify got wrong-phase reply",
                    )
                    .into()),
                }
            }
            .await;
            result.map_err(qualification_error)
        })
    }

    fn validate_backup_address(
        &self,
        path: crate::engine::domain::RelativePath,
    ) -> futures::future::BoxFuture<'_, crate::engine::namespace::Result<()>> {
        Box::pin(async move {
            let result = async {
                ensure_compatible_path_encoding(self.peer)?;
                let path = encode_relative_path(path.as_path())?;
                match request(
                    &self.sender,
                    WireNamespaceRequest::ValidateBackupAddress(path),
                )
                .await?
                {
                    WireNamespaceResult::BackupAddressValidated => Ok(()),
                    _ => Err(ProtocolError::InvalidMessage(
                        "backup address validation got wrong-phase reply",
                    )
                    .into()),
                }
            }
            .await;
            result.map_err(qualification_error)
        })
    }

    fn seal(&self) -> futures::future::BoxFuture<'_, crate::engine::namespace::Result<()>> {
        Box::pin(async move {
            let result = match request(&self.sender, WireNamespaceRequest::Seal).await {
                Ok(WireNamespaceResult::Sealed) => Ok(()),
                Ok(_) => Err(
                    ProtocolError::InvalidMessage("namespace seal got wrong-phase reply").into(),
                ),
                Err(error) => Err(error),
            };
            result.map_err(qualification_error)
        })
    }
}

fn qualification_error(
    error: NamespaceRpcError,
) -> crate::engine::namespace::NamespacePreflightError {
    crate::engine::namespace::NamespacePreflightError::Io(std::io::Error::other(error))
}

async fn request(
    sender: &RouterSender,
    request: WireNamespaceRequest,
) -> Result<WireNamespaceResult> {
    let mut inbox = sender.open_stream()?;
    let id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::NamespaceRequest,
            FrameFlags::FINAL,
            id,
            request.encode()?,
        )?)
        .await?;
    let frame = receive(&mut inbox, FrameKind::NamespaceResult).await?;
    require_frame(
        frame.frame(),
        id,
        FrameKind::NamespaceResult,
        FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
    )?;
    let reply = WireNamespaceResult::decode(frame.frame().payload())?;
    drop(frame);
    sender
        .send(Frame::new(
            FrameKind::Ack,
            FrameFlags::empty(),
            id,
            bytes::Bytes::new(),
        )?)
        .await?;
    Ok(reply)
}

async fn receive(
    inbox: &mut StreamInbox,
    expected: FrameKind,
) -> Result<crate::remote::router::RoutedFrame> {
    inbox
        .recv()
        .await?
        .ok_or(NamespaceRpcError::MissingFrame(expected))
}

fn require_frame(frame: &Frame, id: StreamId, kind: FrameKind, flags: FrameFlags) -> Result<()> {
    if frame.stream_id() != id || frame.kind() != kind || frame.flags() != flags {
        return Err(ProtocolError::InvalidMessage("invalid namespace frame or flags").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_namespace_frames_require_exact_readonly_flags() {
        let id = StreamId::new(1);
        for (kind, required) in [
            (FrameKind::NamespaceRequest, FrameFlags::FINAL),
            (
                FrameKind::NamespaceResult,
                FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            ),
            (FrameKind::Ack, FrameFlags::empty()),
        ] {
            for flags in [
                FrameFlags::empty(),
                FrameFlags::FINAL,
                FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
                FrameFlags::from_bits_retain(0x80),
            ] {
                let frame = Frame::new(kind, flags, id, bytes::Bytes::new()).unwrap();
                assert_eq!(
                    require_frame(&frame, id, kind, required).is_ok(),
                    flags == required
                );
            }
        }
    }
}
