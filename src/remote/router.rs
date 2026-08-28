use crate::protocol::{read_frame, write_frame, Frame, FrameKind, StreamId, MAX_FRAME_PAYLOAD};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

const BYTE_QUANTUM: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterRole {
    Client,
    Server,
}

impl RouterRole {
    const fn first_local_stream(self) -> u32 {
        match self {
            Self::Client => 1,
            Self::Server => 2,
        }
    }

    const fn local_owns(self, stream_id: StreamId) -> bool {
        !stream_id.is_control()
            && match self {
                Self::Client => !stream_id.get().is_multiple_of(2),
                Self::Server => stream_id.get().is_multiple_of(2),
            }
    }

    const fn peer_owns(self, stream_id: StreamId) -> bool {
        !stream_id.is_control() && !self.local_owns(stream_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterConfig {
    pub max_active_streams: u32,
    pub max_inbound_frames: u32,
    pub max_inbound_bytes: u64,
    pub max_outbound_frames: u32,
    pub max_outbound_bytes: u64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            max_active_streams: 256,
            max_inbound_frames: 128,
            max_inbound_bytes: 16 * 1024 * 1024,
            max_outbound_frames: 128,
            max_outbound_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("active stream budget must be greater than zero")]
    ZeroStreamBudget,

    #[error("{direction} frame budget must be greater than zero")]
    ZeroFrameBudget { direction: &'static str },

    #[error(
        "{direction} byte budget must be at least one maximum protocol frame ({minimum} bytes), got {budget}"
    )]
    ByteBudgetTooSmall {
        direction: &'static str,
        budget: u64,
        minimum: u64,
    },

    #[error(
        "{direction} byte budget is too large to represent: {bytes} bytes with {quantum}-byte permits"
    )]
    ByteBudgetTooLarge {
        direction: &'static str,
        bytes: u64,
        quantum: u64,
    },

    #[error("frame router state lock was poisoned")]
    StatePoisoned,

    #[error("frame router is shutting down")]
    ShuttingDown,

    #[error("local stream id space exhausted")]
    StreamIdExhausted,

    #[error("stream {0} is already registered")]
    StreamAlreadyRegistered(u32),

    #[error("active stream limit reached ({0})")]
    TooManyStreams(u32),

    #[error("stream {0} was closed while the peer was still sending frames")]
    StreamClosed(u32),

    #[error("peer attempted to open stream {stream_id} from the local {role:?} id namespace")]
    InvalidPeerStreamId { role: RouterRole, stream_id: u32 },

    #[error("peer attempted to open stream {stream_id} with non-opening frame {kind:?}")]
    InvalidStreamOpen { stream_id: u32, kind: FrameKind },

    #[error("incoming stream acceptor is closed")]
    IncomingClosed,

    #[error("frame router writer is closed")]
    WriterClosed,

    #[error("frame router {direction} budget semaphore was closed")]
    BudgetClosed { direction: &'static str },

    #[error(transparent)]
    Protocol(#[from] crate::protocol::ProtocolError),
}

pub type SharedRouterError = Arc<RouterError>;

enum StreamMessage {
    Frame(RoutedFrame),
    Failed(SharedRouterError),
}

enum IncomingMessage {
    Stream(IncomingStream),
    Failed(SharedRouterError),
}

struct OutboundFrame {
    frame: Frame,
    _frame_permit: OwnedSemaphorePermit,
    _byte_permit: Option<OwnedSemaphorePermit>,
}

/// A received frame that retains its global router capacity until dropped.
///
/// Keeping the permits attached to the frame means queue memory remains bounded
/// even though individual stream inboxes use unbounded channels internally.
pub struct RoutedFrame {
    frame: Frame,
    _frame_permit: OwnedSemaphorePermit,
    _byte_permit: Option<OwnedSemaphorePermit>,
}

impl RoutedFrame {
    pub const fn frame(&self) -> &Frame {
        &self.frame
    }
}

impl fmt::Debug for RoutedFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.frame.fmt(formatter)
    }
}

