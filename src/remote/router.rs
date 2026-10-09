use crate::protocol::{
    read_frame_or_eof, write_frame, Frame, FrameKind, ReadFrame, StreamId, MAX_FRAME_PAYLOAD,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
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
    /// Optional outbound `Data` payload pacing in bytes per second across all
    /// multiplexed streams (`--bwlimit`). Token-bucket with a one-second
    /// burst, matching the legacy limiter. Only Data incurs pacing; other
    /// frames have no additional delay but retain serialized queue order.
    pub outbound_payload_limit: Option<u64>,
    /// Optional inbound progress deadline (`--timeout`), covering both frame
    /// reads and admission/control-response backpressure. `None` disables it.
    pub idle_timeout: Option<std::time::Duration>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            max_active_streams: 256,
            max_inbound_frames: 128,
            max_inbound_bytes: 16 * 1024 * 1024,
            max_outbound_frames: 128,
            max_outbound_bytes: 16 * 1024 * 1024,
            outbound_payload_limit: None,
            idle_timeout: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("active stream budget must be greater than zero")]
    ZeroStreamBudget,

    #[error("no inbound progress within the configured I/O timeout (--timeout); aborting session")]
    IdleTimeoutExceeded,

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

    #[error("transport reached EOF")]
    TransportEof,

    #[error("peer terminated the session with a control Error")]
    PeerError,

    #[error("unexpected control frame: {0:?}")]
    UnexpectedControl(FrameKind),

    #[error("session failed: {0}")]
    SessionFailed(String),

    #[error("transport task failed: {0}")]
    TaskFailed(String),

    #[error("transport {0} actor stopped before session termination")]
    ActorStopped(&'static str),

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

/// The first terminal transition wins. EOF stops admission and transport I/O,
/// but already received stream responses remain readable. Failure discards work.
#[derive(Clone)]
enum Terminal {
    Eof,
    Failed(SharedRouterError),
}

impl Terminal {
    fn error(&self) -> SharedRouterError {
        match self {
            Self::Eof => Arc::new(RouterError::TransportEof),
            Self::Failed(error) => Arc::clone(error),
        }
    }
}

struct OutboundFrame {
    frame: Frame,
    written: Option<oneshot::Sender<()>>,
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
    // This lock serializes registry/admission changes with terminal publication.
    streams: Mutex<HashMap<StreamId, mpsc::UnboundedSender<RoutedFrame>>>,
    terminal: watch::Sender<Option<Terminal>>,
    publication: Arc<crate::endpoint::publication::PublicationAdmission>,
    incoming_tx: mpsc::UnboundedSender<IncomingStream>,
    inbound_frames: Arc<Semaphore>,
    inbound_bytes: Arc<Semaphore>,
    outbound_frames: Arc<Semaphore>,
    outbound_bytes: Arc<Semaphore>,
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
    #[allow(deprecated)]
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
    /// a second hidden buffering policy. Acknowledgements additionally wait for
    /// the writer to flush them so a completed operation cannot lose its final
    /// success signal when the router is dropped immediately afterward.
    pub async fn send(&self, frame: Frame) -> Result<(), SharedRouterError> {
        let mut terminal = self.inner.terminal.subscribe();
        self.check_active()?;
        let (frame_permit, byte_permit) = tokio::select! {
            biased;
            state = terminated(&mut terminal) => return Err(state.error()),
            capacity = acquire_capacity(
                Arc::clone(&self.inner.outbound_frames),
                Arc::clone(&self.inner.outbound_bytes),
                frame.payload().len(),
                self.inner.config.max_outbound_bytes,
                "outbound",
            ) => capacity.map_err(Arc::new)?,
        };
        let (written, completion) = if frame.kind() == FrameKind::Ack {
            let (written, completion) = oneshot::channel();
            (Some(written), Some(completion))
        } else {
            (None, None)
        };
        let queued = OutboundFrame {
            frame,
            written,
            _frame_permit: frame_permit,
            _byte_permit: byte_permit,
        };
        {
            let _streams = self
                .inner
                .streams
                .lock()
                .map_err(|_| Arc::new(RouterError::StatePoisoned))?;
            self.check_active()?;
            self.outbound_tx
                .send(queued)
                .map_err(|_| Arc::new(RouterError::WriterClosed))?;
        }
        if let Some(completion) = completion {
            tokio::select! {
                biased;
                // A flushed Ack remains successful if EOF arrives before the
                // waiter is next polled. Otherwise completion is uncertain.
                completed = completion => completed.map_err(|_| {
                    self.check_active().err().unwrap_or_else(|| Arc::new(RouterError::WriterClosed))
                })?,
                state = terminated(&mut terminal) => return Err(state.error()),
            }
        }
        Ok(())
    }

    /// A point-in-time cancellation check, not a commit lock or rollback guarantee.
    pub(crate) fn check_active(&self) -> Result<(), SharedRouterError> {
        match self.inner.terminal.borrow().as_ref() {
            Some(state) => Err(state.error()),
            None => Ok(()),
        }
    }

    /// Wake producers even if traversal/read work has not yielded its first item.
    pub(crate) async fn closed(&self) -> SharedRouterError {
        let mut terminal = self.inner.terminal.subscribe();
        terminated(&mut terminal).await.error()
    }

    pub(crate) fn publication_admission(
        &self,
    ) -> Arc<crate::endpoint::publication::PublicationAdmission> {
        Arc::clone(&self.inner.publication)
    }

    pub(crate) fn fail(&self, error: SharedRouterError) {
        publish_terminal(&self.inner, Terminal::Failed(error));
    }
}

/// Inbox for one protocol stream.
pub struct StreamInbox {
    stream_id: StreamId,
    receiver: mpsc::UnboundedReceiver<RoutedFrame>,
    terminal: watch::Receiver<Option<Terminal>>,
    inner: Weak<RouterInner>,
}

impl StreamInbox {
    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    pub async fn recv(&mut self) -> Result<Option<RoutedFrame>, SharedRouterError> {
        tokio::select! {
            biased;
            state = terminated(&mut self.terminal) => {
                self.receiver.close();
                match state {
                    Terminal::Failed(error) => {
                        while self.receiver.try_recv().is_ok() {}
                        Err(error)
                    }
                    // A peer may exit immediately after flushing its final Ack.
                    // Retain those already received responses, but never admit
                    // more requests through IncomingStreams after EOF.
                    Terminal::Eof => Ok(self.receiver.recv().await),
                }
            }
            frame = self.receiver.recv() => Ok(frame),
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
    receiver: mpsc::UnboundedReceiver<IncomingStream>,
    terminal: watch::Receiver<Option<Terminal>>,
}

impl IncomingStreams {
    pub async fn recv(&mut self) -> Result<Option<IncomingStream>, SharedRouterError> {
        tokio::select! {
            biased;
            state = terminated(&mut self.terminal) => {
                self.receiver.close();
                while self.receiver.try_recv().is_ok() {}
                match state {
                    Terminal::Eof => Ok(None),
                    Terminal::Failed(error) => Err(error),
                }
            }
            incoming = self.receiver.recv() => Ok(incoming),
        }
    }
}

// Also terminate on actor panic/abort, including before its first poll. All
// ordinary exits publish a terminal state first, so this is only a last-resort
// failure transition; it cannot overwrite the authoritative cause.
struct ActorLifetime {
    inner: Arc<RouterInner>,
    direction: &'static str,
}

impl Drop for ActorLifetime {
    fn drop(&mut self) {
        publish_terminal(
            &self.inner,
            Terminal::Failed(Arc::new(RouterError::ActorStopped(self.direction))),
        );
    }
}

pub struct RouterTasks {
    inner: Arc<RouterInner>,
    reader: JoinHandle<Result<(), SharedRouterError>>,
    writer: JoinHandle<Result<(), SharedRouterError>>,
}

impl RouterTasks {
    pub fn abort(&self) {
        publish_terminal(
            &self.inner,
            Terminal::Failed(Arc::new(RouterError::ShuttingDown)),
        );
        self.reader.abort();
        self.writer.abort();
    }

    async fn finish(&mut self) -> Result<(), SharedRouterError> {
        let (reader, writer) = tokio::join!(&mut self.reader, &mut self.writer);
        for task in [reader, writer] {
            task.map_err(|error| Arc::new(RouterError::TaskFailed(error.to_string())))??;
        }
        Ok(())
    }
}

impl Drop for RouterTasks {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Owns the central reader/writer actors for one already-negotiated transport.
/// Control traffic is handled by the reader, never left in an unconsumed inbox.
pub struct FrameRouter {
    sender: RouterSender,
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
        let (terminal_tx, terminal_rx) = watch::channel(None);
        let inner = Arc::new(RouterInner {
            streams: Mutex::new(HashMap::new()),
            terminal: terminal_tx,
            publication: Arc::new(crate::endpoint::publication::PublicationAdmission::default()),
            incoming_tx,
            inbound_frames: Arc::new(Semaphore::new(config.max_inbound_frames as usize)),
            inbound_bytes: Arc::new(Semaphore::new(inbound_byte_units as usize)),
            outbound_frames: Arc::new(Semaphore::new(config.max_outbound_frames as usize)),
            outbound_bytes: Arc::new(Semaphore::new(outbound_byte_units as usize)),
            config,
            role,
            next_stream_id: AtomicU32::new(role.first_local_stream()),
        });
        let sender = RouterSender {
            inner: Arc::clone(&inner),
            outbound_tx,
        };
        let reader_sender = sender.clone();
        let reader_terminal = terminal_rx.clone();
        let reader_lifetime = ActorLifetime {
            inner: Arc::clone(&inner),
            direction: "reader",
        };
        let reader_task = tokio::spawn(async move {
            let _lifetime = reader_lifetime;
            let result = reader_loop(reader, &reader_sender, reader_terminal).await;
            if let Err(error) = &result {
                reader_sender.fail(Arc::clone(error));
            }
            result
        });
        let writer_inner = Arc::clone(&inner);
        let writer_terminal = terminal_rx.clone();
        let writer_lifetime = ActorLifetime {
            inner: Arc::clone(&inner),
            direction: "writer",
        };
        let writer_task = tokio::spawn(async move {
            let _lifetime = writer_lifetime;
            let result = writer_loop(
                writer,
                outbound_rx,
                &writer_inner,
                writer_terminal,
                config.outbound_payload_limit,
            )
            .await;
            if let Err(error) = &result {
                publish_terminal(&writer_inner, Terminal::Failed(Arc::clone(error)));
            }
            result
        });
        Ok(Self {
            sender,
            incoming: IncomingStreams {
                receiver: incoming_rx,
                terminal: terminal_rx,
            },
            tasks: RouterTasks {
                inner,
                reader: reader_task,
                writer: writer_task,
            },
        })
    }

    pub fn sender(&self) -> RouterSender {
        self.sender.clone()
    }

    pub fn incoming(&mut self) -> &mut IncomingStreams {
        &mut self.incoming
    }

    pub fn tasks(&self) -> &RouterTasks {
        &self.tasks
    }

    /// Stop transport I/O and await its actors. Admitted handlers are owned
    /// and drained by the server, not by the router.
    pub(crate) async fn shutdown(&mut self) -> Result<(), SharedRouterError> {
        self.sender.fail(Arc::new(RouterError::ShuttingDown));
        self.tasks.finish().await
    }
}

async fn terminated(terminal: &mut watch::Receiver<Option<Terminal>>) -> Terminal {
    loop {
        if let Some(state) = terminal.borrow_and_update().clone() {
            return state;
        }
        if terminal.changed().await.is_err() {
            return Terminal::Failed(Arc::new(RouterError::ShuttingDown));
        }
    }
}

async fn reader_loop<R>(
    mut reader: R,
    sender: &RouterSender,
    mut terminal: watch::Receiver<Option<Terminal>>,
) -> Result<(), SharedRouterError>
where
    R: AsyncRead + Unpin,
{
    let inner = &sender.inner;
    loop {
        // The decoder validates the hard payload cap before allocation. Memory
        // stays bounded by retained queue permits plus this one in-progress frame.
        // Include budget waits and control responses in the deadline: otherwise
        // an unconsumed stream can prevent both timeout and EOF observation.
        let receive = async {
            let frame = match read_frame_or_eof(&mut reader)
                .await
                .map_err(RouterError::from)
                .map_err(Arc::new)?
            {
                ReadFrame::CleanEof => return Ok::<_, SharedRouterError>(false),
                ReadFrame::Frame(frame) => frame,
            };
            if frame.stream_id().is_control() {
                handle_control(sender, frame).await?;
                return Ok(true);
            }
            let (frame_permit, byte_permit) = acquire_capacity(
                Arc::clone(&inner.inbound_frames),
                Arc::clone(&inner.inbound_bytes),
                frame.payload().len(),
                inner.config.max_inbound_bytes,
                "inbound",
            )
            .await
            .map_err(Arc::new)?;
            route_inbound(
                inner,
                RoutedFrame {
                    frame,
                    _frame_permit: frame_permit,
                    _byte_permit: byte_permit,
                },
            )
            .map_err(Arc::new)?;
            Ok(true)
        };
        let result = tokio::select! {
            biased;
            _ = terminated(&mut terminal) => return Ok(()),
            result = async {
                match inner.config.idle_timeout {
                    Some(duration) => tokio::time::timeout(duration, receive).await
                        .map_err(|_| Arc::new(RouterError::IdleTimeoutExceeded))?,
                    None => receive.await,
                }
            } => result?,
        };
        if !result {
            publish_terminal(inner, Terminal::Eof);
            return Ok(());
        }
    }
}

async fn handle_control(sender: &RouterSender, frame: Frame) -> Result<(), SharedRouterError> {
    if !frame.flags().is_empty() {
        return Err(Arc::new(RouterError::UnexpectedControl(frame.kind())));
    }
    match frame.kind() {
        FrameKind::Ping => {
            sender
                .send(
                    Frame::control(FrameKind::Pong, frame.into_payload())
                        .map_err(RouterError::from)
                        .map_err(Arc::new)?,
                )
                .await
        }
        FrameKind::Pong => Ok(()),
        FrameKind::Error => Err(Arc::new(RouterError::PeerError)),
        kind => Err(Arc::new(RouterError::UnexpectedControl(kind))),
    }
}

async fn writer_loop<W>(
    mut writer: W,
    mut receiver: mpsc::UnboundedReceiver<OutboundFrame>,
    inner: &Arc<RouterInner>,
    mut terminal: watch::Receiver<Option<Terminal>>,
    payload_limit: Option<u64>,
) -> Result<(), SharedRouterError>
where
    W: AsyncWrite + Unpin,
{
    let mut limiter = payload_limit.map(crate::sync::ratelimit::RateLimiter::new);
    loop {
        let queued = tokio::select! {
            biased;
            _ = terminated(&mut terminal) => return Ok(()),
            queued = receiver.recv() => queued,
        };
        let Some(mut queued) = queued else {
            return Ok(());
        };
        let result = tokio::select! {
            biased;
            _ = terminated(&mut terminal) => return Ok(()),
            result = async {
                if let (Some(limiter), FrameKind::Data) = (limiter.as_mut(), queued.frame.kind()) {
                    let sleep = limiter.consume(queued.frame.payload().len() as u64);
                    if !sleep.is_zero() {
                        tokio::time::sleep(sleep).await;
                    }
                }
                write_frame(&mut writer, &queued.frame).await
                    .map_err(RouterError::from).map_err(Arc::new)?;
                // Process stdout is line buffered; every complete frame must
                // be flushed, not only Ack.
                writer.flush().await
                    .map_err(crate::protocol::ProtocolError::from)
                    .map_err(RouterError::from).map_err(Arc::new)
            } => result,
        };
        if let Err(error) = result {
            // Publish before dropping the Ack completion sender: even on
            // another worker, its waiter must see the authoritative cause.
            publish_terminal(inner, Terminal::Failed(Arc::clone(&error)));
            return Err(error);
        }
        if let Some(written) = queued.written.take() {
            let _ = written.send(());
        }
        // Cancellation may interrupt a partial frame or flush. We always drop
        // the transport on that path; it must never resume writing another frame.
    }
}

fn route_inbound(inner: &Arc<RouterInner>, routed: RoutedFrame) -> Result<(), RouterError> {
    let stream_id = routed.frame.stream_id();
    let mut streams = inner
        .streams
        .lock()
        .map_err(|_| RouterError::StatePoisoned)?;
    if inner.terminal.borrow().is_some() {
        return Err(RouterError::ShuttingDown);
    }
    if let Some(sender) = streams.get(&stream_id) {
        sender
            .send(routed)
            .map_err(|_| RouterError::StreamClosed(stream_id.get()))
    } else {
        if !inner.role.peer_owns(stream_id) {
            return Err(RouterError::InvalidPeerStreamId {
                role: inner.role,
                stream_id: stream_id.get(),
            });
        }
        if !is_stream_opening_kind(routed.frame.kind()) {
            return Err(RouterError::InvalidStreamOpen {
                stream_id: stream_id.get(),
                kind: routed.frame.kind(),
            });
        }
        ensure_stream_capacity(inner, &streams)?;
        let (sender, receiver) = mpsc::unbounded_channel();
        streams.insert(stream_id, sender);
        let inbox = StreamInbox {
            stream_id,
            receiver,
            terminal: inner.terminal.subscribe(),
            inner: Arc::downgrade(inner),
        };
        // Publication is serialized with termination. Never drop an inbox while
        // holding the registry lock: its Drop removes its registration.
        let result = inner.incoming_tx.send(IncomingStream {
            first: routed,
            inbox,
        });
        drop(streams);
        result.map_err(|_| RouterError::IncomingClosed)
    }
}

fn is_stream_opening_kind(kind: FrameKind) -> bool {
    matches!(
        kind,
        FrameKind::ScanRequest
            | FrameKind::HashRequest
            | FrameKind::FileFetchRequest
            | FrameKind::SignatureRequest
            | FrameKind::FileBegin
            | FrameKind::Metadata
            | FrameKind::Mutation
            | FrameKind::XattrRequest
            | FrameKind::AclRequest
            | FrameKind::BsdFlagsRequest
    )
}

fn register_stream(
    inner: &Arc<RouterInner>,
    stream_id: StreamId,
) -> Result<StreamInbox, SharedRouterError> {
    let mut streams = inner
        .streams
        .lock()
        .map_err(|_| Arc::new(RouterError::StatePoisoned))?;
    if let Some(state) = inner.terminal.borrow().as_ref() {
        return Err(state.error());
    }
    if streams.contains_key(&stream_id) {
        return Err(Arc::new(RouterError::StreamAlreadyRegistered(
            stream_id.get(),
        )));
    }
    ensure_stream_capacity(inner, &streams).map_err(Arc::new)?;
    let (sender, receiver) = mpsc::unbounded_channel();
    streams.insert(stream_id, sender);
    Ok(StreamInbox {
        stream_id,
        receiver,
        terminal: inner.terminal.subscribe(),
        inner: Arc::downgrade(inner),
    })
}

fn ensure_stream_capacity(
    inner: &RouterInner,
    streams: &HashMap<StreamId, mpsc::UnboundedSender<RoutedFrame>>,
) -> Result<(), RouterError> {
    if streams.len() >= inner.config.max_active_streams as usize {
        return Err(RouterError::TooManyStreams(inner.config.max_active_streams));
    }
    Ok(())
}

fn publish_terminal(inner: &Arc<RouterInner>, state: Terminal) {
    // Cut off new native publications before notifying terminal observers.
    // Admitted work remains owned by its worker and may finish after this point.
    inner.publication.close();
    let Ok(mut streams) = inner.streams.lock() else {
        // Poisoning is itself terminal; still wake waiters and close budgets.
        inner.terminal.send_if_modified(|terminal| {
            if terminal.is_some() {
                return false;
            }
            *terminal = Some(Terminal::Failed(Arc::new(RouterError::StatePoisoned)));
            true
        });
        inner.inbound_frames.close();
        inner.inbound_bytes.close();
        inner.outbound_frames.close();
        inner.outbound_bytes.close();
        return;
    };
    let changed = inner.terminal.send_if_modified(|terminal| {
        if terminal.is_some() {
            return false;
        }
        *terminal = Some(state);
        true
    });
    if changed {
        inner.inbound_frames.close();
        inner.inbound_bytes.close();
        inner.outbound_frames.close();
        inner.outbound_bytes.close();
        streams.clear();
    }
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

    #[tokio::test]
    async fn eof_closes_admission_even_with_queued_requests() {
        let (router_io, mut peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let mut router =
            FrameRouter::start(reader, writer, RouterRole::Server, RouterConfig::default())
                .unwrap();
        write_frame(
            &mut peer,
            &frame(FrameKind::ScanRequest, StreamId::new(1), b"request"),
        )
        .await
        .unwrap();
        peer.shutdown().await.unwrap();
        (&mut router.tasks.reader).await.unwrap().unwrap();
        assert!(
            router.sender().open_stream().is_err(),
            "EOF must close admission"
        );
        assert!(
            router.incoming().recv().await.unwrap().is_none(),
            "EOF must not admit queued requests"
        );
    }

    #[tokio::test]
    async fn eof_retains_final_responses_but_inbox_end_is_repeatable() {
        let (router_io, mut peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let mut router =
            FrameRouter::start(reader, writer, RouterRole::Client, RouterConfig::default())
                .unwrap();
        let mut inbox = router.sender().open_stream().unwrap();
        write_frame(&mut peer, &frame(FrameKind::Ack, inbox.stream_id(), b""))
            .await
            .unwrap();
        peer.shutdown().await.unwrap();
        (&mut router.tasks.reader).await.unwrap().unwrap();
        assert_eq!(
            inbox.recv().await.unwrap().unwrap().frame().kind(),
            FrameKind::Ack
        );
        for _ in 0..2 {
            assert!(inbox.recv().await.unwrap().is_none());
            assert!(router.incoming().recv().await.unwrap().is_none());
        }
        assert!(matches!(
            *router.sender().open_stream().err().unwrap(),
            RouterError::TransportEof
        ));
    }

    #[tokio::test]
    async fn failure_preempts_queued_frames_and_requests() {
        let (router_io, mut peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let mut router =
            FrameRouter::start(reader, writer, RouterRole::Server, RouterConfig::default())
                .unwrap();
        let mut inbox = router.sender().open_stream().unwrap();
        write_frame(
            &mut peer,
            &frame(FrameKind::Entry, inbox.stream_id(), b"queued"),
        )
        .await
        .unwrap();
        write_frame(
            &mut peer,
            &frame(FrameKind::ScanRequest, StreamId::new(1), b"queued"),
        )
        .await
        .unwrap();
        write_frame(
            &mut peer,
            &frame(FrameKind::Entry, StreamId::new(3), b"invalid"),
        )
        .await
        .unwrap();
        assert!((&mut router.tasks.reader).await.unwrap().is_err());
        assert!(
            inbox.recv().await.is_err(),
            "failure must preempt buffered work"
        );
        assert!(router.incoming().recv().await.is_err());
    }

    #[tokio::test]
    async fn timeout_releases_ack_waiter_during_stalled_write() {
        let (router_io, _peer) = tokio::io::duplex(1);
        let (reader, writer) = tokio::io::split(router_io);
        let router = FrameRouter::start(
            reader,
            writer,
            RouterRole::Client,
            RouterConfig {
                idle_timeout: Some(Duration::from_millis(50)),
                ..RouterConfig::default()
            },
        )
        .unwrap();
        let inbox = router.sender().open_stream().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            router
                .sender()
                .send(frame(FrameKind::Ack, inbox.stream_id(), b"ok")),
        )
        .await;
        assert!(result.expect("Ack waiter stuck after timeout").is_err());
    }

    #[tokio::test]
    async fn inbound_capacity_wait_is_covered_by_timeout() {
        let (router_io, mut peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let mut router = FrameRouter::start(
            reader,
            writer,
            RouterRole::Client,
            RouterConfig {
                max_inbound_frames: 1,
                idle_timeout: Some(Duration::from_millis(50)),
                ..RouterConfig::default()
            },
        )
        .unwrap();
        let inbox = router.sender().open_stream().unwrap();
        for _ in 0..2 {
            write_frame(
                &mut peer,
                &frame(FrameKind::Entry, inbox.stream_id(), b"held"),
            )
            .await
            .unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(1), router.incoming().recv()).await;
        assert!(
            matches!(result.expect("permit wait escaped timeout"), Err(error) if matches!(*error, RouterError::IdleTimeoutExceeded))
        );
    }

    #[tokio::test]
    async fn actor_abort_before_first_poll_terminates_surviving_handles() {
        let (router_io, _peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let mut router =
            FrameRouter::start(reader, writer, RouterRole::Client, RouterConfig::default())
                .unwrap();
        let sender = router.sender();
        let mut inbox = sender.open_stream().unwrap();
        router.tasks.writer.abort();
        assert!((&mut router.tasks.writer).await.unwrap_err().is_cancelled());
        let error = tokio::time::timeout(Duration::from_secs(1), inbox.recv())
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(*error, RouterError::ActorStopped("writer")));
        assert!(sender.open_stream().is_err());
    }

    #[tokio::test]
    async fn control_traffic_cannot_fill_the_inbound_budget() {
        let (router_io, mut peer) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let router = FrameRouter::start(
            reader,
            writer,
            RouterRole::Client,
            RouterConfig {
                max_inbound_frames: 1,
                ..RouterConfig::default()
            },
        )
        .unwrap();
        let mut inbox = router.sender().open_stream().unwrap();
        for _ in 0..4 {
            write_frame(
                &mut peer,
                &frame(FrameKind::Pong, StreamId::CONTROL, b"pong"),
            )
            .await
            .unwrap();
        }
        write_frame(
            &mut peer,
            &frame(FrameKind::Entry, inbox.stream_id(), b"entry"),
        )
        .await
        .unwrap();
        let entry = tokio::time::timeout(Duration::from_secs(1), inbox.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(entry.frame().payload(), &Bytes::from_static(b"entry"));
    }

    #[tokio::test]
    async fn shutdown_interrupts_payload_pacing_and_budget_waiters() {
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(router_io);
        let (mut peer_reader, _peer_writer) = tokio::io::split(peer_io);
        let mut router = FrameRouter::start(
            reader,
            writer,
            RouterRole::Client,
            RouterConfig {
                max_outbound_frames: 1,
                outbound_payload_limit: Some(1024),
                ..RouterConfig::default()
            },
        )
        .unwrap();
        let sender = router.sender();
        let inbox = sender.open_stream().unwrap();
        let data = frame(FrameKind::Data, inbox.stream_id(), &[0; 1024]);
        sender.send(data.clone()).await.unwrap();
        crate::protocol::read_frame(&mut peer_reader).await.unwrap();
        sender.send(data).await.unwrap();
        let ack_sender = sender.clone();
        let ack = tokio::spawn(async move {
            ack_sender
                .send(frame(FrameKind::Ack, inbox.stream_id(), b"ok"))
                .await
        });
        tokio::task::yield_now().await;
        sender.fail(Arc::new(RouterError::PeerError));
        tokio::time::timeout(Duration::from_millis(250), router.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            *ack.await.unwrap().unwrap_err(),
            RouterError::PeerError
        ));
        assert!(matches!(
            *sender.open_stream().err().unwrap(),
            RouterError::PeerError
        ));
    }

    struct StallingWriter {
        flush: bool,
        fail: bool,
        first_write: bool,
        reached: Option<oneshot::Sender<()>>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl AsyncWrite for StallingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.flush {
                return std::task::Poll::Ready(Ok(bytes.len()));
            }
            if self.first_write {
                self.first_write = false;
                return std::task::Poll::Ready(Ok(1));
            }
            if let Some(reached) = self.reached.take() {
                let _ = reached.send(());
            }
            if self.fail {
                std::task::Poll::Ready(Err(std::io::Error::other("injected write failure")))
            } else {
                std::task::Poll::Pending
            }
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(reached) = self.reached.take() {
                let _ = reached.send(());
            }
            if self.fail {
                std::task::Poll::Ready(Err(std::io::Error::other("injected flush failure")))
            } else {
                std::task::Poll::Pending
            }
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl Drop for StallingWriter {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn interrupted_or_failed_write_and_flush_drop_transport_and_wake_ack() {
        for (flush, fail) in [(false, false), (true, false), (false, true), (true, true)] {
            let (reached_tx, reached_rx) = oneshot::channel();
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (reader, _peer) = tokio::io::duplex(4096);
            let mut router = FrameRouter::start(
                reader,
                StallingWriter {
                    flush,
                    fail,
                    first_write: true,
                    reached: Some(reached_tx),
                    dropped: Arc::clone(&dropped),
                },
                RouterRole::Client,
                RouterConfig::default(),
            )
            .unwrap();
            let sender = router.sender();
            let inbox = sender.open_stream().unwrap();
            let ack = tokio::spawn(async move {
                sender
                    .send(frame(FrameKind::Ack, inbox.stream_id(), b"ok"))
                    .await
            });
            tokio::time::timeout(Duration::from_secs(1), reached_rx)
                .await
                .unwrap()
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(1), router.shutdown())
                .await
                .unwrap();
            assert_eq!(result.is_err(), fail);
            assert!(
                dropped.load(Ordering::SeqCst),
                "interrupted transport must be dropped"
            );
            let error = ack.await.unwrap().unwrap_err();
            if fail {
                assert!(matches!(
                    *error,
                    RouterError::Protocol(crate::protocol::ProtocolError::Io(_))
                ));
            } else {
                assert!(matches!(*error, RouterError::ShuttingDown));
            }
        }
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
        let outbound = crate::protocol::read_frame(&mut peer_reader).await.unwrap();
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
    async fn control_ping_is_answered_without_an_inbox() {
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (mut peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let _router = FrameRouter::start(
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
        let pong = tokio::time::timeout(
            Duration::from_secs(1),
            crate::protocol::read_frame(&mut peer_reader),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(pong.kind(), FrameKind::Pong);
        assert_eq!(pong.payload(), &Bytes::from_static(b"ping"));
    }

    /// --timeout: a session that receives no inbound frame within the
    /// configured window aborts with the typed idle error; stream waiters
    /// observe the failure rather than a silent clean end.
    #[tokio::test]
    async fn idle_timeout_aborts_silent_session() {
        let config = RouterConfig {
            idle_timeout: Some(Duration::from_millis(150)),
            ..RouterConfig::default()
        };
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (_peer_reader, _peer_writer) = tokio::io::split(peer_io);
        let router =
            FrameRouter::start(router_reader, router_writer, RouterRole::Client, config).unwrap();
        let mut inbox = router.sender().open_stream().unwrap();

        // The peer never writes; the deadline must fire and the inbox must
        // surface the typed timeout (not a clean `None`).
        let error = inbox.recv().await.unwrap_err();
        assert!(matches!(*error, RouterError::IdleTimeoutExceeded));
    }

    /// A session that keeps receiving frames resets the idle deadline each
    /// time and must survive windows far beyond the configured timeout.
    #[tokio::test]
    async fn steady_frames_never_trip_idle_deadline() {
        let config = RouterConfig {
            idle_timeout: Some(Duration::from_millis(200)),
            ..RouterConfig::default()
        };
        let (router_io, peer_io) = tokio::io::duplex(4096);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (_peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let router =
            FrameRouter::start(router_reader, router_writer, RouterRole::Client, config).unwrap();
        let mut inbox = router.sender().open_stream().unwrap();
        let stream_id = inbox.stream_id();

        // Frames arrive well inside each 200 ms window; five windows pass,
        // far beyond the timeout, without tripping it.
        for tick in 0..5 {
            write_frame(
                &mut peer_writer,
                &Frame::new(
                    FrameKind::Entry,
                    FrameFlags::empty(),
                    stream_id,
                    Bytes::from(format!("tick {tick}")),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let routed = tokio::time::timeout(Duration::from_millis(500), inbox.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                routed.frame().payload(),
                &Bytes::from(format!("tick {tick}"))
            );
        }

        // Session is still healthy: a final frame routes normally.
        write_frame(
            &mut peer_writer,
            &frame(FrameKind::Entry, stream_id, b"final"),
        )
        .await
        .unwrap();
        let routed = tokio::time::timeout(Duration::from_millis(500), inbox.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(routed.frame().payload(), &Bytes::from_static(b"final"));
    }

    /// --bwlimit paces outbound `Data` payload bytes across all streams while
    /// control frames bypass the limiter entirely.
    #[tokio::test]
    async fn data_frames_are_paced_by_payload_limit() {
        let config = RouterConfig {
            // 1 KiB/s with a 1 KiB burst: the first frame is free, the second
            // identical frame must wait a full second.
            outbound_payload_limit: Some(1024),
            ..RouterConfig::default()
        };
        let (router_io, peer_io) = tokio::io::duplex(64 * 1024);
        let (router_reader, router_writer) = tokio::io::split(router_io);
        let (mut peer_reader, _peer_writer) = tokio::io::split(peer_io);
        let router =
            FrameRouter::start(router_reader, router_writer, RouterRole::Client, config).unwrap();
        let inbox = router.sender().open_stream().unwrap();
        let stream_id = inbox.stream_id();

        let data = frame(FrameKind::Data, stream_id, &[0_u8; 1024]);
        let start = std::time::Instant::now();
        router.sender().send(data.clone()).await.unwrap();
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            crate::protocol::read_frame(&mut peer_reader),
        )
        .await
        .expect("first data frame must pass within the burst")
        .unwrap();
        assert_eq!(first.kind(), FrameKind::Data);

        // Second 1 KiB frame: burst budget spent, ~1 s token deficit.
        router.sender().send(data).await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            crate::protocol::read_frame(&mut peer_reader),
        )
        .await
        .expect("second data frame must still arrive")
        .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "paced frame arrived too early: {elapsed:?}"
        );

        // Once the paced data is written, Ack incurs no additional token
        // delay even though the bucket is exhausted.
        let ack = frame(FrameKind::Ack, stream_id, b"ok");
        router.sender().send(ack).await.unwrap();
        let control = tokio::time::timeout(
            Duration::from_millis(250),
            crate::protocol::read_frame(&mut peer_reader),
        )
        .await
        .expect("control frames must bypass the payload limiter")
        .unwrap();
        assert_eq!(control.kind(), FrameKind::Ack);
    }
}
