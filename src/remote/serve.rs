use crate::remote::fetch::serve_incoming_file_fetch;
use crate::remote::router::RouterConfig;
use crate::remote::runtime::{
    IncomingRequest, RemoteSessionError, ServerAclHandler, ServerBsdFlagsHandler,
    ServerFileHandler, ServerHashHandler, ServerMetadataHandler, ServerMutationHandler,
    ServerRemoteSession, ServerScanHandler, ServerSignatureHandler, ServerXattrHandler,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinSet;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Session(#[from] RemoteSessionError),

    #[error("v3 request handler failed: {0}")]
    Request(String),

    #[error("v3 request task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, ServeError>;

type RequestResult = std::result::Result<(), String>;

#[derive(Clone)]
struct RequestHandlers {
    scan: ServerScanHandler,
    native: Option<NativeRequestHandlers>,
}

#[derive(Clone)]
struct NativeRequestHandlers {
    hash: ServerHashHandler,
    signatures: ServerSignatureHandler,
    file: ServerFileHandler,
    metadata: ServerMetadataHandler,
    mutation: ServerMutationHandler,
    xattr: ServerXattrHandler,
    acl: ServerAclHandler,
    bsd_flags: ServerBsdFlagsHandler,
    fetch: ServerFetchHandler,
}

/// Pull-session whole-file source producer. The compression policy mirrors
/// the push direction: the client sets `-z`/`--compress`, and per-chunk
/// zstd applies only when both sides negotiated the ZSTD capability.
#[derive(Clone)]
pub struct ServerFetchHandler {
    rooted: crate::rooted_fs::RootedFs,
    sender: crate::remote::router::RouterSender,
    peer: crate::protocol::PlatformOs,
}

/// Serve one negotiated v3 transport with bounded request-task ownership.
///
/// The frame router independently bounds active streams, queued frames, and
/// queued bytes. This loop additionally caps spawned handler tasks at the same
/// active-stream budget so a peer cannot turn accepted streams into an
/// unbounded task backlog above the router's memory limits.
pub async fn serve_transport<R, W>(reader: R, writer: W, config: RouterConfig) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let max_tasks = config.max_active_streams as usize;
    let mut session = ServerRemoteSession::accept(reader, writer, config).await?;
    let mut tasks = JoinSet::<RequestResult>::new();
    serve_requests(&mut session, &mut tasks, max_tasks).await
}

async fn serve_requests(
    session: &mut ServerRemoteSession,
    tasks: &mut JoinSet<RequestResult>,
    max_tasks: usize,
) -> Result<()> {
    let mut handlers = RequestHandlers::new(session)?;
    let mut accepting = true;
    let mut failure = None;

    while accepting || !tasks.is_empty() {
        let result = if !accepting || tasks.len() >= max_tasks {
            check_joined(tasks.join_next().await)
        } else {
            tokio::select! {
                biased;
                joined = tasks.join_next(), if !tasks.is_empty() => check_joined(joined),
                request = session.next_request() => match request {
                    Ok(Some(IncomingRequest::AcquireRoot(incoming))) => {
                        // Producers own their blocking work until drained. Never
                        // change root authority beneath active scans/handlers.
                        let drained = async {
                            while !tasks.is_empty() { check_joined(tasks.join_next().await)?; }
                            session.acquire_root(incoming).await?;
                            handlers = RequestHandlers::new(session)?;
                            Ok(())
                        }.await;
                        drained
                    }
                    Ok(Some(request)) => spawn_request(tasks, &handlers, request),
                    Ok(None) => {
                        accepting = false;
                        Ok(())
                    }
                    Err(error) => Err(ServeError::Session(error)),
                }
            }
        };
        if let Err(error) = result {
            // Stop admission and wake all transport/stream waiters before
            // draining. Never drop JoinSet on an error: handlers may own
            // non-abortable blocking filesystem work and private staging.
            accepting = false;
            if failure.is_none() {
                session.sender().fail(std::sync::Arc::new(
                    crate::remote::router::RouterError::SessionFailed(error.to_string()),
                ));
                failure = Some(error);
            }
        }
    }

    // Orderly EOF drains admitted output and checks both actor results. Error
    // cleanup instead interrupts I/O; neither path detaches transport actors.
    let transport = if failure.is_some() {
        session.shutdown().await
    } else {
        session.finish().await
    };
    if let Err(error) = transport {
        if failure.is_none() {
            failure = Some(ServeError::Session(error));
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Run the private v3 agent over stdin/stdout. No logging or user-facing output
/// is initialized here; stdout is reserved exclusively for protocol frames.
pub async fn run_stdio() -> Result<()> {
    serve_transport(
        tokio::io::stdin(),
        tokio::io::stdout(),
        RouterConfig::default(),
    )
    .await
}

/// The per-request compression bit (from the fetch request) selects zstd; the
/// ZSTD capability was negotiated at handshake, so a client requesting
/// compression with a peer lacking it is rejected loudly on the client side
/// before any request is sent.
fn fetch_handler(session: &ServerRemoteSession) -> Result<ServerFetchHandler> {
    Ok(ServerFetchHandler {
        rooted: session.scan_handler_rooted()?,
        sender: session.sender(),
        peer: session.client().platform.os,
    })
}

impl RequestHandlers {
    fn new(session: &ServerRemoteSession) -> Result<Self> {
        Ok(Self {
            scan: session.scan_handler(),
            native: if session.has_root() {
                Some(NativeRequestHandlers {
                    hash: session.hash_handler()?,
                    signatures: session.signature_handler()?,
                    file: session.file_handler()?,
                    metadata: session.metadata_handler()?,
                    mutation: session.mutation_handler()?,
                    xattr: session.xattr_handler()?,
                    acl: session.acl_handler()?,
                    bsd_flags: session.bsd_flags_handler()?,
                    fetch: fetch_handler(session)?,
                })
            } else {
                None
            },
        })
    }

    fn native(&self) -> Result<&NativeRequestHandlers> {
        self.native
            .as_ref()
            .ok_or_else(|| RemoteSessionError::PendingRoot.into())
    }
}

fn spawn_request(
    tasks: &mut JoinSet<RequestResult>,
    handlers: &RequestHandlers,
    request: IncomingRequest,
) -> Result<()> {
    match request {
        IncomingRequest::AcquireRoot(_) => {
            return Err(ServeError::Request("acquisition bypassed barrier".into()))
        }
        IncomingRequest::Scan(incoming) => {
            let handler = handlers.scan.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Hash(incoming) => {
            let handler = handlers.native()?.hash.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Signatures(incoming) => {
            let handler = handlers.native()?.signatures.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::File(incoming) => {
            let handler = handlers.native()?.file.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Metadata(incoming) => {
            let handler = handlers.native()?.metadata.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::FileFetch(incoming) => {
            let handler = handlers.native()?.fetch.clone();
            tasks.spawn(async move {
                serve_incoming_file_fetch(
                    handler.rooted.clone(),
                    incoming,
                    &handler.sender,
                    handler.peer,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Mutation(incoming) => {
            let handler = handlers.native()?.mutation.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Xattr(incoming) => {
            let handler = handlers.native()?.xattr.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Acl(incoming) => {
            let handler = handlers.native()?.acl.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::BsdFlags(incoming) => {
            let handler = handlers.native()?.bsd_flags.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
    }
    Ok(())
}

fn check_joined(
    joined: Option<std::result::Result<RequestResult, tokio::task::JoinError>>,
) -> Result<()> {
    let Some(joined) = joined else {
        return Ok(());
    };
    match joined? {
        Ok(()) => Ok(()),
        Err(error) => Err(ServeError::Request(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Frame, FrameFlags, FrameKind, Operation};
    use crate::remote::runtime::ClientRemoteSession;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A frame-boundary EOF is orderly only if admitted fetch work has its
    /// commit Ack. Missing or truncated acknowledgements must fail the agent.
    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_completion_requires_ack_before_eof() {
        use crate::protocol::{
            read_frame, write_frame, StreamId, WireFetchCompression, WireFileFetchRequest,
        };
        use futures::TryStreamExt;
        use tokio::io::AsyncWriteExt;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"validated source").unwrap();
        let source = crate::endpoint::source_root::SourceRoot::open(root.path().to_path_buf())
            .await
            .unwrap()
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        for ending in ["ack", "missing", "truncated"] {
            let (mut client_io, server_io) = tokio::io::duplex(4096);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let serving = tokio::spawn(serve_transport(
                server_reader,
                server_writer,
                RouterConfig::default(),
            ));
            let (mut reader, mut writer) = tokio::io::split(&mut client_io);
            crate::remote::client_handshake(&mut reader, &mut writer, Operation::Pull, root.path())
                .await
                .unwrap();
            let id = StreamId::new(1);
            let request = WireFileFetchRequest::new(
                crate::remote::path::encode_relative_path(source.path.as_path()).unwrap(),
                source.size,
                *source.identity.unwrap().as_bytes(),
                WireFetchCompression::None,
            );
            write_frame(
                &mut writer,
                &Frame::new(
                    FrameKind::FileFetchRequest,
                    FrameFlags::empty(),
                    id,
                    request.encode(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            loop {
                let frame = read_frame(&mut reader).await.unwrap();
                if frame.kind() == FrameKind::FileEnd {
                    break;
                }
                assert_eq!(frame.kind(), FrameKind::Data);
            }
            match ending {
                "ack" => write_frame(
                    &mut writer,
                    &Frame::new(FrameKind::Ack, FrameFlags::empty(), id, bytes::Bytes::new())
                        .unwrap(),
                )
                .await
                .unwrap(),
                "truncated" => writer.write_all(&[0]).await.unwrap(),
                "missing" => {}
                _ => unreachable!(),
            }
            writer.shutdown().await.unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), serving)
                .await
                .unwrap()
                .unwrap();
            match ending {
                "ack" => result.unwrap(),
                "missing" => assert!(
                    matches!(result, Err(ServeError::Request(_))),
                    "missing commit Ack must fail admitted handler work: {result:?}"
                ),
                "truncated" => assert!(result.is_err(), "truncated Ack cannot become graceful EOF"),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            std::fs::read(root.path().join("file")).unwrap(),
            b"validated source"
        );
        assert_eq!(
            crate::endpoint::source_root::SourceRoot::open(root.path().to_path_buf())
                .await
                .unwrap()
                .entries(Default::default())
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .remove(0)
                .identity,
            source.identity
        );
    }

    #[tokio::test]
    async fn terminal_paths_drain_admitted_handlers_and_blocking_work() {
        for cause in ["request", "task", "router", "eof"] {
            let root = tempfile::TempDir::new().unwrap();
            let (client_io, server_io) = tokio::io::duplex(4096);
            let (client_reader, client_writer) = tokio::io::split(client_io);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let (client, server) = tokio::join!(
                ClientRemoteSession::connect(
                    client_reader,
                    client_writer,
                    Operation::Push,
                    root.path(),
                    RouterConfig::default()
                ),
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default()),
            );
            let client = client.unwrap();
            let mut server = server.unwrap();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let rooted = server.scan_handler_rooted().unwrap();
            rooted.pause_mutation_at(
                0,
                crate::rooted_fs::PublicationPause {
                    point: crate::rooted_fs::PublicationPausePoint::AfterAdmission,
                    reached: reached_tx,
                    resume: release_rx,
                },
            );
            let finished = Arc::new(AtomicBool::new(false));
            let worker_finished = Arc::clone(&finished);
            let worker = tokio::task::spawn_blocking(move || {
                let result = rooted.create_directory_blocking(
                    &crate::engine::domain::RelativePath::new("admitted").unwrap(),
                );
                worker_finished.store(true, Ordering::SeqCst);
                result.map(|_| ()).map_err(|error| error.to_string())
            });
            let paused = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
            let mut tasks = JoinSet::new();
            tasks.spawn(async move { worker.await.map_err(|error| error.to_string())? });
            match cause {
                "request" => {
                    tasks.spawn(async { Err("handler failed".to_string()) });
                }
                "task" => {
                    tasks.spawn(async {
                        panic!("handler panicked");
                    });
                }
                "router" => {
                    let inbox = client.sender().open_stream().unwrap();
                    client
                        .sender()
                        .send(
                            Frame::new(
                                FrameKind::Entry,
                                FrameFlags::empty(),
                                inbox.stream_id(),
                                bytes::Bytes::new(),
                            )
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                }
                "eof" => {}
                _ => unreachable!(),
            }
            if cause == "eof" {
                drop(client);
            }
            let mut serving =
                tokio::spawn(async move { serve_requests(&mut server, &mut tasks, 8).await });
            // Returning here would detach the blocking worker and abort its
            // handler, exactly the old error-path JoinSet-drop defect.
            let early = tokio::time::timeout(Duration::from_millis(30), &mut serving).await;
            let draining = early.is_err();
            // Release before assertions; the native gate also bounds failed-test
            // cleanup. Ordinary failure must drain this actual admitted mkdir.
            let _ = release_tx.send(());
            let result = match early {
                Ok(result) => result.unwrap(),
                Err(_) => tokio::time::timeout(Duration::from_secs(1), serving)
                    .await
                    .unwrap()
                    .unwrap(),
            };
            assert!(
                matches!(paused, Ok(Ok(()))),
                "{cause} missed admission gate"
            );
            assert!(draining, "{cause} returned before drainage");
            assert!(finished.load(Ordering::SeqCst));
            assert!(root.path().join("admitted").is_dir());
            match cause {
                "request" => assert!(
                    matches!(result, Err(ServeError::Request(error)) if error == "handler failed")
                ),
                "task" => assert!(matches!(result, Err(ServeError::Task(_)))),
                "router" => assert!(
                    matches!(result, Err(ServeError::Session(RemoteSessionError::Router(error))) if matches!(*error, crate::remote::router::RouterError::InvalidStreamOpen { stream_id: 1, kind: FrameKind::Entry }))
                ),
                "eof" => assert!(result.is_ok()),
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn dispatch_error_stops_admission_without_mutation() {
        let root = tempfile::TempDir::new().unwrap();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let serving = tokio::spawn(serve_transport(
            server_reader,
            server_writer,
            RouterConfig::default(),
        ));
        let client = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Pull,
            root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let mut inbox = client.sender().open_stream().unwrap();
        client
            .sender()
            .send(
                Frame::new(
                    FrameKind::FileBegin,
                    FrameFlags::empty(),
                    inbox.stream_id(),
                    bytes::Bytes::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), serving)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            Err(ServeError::Session(RemoteSessionError::OperationMismatch {
                operation: Operation::Pull,
                kind: FrameKind::FileBegin
            }))
        ));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert!(tokio::time::timeout(Duration::from_secs(1), inbox.recv())
            .await
            .unwrap()
            .unwrap()
            .is_none());
        assert!(client.sender().open_stream().is_err());
    }
}