struct RouterInner {
    streams: Mutex<HashMap<StreamId, mpsc::UnboundedSender<StreamMessage>>>,
    failure: Mutex<Option<SharedRouterError>>,
    incoming_tx: mpsc::UnboundedSender<IncomingMessage>,
    inbound_frames: Arc<Semaphore>,
    inbound_bytes: Arc<Semaphore>,
    outbound_frames: Arc<Semaphore>,
    outbound_bytes: Arc<Semaphore>,
    shutdown: watch::Sender<bool>,
    config: RouterConfig,
    role: RouterRole,
    next_stream_id: AtomicU32,
}

#[derive(Clone)]
pub struct RouterSender {
    inner: Arc<RouterInner>,
    outbound_tx: mpsc::UnboundedSender<OutboundFrame>,
}

impl RouterSender {
    /// Allocate and register a locally initiated non-zero stream.
    ///
    /// Client-initiated streams are odd and server-initiated streams are even.
    /// Registration happens before the caller can send the opening frame, so a
    /// fast peer response cannot race ahead of the inbox.
    pub fn open_stream(&self) -> Result<StreamInbox, SharedRouterError> {
        let raw = self
            .inner
            .next_stream_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                if current == 0 {
                    None
                } else {
                    Some(current.checked_add(2).unwrap_or(0))
                }
            })
            .map_err(|_| Arc::new(RouterError::StreamIdExhausted))?;
        register_stream(&self.inner, StreamId::new(raw))
    }

    /// Queue a frame for the single serialized transport writer.
    ///
    /// Frame-count and byte permits are acquired before enqueueing and remain
    /// held until the writer finishes the frame, providing backpressure without
    /// a second hidden buffering policy.
    pub async fn send(&self, frame: Frame) -> Result<(), SharedRouterError> {
        if let Some(failure) = current_failure(&self.inner)? {
            return Err(failure);
        }

        let (frame_permit, byte_permit) = acquire_capacity(
            Arc::clone(&self.inner.outbound_frames),
            Arc::clone(&self.inner.outbound_bytes),
            frame.payload().len(),
            self.inner.config.max_outbound_bytes,
            "outbound",
        )
        .await
        .map_err(Arc::new)?;
        let queued = OutboundFrame {
            frame,
            _frame_permit: frame_permit,
            _byte_permit: byte_permit,
        };
        self.outbound_tx
            .send(queued)
            .map_err(|_| Arc::new(RouterError::WriterClosed))
    }
}

/// Inbox for one protocol stream.
pub struct StreamInbox {
    stream_id: StreamId,
    receiver: mpsc::UnboundedReceiver<StreamMessage>,
    inner: Weak<RouterInner>,
}

impl StreamInbox {
    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    pub async fn recv(&mut self) -> Result<Option<RoutedFrame>, SharedRouterError> {
        match self.receiver.recv().await {
            Some(StreamMessage::Frame(frame)) => Ok(Some(frame)),
            Some(StreamMessage::Failed(error)) => Err(error),
            None => match self.inner.upgrade() {
                Some(inner) => match current_failure(&inner)? {
                    Some(error) => Err(error),
                    None => Ok(None),
                },
                None => Ok(None),
            },
        }
    }
}

impl Drop for StreamInbox {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if let Ok(mut streams) = inner.streams.lock() {
            streams.remove(&self.stream_id);
        };
    }
}

/// First frame and inbox for a stream initiated by the peer.
pub struct IncomingStream {
    pub first: RoutedFrame,
    pub inbox: StreamInbox,
}

impl IncomingStream {
    pub const fn stream_id(&self) -> StreamId {
        self.inbox.stream_id()
    }
}

pub struct IncomingStreams {
    receiver: mpsc::UnboundedReceiver<IncomingMessage>,
}

impl IncomingStreams {
    pub async fn recv(&mut self) -> Result<Option<IncomingStream>, SharedRouterError> {
        match self.receiver.recv().await {
            Some(IncomingMessage::Stream(stream)) => Ok(Some(stream)),
            Some(IncomingMessage::Failed(error)) => Err(error),
            None => Ok(None),
        }
    }
}

