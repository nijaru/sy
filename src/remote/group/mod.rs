//! One live, bounded hardlink-group stream owns its native resources through
//! exact declaration replay and the final completion acknowledgement.
mod declarations;
mod destination;
pub(crate) mod destination_client;
mod source;
pub(crate) mod source_client;

pub(crate) async fn serve_destination(
    rooted: crate::rooted_fs::RootedFs,
    incoming: crate::remote::router::IncomingStream,
    sender: &RouterSender,
    peer: crate::protocol::PlatformOs,
    resolved: crate::protocol::ResolvedOperand,
) -> Result<()> {
    destination::serve(rooted, incoming, sender, peer, resolved).await
}

pub(crate) async fn serve_source(
    rooted: crate::rooted_fs::RootedFs,
    incoming: crate::remote::router::IncomingStream,
    sender: &RouterSender,
    peer: crate::protocol::PlatformOs,
    resolved: crate::protocol::ResolvedOperand,
) -> Result<()> {
    source::serve(rooted, incoming, sender, peer, resolved).await
}

use crate::protocol::{Frame, FrameFlags, FrameKind, ProtocolError, StreamId, WireGroupResult};
use crate::remote::router::{RouterError, RouterSender, StreamInbox};

#[derive(Debug, thiserror::Error)]
pub(crate) enum GroupError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Router(#[from] std::sync::Arc<RouterError>),
    #[error(transparent)]
    Path(#[from] crate::remote::path::RemotePathError),
    #[error(transparent)]
    Scope(#[from] crate::remote::runtime::scope::ScopeError),
    #[error(transparent)]
    Declarations(#[from] declarations::DeclarationError),
    #[error(transparent)]
    Source(#[from] crate::remote::source_file::SourceFileError),
    #[error(transparent)]
    Transfer(#[from] crate::remote::transfer::RemoteTransferError),
    #[error(transparent)]
    Rooted(#[from] crate::rooted_fs::RootedFsError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Existing(#[from] crate::endpoint::existing::ExistingDestinationError),
    #[error("group failed after {published} publications: {source}")]
    PartialPublication {
        published: u64,
        source: Box<GroupError>,
    },
    #[error("group operation failed: {operation}; private cleanup failed: {cleanup}")]
    Cleanup {
        operation: Box<GroupError>,
        cleanup: Box<GroupError>,
    },
    #[error("invalid group stream phase: {0}")]
    Phase(&'static str),
    #[error("group native worker failed: {0}")]
    Worker(String),
}

type Result<T> = std::result::Result<T, GroupError>;

async fn send_result(
    sender: &RouterSender,
    stream: StreamId,
    result: WireGroupResult,
) -> Result<()> {
    sender
        .send(Frame::new(
            FrameKind::GroupResult,
            FrameFlags::empty(),
            stream,
            result.encode(),
        )?)
        .await?;
    Ok(())
}

async fn next_frame(inbox: &mut StreamInbox) -> Result<crate::remote::router::RoutedFrame> {
    let routed = inbox
        .recv()
        .await?
        .ok_or(GroupError::Phase("group stream ended before completion"))?;
    let frame = routed.frame();
    if frame.stream_id() != inbox.stream_id() || !frame.flags().is_empty() {
        return Err(GroupError::Phase("group command stream or flags"));
    }
    Ok(routed)
}