pub struct RouterTasks {
    reader: JoinHandle<Result<(), SharedRouterError>>,
    writer: JoinHandle<Result<(), SharedRouterError>>,
}

impl RouterTasks {
    pub fn abort(&self) {
        self.reader.abort();
        self.writer.abort();
    }
}

impl Drop for RouterTasks {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Owns the central reader/writer actors for one already-negotiated transport.
///
/// The router starts after the v3 control-plane handshake. Control stream 0 is
/// registered immediately for later Error/Ping/Pong traffic; locally opened
/// streams are registered before use, while unknown peer-owned streams are
/// delivered through `IncomingStreams` with their first frame attached.
pub struct FrameRouter {
    sender: RouterSender,
    control: StreamInbox,
    incoming: IncomingStreams,
    tasks: RouterTasks,
}

impl FrameRouter {
    pub fn start<R, W>(
        reader: R,
        writer: W,
        role: RouterRole,
        config: RouterConfig,
    ) -> Result<Self, RouterError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let inbound_byte_units = validate_config(config)?;
        let outbound_byte_units = byte_units(config.max_outbound_bytes, "outbound")?;
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let inner = Arc::new(RouterInner {
            streams: Mutex::new(HashMap::new()),
            failure: Mutex::new(None),
            incoming_tx,
            inbound_frames: Arc::new(Semaphore::new(config.max_inbound_frames as usize)),
            inbound_bytes: Arc::new(Semaphore::new(inbound_byte_units as usize)),
            outbound_frames: Arc::new(Semaphore::new(config.max_outbound_frames as usize)),
            outbound_bytes: Arc::new(Semaphore::new(outbound_byte_units as usize)),
            shutdown: shutdown_tx,
            config,
            role,
            next_stream_id: AtomicU32::new(role.first_local_stream()),
        });

        let control = register_stream_plain(&inner, StreamId::CONTROL)?;
        let sender = RouterSender {
            inner: Arc::clone(&inner),
            outbound_tx,
        };

        let reader_inner = Arc::clone(&inner);
        let reader_shutdown = shutdown_rx.clone();
        let reader_task = tokio::spawn(async move {
            match reader_loop(reader, &reader_inner, reader_shutdown).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    let error = Arc::new(error);
                    publish_failure(&reader_inner, Arc::clone(&error));
                    Err(error)
                }
            }
        });

        let writer_inner = Arc::clone(&inner);
        let writer_task = tokio::spawn(async move {
            match writer_loop(writer, outbound_rx, shutdown_rx).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    let error = Arc::new(error);
                    publish_failure(&writer_inner, Arc::clone(&error));
                    Err(error)
                }
            }
        });

        Ok(Self {
            sender,
            control,
            incoming: IncomingStreams {
                receiver: incoming_rx,
            },
            tasks: RouterTasks {
                reader: reader_task,
                writer: writer_task,
            },
        })
    }

    pub fn sender(&self) -> RouterSender {
        self.sender.clone()
    }

    pub fn control(&mut self) -> &mut StreamInbox {
        &mut self.control
    }

    pub fn incoming(&mut self) -> &mut IncomingStreams {
        &mut self.incoming
    }

    pub fn tasks(&self) -> &RouterTasks {
        &self.tasks
    }
}

async fn reader_loop<R>(
    mut reader: R,
    inner: &Arc<RouterInner>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RouterError>
where
    R: AsyncRead + Unpin,
{
    loop {
        // `read_frame` validates the 1 MiB protocol payload cap before
        // allocation. The router then admits the completed frame into its
        // global queue budget, so resident inbound memory is bounded by the
        // configured budget plus at most one frame currently being read.
        let frame = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
            result = read_frame(&mut reader) => result?,
        };
        let (frame_permit, byte_permit) = acquire_capacity(
            Arc::clone(&inner.inbound_frames),
            Arc::clone(&inner.inbound_bytes),
            frame.payload().len(),
            inner.config.max_inbound_bytes,
            "inbound",
        )
        .await?;
        route_inbound(
            inner,
            RoutedFrame {
                frame,
                _frame_permit: frame_permit,
                _byte_permit: byte_permit,
            },
        )?;
    }
}

async fn writer_loop<W>(
    mut writer: W,
    mut receiver: mpsc::UnboundedReceiver<OutboundFrame>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RouterError>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let queued = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
            queued = receiver.recv() => queued,
        };
        let Some(queued) = queued else {
            writer
                .flush()
                .await
                .map_err(crate::protocol::ProtocolError::from)?;
            return Ok(());
        };
        write_frame(&mut writer, &queued.frame).await?;
        // `queued` drops here, releasing frame and byte capacity only after the
        // bytes have been accepted by the underlying AsyncWrite.
    }
}

fn route_inbound(inner: &Arc<RouterInner>, routed: RoutedFrame) -> Result<(), RouterError> {
    {
        let failure = inner
            .failure
            .lock()
            .map_err(|_| RouterError::StatePoisoned)?;
        if failure.is_some() {
            return Err(RouterError::ShuttingDown);
        }
    }

    let stream_id = routed.frame.stream_id();
    let sender = {
        let mut streams = inner
            .streams
            .lock()
            .map_err(|_| RouterError::StatePoisoned)?;
        if let Some(sender) = streams.get(&stream_id) {
            sender.clone()
        } else if !inner.role.peer_owns(stream_id) {
            return Err(RouterError::InvalidPeerStreamId {
                role: inner.role,
                stream_id: stream_id.get(),
            });
        } else if !is_stream_opening_kind(routed.frame.kind()) {
            return Err(RouterError::InvalidStreamOpen {
                stream_id: stream_id.get(),
                kind: routed.frame.kind(),
            });
        } else {
            ensure_stream_capacity(inner, &streams)?;
            let (sender, receiver) = mpsc::unbounded_channel();
            streams.insert(stream_id, sender);
            let inbox = StreamInbox {
                stream_id,
                receiver,
                inner: Arc::downgrade(inner),
            };
            drop(streams);
            inner
                .incoming_tx
                .send(IncomingMessage::Stream(IncomingStream {
                    first: routed,
                    inbox,
                }))
                .map_err(|_| RouterError::IncomingClosed)?;
            return Ok(());
        }
    };

    sender
        .send(StreamMessage::Frame(routed))
        .map_err(|_| RouterError::StreamClosed(stream_id.get()))
}

fn is_stream_opening_kind(kind: FrameKind) -> bool {
    matches!(
        kind,
        FrameKind::ScanRequest | FrameKind::SignatureRequest | FrameKind::FileBegin
    )
}

fn register_stream(
    inner: &Arc<RouterInner>,
    stream_id: StreamId,
) -> Result<StreamInbox, SharedRouterError> {
    // Match `publish_failure`'s lock order. Holding the failure guard through
    // registration prevents a stream from being inserted just after failure
    // broadcast drained the registry.
    let failure = inner
        .failure
        .lock()
        .map_err(|_| Arc::new(RouterError::StatePoisoned))?;
    if let Some(error) = failure.as_ref() {
        return Err(Arc::clone(error));
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut streams = inner
        .streams
        .lock()
        .map_err(|_| Arc::new(RouterError::StatePoisoned))?;
    if streams.contains_key(&stream_id) {
        return Err(Arc::new(RouterError::StreamAlreadyRegistered(
            stream_id.get(),
        )));
    }
    ensure_stream_capacity(inner, &streams).map_err(Arc::new)?;
    streams.insert(stream_id, sender);
    Ok(StreamInbox {
        stream_id,
        receiver,
        inner: Arc::downgrade(inner),
    })
}

fn register_stream_plain(
    inner: &Arc<RouterInner>,
    stream_id: StreamId,
) -> Result<StreamInbox, RouterError> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut streams = inner
        .streams
        .lock()
        .map_err(|_| RouterError::StatePoisoned)?;
    if streams.contains_key(&stream_id) {
        return Err(RouterError::StreamAlreadyRegistered(stream_id.get()));
    }
    streams.insert(stream_id, sender);
    Ok(StreamInbox {
        stream_id,
        receiver,
        inner: Arc::downgrade(inner),
    })
}

fn ensure_stream_capacity(
    inner: &RouterInner,
    streams: &HashMap<StreamId, mpsc::UnboundedSender<StreamMessage>>,
) -> Result<(), RouterError> {
    let active = streams
        .len()
        .saturating_sub(usize::from(streams.contains_key(&StreamId::CONTROL)));
    if active >= inner.config.max_active_streams as usize {
        return Err(RouterError::TooManyStreams(inner.config.max_active_streams));
    }
    Ok(())
}

fn current_failure(
    inner: &Arc<RouterInner>,
) -> Result<Option<SharedRouterError>, SharedRouterError> {
    inner
        .failure
        .lock()
        .map(|failure| failure.clone())
        .map_err(|_| Arc::new(RouterError::StatePoisoned))
}

fn publish_failure(inner: &Arc<RouterInner>, error: SharedRouterError) {
    let first_failure = match inner.failure.lock() {
        Ok(mut failure) => {
            if failure.is_some() {
                false
            } else {
                *failure = Some(Arc::clone(&error));
                true
            }
        }
        Err(_) => false,
    };
    if !first_failure {
        return;
    }

    let _ = inner.shutdown.send(true);
    inner.inbound_frames.close();
    inner.inbound_bytes.close();
    inner.outbound_frames.close();
    inner.outbound_bytes.close();

    if let Ok(mut streams) = inner.streams.lock() {
        for (_, sender) in streams.drain() {
            let _ = sender.send(StreamMessage::Failed(Arc::clone(&error)));
        }
    }
    let _ = inner
        .incoming_tx
        .send(IncomingMessage::Failed(Arc::clone(&error)));
}

fn validate_config(config: RouterConfig) -> Result<u32, RouterError> {
    if config.max_active_streams == 0 {
        return Err(RouterError::ZeroStreamBudget);
    }
    if config.max_inbound_frames == 0 {
        return Err(RouterError::ZeroFrameBudget {
            direction: "inbound",
        });
    }
    if config.max_outbound_frames == 0 {
        return Err(RouterError::ZeroFrameBudget {
            direction: "outbound",
        });
    }
    validate_byte_budget(config.max_inbound_bytes, "inbound")?;
    validate_byte_budget(config.max_outbound_bytes, "outbound")?;
    byte_units(config.max_inbound_bytes, "inbound")
}

fn validate_byte_budget(bytes: u64, direction: &'static str) -> Result<(), RouterError> {
    let minimum = MAX_FRAME_PAYLOAD as u64;
    if bytes < minimum {
        return Err(RouterError::ByteBudgetTooSmall {
            direction,
            budget: bytes,
            minimum,
        });
    }
    byte_units(bytes, direction).map(|_| ())
}

fn byte_units(bytes: u64, direction: &'static str) -> Result<u32, RouterError> {
    let units = bytes.div_ceil(BYTE_QUANTUM);
    u32::try_from(units).map_err(|_| RouterError::ByteBudgetTooLarge {
        direction,
        bytes,
        quantum: BYTE_QUANTUM,
    })
}

fn frame_byte_units(bytes: usize) -> u32 {
    if bytes == 0 {
        0
    } else {
        // A protocol frame is capped at 1 MiB, so this conversion cannot exceed
        // u32 on any supported target.
        (bytes as u64).div_ceil(BYTE_QUANTUM) as u32
    }
}

async fn acquire_capacity(
    frames: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    payload_len: usize,
    byte_budget: u64,
    direction: &'static str,
) -> Result<(OwnedSemaphorePermit, Option<OwnedSemaphorePermit>), RouterError> {
    if payload_len as u64 > byte_budget {
        return Err(RouterError::ByteBudgetTooSmall {
            direction,
            budget: byte_budget,
            minimum: payload_len as u64,
        });
    }

    let frame_permit = frames
        .acquire_owned()
        .await
        .map_err(|_| RouterError::BudgetClosed { direction })?;
    let units = frame_byte_units(payload_len);
    let byte_permit = if units == 0 {
        None
    } else {
        Some(
            bytes
                .acquire_many_owned(units)
                .await
                .map_err(|_| RouterError::BudgetClosed { direction })?,
        )
    };
    Ok((frame_permit, byte_permit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FrameFlags;
    use bytes::Bytes;
    use std::time::Duration;

    fn frame(kind: FrameKind, stream_id: StreamId, payload: &'static [u8]) -> Frame {
        Frame::new(
            kind,
            FrameFlags::empty(),
            stream_id,
            Bytes::from_static(payload),
        )
        .unwrap()
    }

    #[test]
    fn rejects_unbounded_or_impossibly_small_configs() {
        let zero_streams = RouterConfig {
            max_active_streams: 0,
            ..RouterConfig::default()
        };
        assert!(matches!(
            FrameRouter::start(
                tokio::io::empty(),
                tokio::io::sink(),
                RouterRole::Client,
                zero_streams
            ),
            Err(RouterError::ZeroStreamBudget)
        ));

        let zero_frames = RouterConfig {
            max_inbound_frames: 0,
            ..RouterConfig::default()
        };
        assert!(matches!(
            FrameRouter::start(
                tokio::io::empty(),
                tokio::io::sink(),
                RouterRole::Client,
                zero_frames
            ),
            Err(RouterError::ZeroFrameBudget {
                direction: "inbound"
            })
        ));

        let small = RouterConfig {
            max_inbound_bytes: (MAX_FRAME_PAYLOAD - 1) as u64,
            ..RouterConfig::default()
        };
        assert!(matches!(
            FrameRouter::start(
                tokio::io::empty(),
                tokio::io::sink(),
                RouterRole::Client,
                small
            ),
            Err(RouterError::ByteBudgetTooSmall {
                direction: "inbound",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn allocates_disjoint_client_and_server_stream_namespaces() {
        let (client_io, _peer) = tokio::io::duplex(4096);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let client = FrameRouter::start(
            client_reader,
            client_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();
        assert_eq!(client.sender().open_stream().unwrap().stream_id().get(), 1);
        assert_eq!(client.sender().open_stream().unwrap().stream_id().get(), 3);

        let (server_io, _peer) = tokio::io::duplex(4096);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = FrameRouter::start(
            server_reader,
            server_writer,
            RouterRole::Server,
            RouterConfig::default(),
        )
        .unwrap();
        assert_eq!(server.sender().open_stream().unwrap().stream_id().get(), 2);
        assert_eq!(server.sender().open_stream().unwrap().stream_id().get(), 4);
    }

    #[tokio::test]
    async fn active_stream_limit_is_enforced() {
        let config = RouterConfig {
            max_active_streams: 1,
            ..RouterConfig::default()
        };
        let (router_io, _peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let router = FrameRouter::start(reader, writer, RouterRole::Client, config).unwrap();
        let first = router.sender().open_stream().unwrap();
        let error = match router.sender().open_stream() {
            Err(error) => error,
            Ok(_) => panic!("expected active stream limit error"),
        };
        assert!(matches!(error.as_ref(), RouterError::TooManyStreams(1)));
        drop(first);
        assert!(router.sender().open_stream().is_ok());
    }

    #[tokio::test]
    async fn routes_registered_streams() {
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (mut peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let router = FrameRouter::start(
            router_reader,
            router_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();
        let mut inbox = router.sender().open_stream().unwrap();
        let stream_id = inbox.stream_id();

        write_frame(
            &mut peer_writer,
            &frame(FrameKind::Entry, stream_id, b"metadata"),
        )
        .await
        .unwrap();
        let routed = inbox.recv().await.unwrap().unwrap();
        assert_eq!(routed.frame().kind(), FrameKind::Entry);
        assert_eq!(routed.frame().payload(), &Bytes::from_static(b"metadata"));

        router
            .sender()
            .send(frame(FrameKind::Ack, stream_id, b"ok"))
            .await
            .unwrap();
        let outbound = read_frame(&mut peer_reader).await.unwrap();
        assert_eq!(outbound.kind(), FrameKind::Ack);
        assert_eq!(outbound.stream_id(), stream_id);
    }

    #[tokio::test]
    async fn accepts_peer_opened_stream_and_routes_followups() {
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (_peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let mut router = FrameRouter::start(
            router_reader,
            router_writer,
            RouterRole::Server,
            RouterConfig::default(),
        )
        .unwrap();
        let stream_id = StreamId::new(77);

        write_frame(
            &mut peer_writer,
            &frame(FrameKind::ScanRequest, stream_id, b"request"),
        )
        .await
        .unwrap();
        let mut incoming = router.incoming().recv().await.unwrap().unwrap();
        assert_eq!(incoming.stream_id(), stream_id);
        assert_eq!(incoming.first.frame().kind(), FrameKind::ScanRequest);

        write_frame(
            &mut peer_writer,
            &frame(FrameKind::Entry, stream_id, b"entry"),
        )
        .await
        .unwrap();
        let followup = incoming.inbox.recv().await.unwrap().unwrap();
        assert_eq!(followup.frame().kind(), FrameKind::Entry);
    }

    #[tokio::test]
    async fn rejects_peer_streams_from_local_namespace() {
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (_peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let mut router = FrameRouter::start(
            router_reader,
            router_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();

        write_frame(
            &mut peer_writer,
            &frame(FrameKind::ScanRequest, StreamId::new(3), b"bad"),
        )
        .await
        .unwrap();
        let error = match router.incoming().recv().await {
            Err(error) => error,
            Ok(_) => panic!("expected peer namespace error"),
        };
        assert!(matches!(
            error.as_ref(),
            RouterError::InvalidPeerStreamId {
                role: RouterRole::Client,
                stream_id: 3
            }
        ));
    }

    #[tokio::test]
    async fn rejects_non_opening_frame_for_unknown_peer_stream() {
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (_peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let mut router = FrameRouter::start(
            router_reader,
            router_writer,
            RouterRole::Server,
            RouterConfig::default(),
        )
        .unwrap();

        write_frame(
            &mut peer_writer,
            &frame(FrameKind::Entry, StreamId::new(77), b"bad"),
        )
        .await
        .unwrap();
        let error = match router.incoming().recv().await {
            Err(error) => error,
            Ok(_) => panic!("expected stream opener error"),
        };
        assert!(matches!(
            error.as_ref(),
            RouterError::InvalidStreamOpen {
                stream_id: 77,
                kind: FrameKind::Entry
            }
        ));
    }

    #[tokio::test]
    async fn inbound_frame_budget_applies_backpressure_until_frame_drop() {
        let config = RouterConfig {
            max_inbound_frames: 1,
            ..RouterConfig::default()
        };
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (_peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let router =
            FrameRouter::start(router_reader, router_writer, RouterRole::Client, config).unwrap();
        let mut inbox = router.sender().open_stream().unwrap();
        let stream_id = inbox.stream_id();

        write_frame(
            &mut peer_writer,
            &frame(FrameKind::Entry, stream_id, b"first"),
        )
        .await
        .unwrap();
        write_frame(
            &mut peer_writer,
            &frame(FrameKind::Entry, stream_id, b"second"),
        )
        .await
        .unwrap();

        let first = inbox.recv().await.unwrap().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), inbox.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(second.frame().payload(), &Bytes::from_static(b"second"));
    }

    #[tokio::test]
    async fn control_stream_is_pre_registered() {
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (_peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let mut router = FrameRouter::start(
            router_reader,
            router_writer,
            RouterRole::Client,
            RouterConfig::default(),
        )
        .unwrap();

        write_frame(
            &mut peer_writer,
            &frame(FrameKind::Ping, StreamId::CONTROL, b"ping"),
        )
        .await
        .unwrap();
        let routed = router.control().recv().await.unwrap().unwrap();
        assert_eq!(routed.frame().kind(), FrameKind::Ping);
    }
}
