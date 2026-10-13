use crate::remote::fetch::{decode_incoming_file_fetch, serve_decoded_file_fetch};
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
    namespace: Option<crate::remote::namespace::NamespaceHandler>,
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
    source_ignore: crate::remote::source_ignore::SourceIgnoreHandler,
    fetch: ServerFetchHandler,
    resolved: crate::protocol::ResolvedOperand,
    group_slot: std::sync::Arc<tokio::sync::Semaphore>,
}

/// Pull-session whole/delta source producer. The compression policy mirrors
/// the push direction: the client sets `-z`/`--compress`, and per-chunk
/// zstd applies only when both sides negotiated the ZSTD capability.
#[derive(Clone)]
pub struct ServerFetchHandler {
    rooted: crate::rooted_fs::RootedFs,
    sender: crate::remote::router::RouterSender,
    peer: crate::protocol::PlatformOs,
    // Shared across all fetch handlers, independently of the peer's stream
    // count. Each resident signature index and native matcher consumes budget.
    scheduler: crate::engine::scheduler::Scheduler,
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
                    Ok(Some(IncomingRequest::NamespaceBegin(incoming))) => {
                        let begun = async {
                            let (request, inbox) = crate::remote::namespace::decode_request(incoming)
                                .map_err(|error| ServeError::Request(error.to_string()))?;
                            if request != crate::protocol::WireNamespaceRequest::Begin {
                                return Err(ServeError::Request("invalid namespace begin phase".into()));
                            }
                            // The opener no longer owns shared frame permits.
                            // Earlier requests keep their owners through this drain.
                            while !tasks.is_empty() { check_joined(tasks.join_next().await)?; }
                            session.begin_namespace(inbox).await?;
                            handlers = RequestHandlers::new(session)?;
                            Ok(())
                        }.await;
                        begun
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
        scheduler: crate::engine::scheduler::Scheduler::new(Default::default())
            .map_err(|error| ServeError::Request(error.to_string()))?,
    })
}

impl RequestHandlers {
    fn new(session: &ServerRemoteSession) -> Result<Self> {
        Ok(Self {
            scan: session.scan_handler(),
            namespace: session.namespace_handler(),
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
                    source_ignore: session.source_ignore_handler()?,
                    fetch: fetch_handler(session)?,
                    resolved: session.ready().resolved.clone(),
                    group_slot: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
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
        IncomingRequest::NamespaceBegin(_) => {
            return Err(ServeError::Request(
                "namespace begin bypassed barrier".into(),
            ))
        }
        IncomingRequest::Namespace(incoming) => {
            let handler = handlers.namespace.clone().ok_or_else(|| {
                ServeError::Request("namespace qualification was not begun".into())
            })?;
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
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
        IncomingRequest::SourceIgnore(incoming) => {
            let handler = handlers.native()?.source_ignore.clone();
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
                let (decoded, mut inbox) = decode_incoming_file_fetch(incoming, handler.peer);
                let request = match decoded {
                    Ok(request) => request,
                    Err(error) => {
                        // Keep invalid-request root release on the same joined
                        // blocking path as the unscheduled fetch entry point.
                        return serve_decoded_file_fetch(
                            handler.rooted.clone(),
                            Err(error),
                            inbox,
                            &handler.sender,
                        )
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string());
                    }
                };
                let is_delta = request.is_delta();
                let resources = crate::engine::scheduler::ResourceRequest {
                    active_files: 1,
                    buffered_bytes: if is_delta {
                        crate::remote::delta_pull::SERVER_WORKING_SET
                    } else {
                        crate::remote::pull::REMOTE_FETCH_WORKING_SET
                    },
                    metadata_ops: 1,
                    cpu_tasks: 1,
                    network_writes: 1,
                };
                let _permit = tokio::select! {
                    biased;
                    error = handler.sender.closed() => return Err(error.to_string()),
                    early = inbox.recv(), if is_delta => {
                        return Err(match early {
                            Ok(Some(frame)) => format!("delta PULL frame {:?} before readiness", frame.frame().kind()),
                            Ok(None) => "delta PULL ended before readiness".into(),
                            Err(error) => error.to_string(),
                        });
                    }
                    permit = handler.scheduler.acquire(resources) => permit.map_err(|error| error.to_string())?,
                };
                serve_decoded_file_fetch(
                    handler.rooted.clone(),
                    Ok(request),
                    inbox,
                    &handler.sender,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Group(incoming) => {
            let role = crate::protocol::WireGroupOpen::decode(incoming.first.frame().payload())
                .map_err(|error| ServeError::Request(error.to_string()))?
                .role;
            let native = handlers.native()?;
            let handler = native.fetch.clone();
            let resolved = native.resolved.clone();
            let group_slot = native.group_slot.clone();
            tasks.spawn(async move {
                // A terminal reply may reach the peer before this handler's
                // joined cleanup releases admission. Queue the next bounded
                // request instead of turning that scheduling gap into failure.
                let _permit = group_slot
                    .acquire_owned()
                    .await
                    .map_err(|error| error.to_string())?;
                let result = match role {
                    crate::protocol::WireGroupRole::Source => {
                        crate::remote::group::serve_source(
                            handler.rooted.clone(),
                            incoming,
                            &handler.sender,
                            handler.peer,
                            resolved,
                        )
                        .await
                    }
                    crate::protocol::WireGroupRole::Destination => {
                        crate::remote::group::serve_destination(
                            handler.rooted.clone(),
                            incoming,
                            &handler.sender,
                            handler.peer,
                            resolved,
                        )
                        .await
                    }
                };
                result.map_err(|error| error.to_string())
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

    #[cfg(unix)]
    #[tokio::test]
    async fn next_group_waits_for_prior_admission_retirement() {
        use crate::protocol::{
            WireEntryKind, WireFetchCompression, WireFileBasis, WireFileBegin, WireGroupMember,
            WireGroupOpen, WireGroupResult, WireGroupRole, WireGroupSeal, WireGroupStep,
        };
        use futures::TryStreamExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"source").unwrap();
        let source = crate::endpoint::source_root::SourceRoot::open(root.path().into())
            .await
            .unwrap()
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let (client, server) = tokio::join!(
            ClientRemoteSession::connect(
                cr,
                cw,
                Operation::Pull,
                root.path(),
                RouterConfig::default()
            ),
            ServerRemoteSession::accept(sr, sw, RouterConfig::default()),
        );
        let client = client.unwrap();
        let mut server = server.unwrap();
        crate::rooted_fs::test_source::scan(&server.scan_handler_rooted().unwrap()).await;
        let handlers = RequestHandlers::new(&server).unwrap();
        let previous = handlers
            .native()
            .unwrap()
            .group_slot
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let sender = client.sender();
        let mut inbox = sender.open_stream().unwrap();
        let stream = inbox.stream_id();
        sender
            .send(
                Frame::new(
                    FrameKind::GroupOpen,
                    FrameFlags::empty(),
                    stream,
                    WireGroupOpen {
                        role: WireGroupRole::Source,
                        source_bytes: false,
                        retain_metadata: false,
                        xattrs: false,
                        acls: false,
                        source_flags: false,
                        verify_bytes: false,
                        compression: WireFetchCompression::None,
                        file: WireFileBegin::delta(
                            crate::remote::path::encode_relative_path(source.path.as_path())
                                .unwrap(),
                            source.size,
                            WireFileBasis::new(source.size, *source.identity.unwrap().as_bytes()),
                        ),
                    }
                    .encode(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let member = WireGroupMember {
            ordinal: 0,
            path: crate::remote::path::encode_relative_path(source.path.as_path()).unwrap(),
            observation: Some((WireEntryKind::File, *source.identity.unwrap().as_bytes())),
        };
        let payload = member.encode();
        let mut digest = blake3::Hasher::new();
        digest.update(&(payload.len() as u32).to_be_bytes());
        digest.update(&payload);
        let seal = WireGroupSeal {
            count: 1,
            digest: *digest.finalize().as_bytes(),
        };
        for (kind, payload) in [
            (FrameKind::GroupMember, payload),
            (FrameKind::GroupMembersEnd, seal.encode()),
        ] {
            sender
                .send(Frame::new(kind, FrameFlags::empty(), stream, payload).unwrap())
                .await
                .unwrap();
        }
        let request = server.next_request().await.unwrap().unwrap();
        let mut tasks = JoinSet::new();
        let spawned = spawn_request(&mut tasks, &handlers, request);
        // Inspect the blocked next opener while the previous handler still
        // owns its cleanup permit, then release before any assertion.
        let early = if spawned.is_ok() {
            tokio::time::timeout(Duration::from_millis(20), tasks.join_next()).await
        } else {
            Ok(None)
        };
        drop(previous);
        spawned.unwrap();
        assert!(
            early.is_err(),
            "next group must queue, not terminate the session"
        );
        let declaration = inbox.recv().await.unwrap().unwrap();
        assert_eq!(
            WireGroupResult::decode(declaration.frame().payload()).unwrap(),
            WireGroupResult::Declarations(seal)
        );
        drop(declaration);
        for (step, reply) in [
            (
                WireGroupStep::Prepare,
                WireGroupResult::SourceReady { flags: None },
            ),
            (WireGroupStep::Abort, WireGroupResult::Aborted),
        ] {
            sender
                .send(
                    Frame::new(
                        FrameKind::GroupStep,
                        FrameFlags::empty(),
                        stream,
                        step.encode(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            let received = inbox.recv().await.unwrap().unwrap();
            assert_eq!(
                WireGroupResult::decode(received.frame().payload()).unwrap(),
                reply
            );
        }
        tokio::time::timeout(Duration::from_secs(5), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        drop(client);
        server.shutdown().await.unwrap();
    }

    /// Saturate the real router with valid, unadmitted openers after two
    /// streams have Ready. All 130 fit the 256-stream limit, but the 128 queued
    /// openers used to retain every inbound permit and block those uploads.
    #[cfg(unix)]
    #[tokio::test]
    async fn valid_queued_fetch_openers_release_inbound_admission_before_budget_wait() {
        use crate::protocol::{
            SignatureBlockSize, WireDeltaFetchRequest, WireFetchCompression, WireFileEnd,
            WireFileFetchRequest, WireSignature, WireSignatureEnd,
        };
        use futures::TryStreamExt;

        let root = tempfile::tempdir().unwrap();
        let data = b"original SOURCE";
        std::fs::write(root.path().join("file"), data).unwrap();
        let config = RouterConfig::default();
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let (client, server) = tokio::join!(
            ClientRemoteSession::connect(cr, cw, Operation::Pull, root.path(), config),
            ServerRemoteSession::accept(sr, sw, config),
        );
        let client = client.unwrap();
        let mut server = server.unwrap();
        // Establish originals with the serving session's scanner, not an
        // independently opened root or peer-supplied identity authority.
        let rooted = server.scan_handler_rooted().unwrap();
        let source = rooted
            .source_entry_stream(crate::engine::scan::ScanRequest {
                metadata: crate::engine::scan::EntryMetadataRequest {
                    identity: true,
                    ..Default::default()
                },
                respect_gitignore: false,
                ..Default::default()
            })
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        let request = WireDeltaFetchRequest {
            source: WireFileFetchRequest::new(
                crate::remote::path::encode_relative_path(source.path.as_path()).unwrap(),
                source.size,
                *source.identity.unwrap().as_bytes(),
                WireFetchCompression::None,
            ),
            block_size: SignatureBlockSize::new(4096).unwrap(),
            basis_size: 4096,
            basis_identity: [9; 32],
        };
        let handlers = RequestHandlers::new(&server).unwrap();
        let mut tasks = JoinSet::new();
        let mut streams = Vec::new();
        for index in 0..config.max_inbound_frames as usize + 2 {
            let mut inbox = client.sender().open_stream().unwrap();
            client
                .sender()
                .send(
                    Frame::new(
                        FrameKind::DeltaFetchRequest,
                        FrameFlags::empty(),
                        inbox.stream_id(),
                        request.encode(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            // Observing every opener through next_request makes saturation
            // deterministic: no sleep or assumed transport/task progress.
            let incoming = server.next_request().await.unwrap().unwrap();
            spawn_request(&mut tasks, &handlers, incoming).unwrap();
            if index < 2 {
                let ready = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(ready.frame().kind(), FrameKind::DeltaFetchReady);
                assert!(ready.frame().flags().is_empty() && ready.frame().payload().is_empty());
            }
            streams.push((inbox, index < 2));
        }
        let mut clients = JoinSet::new();
        for (mut inbox, ready) in streams {
            let sender = client.sender();
            clients.spawn(async move {
                let id = inbox.stream_id();
                if !ready {
                    let ready = inbox
                        .recv()
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "fetch ended before Ready".to_string())?;
                    assert_eq!(ready.frame().kind(), FrameKind::DeltaFetchReady);
                    assert!(ready.frame().flags().is_empty() && ready.frame().payload().is_empty());
                }
                for (kind, payload) in [
                    (
                        FrameKind::Signature,
                        WireSignature::new(0, 4096, 0, [0; 16]).unwrap().encode(),
                    ),
                    (
                        FrameKind::SignatureEnd,
                        WireSignatureEnd::new(4096, 1, [9; 32]).unwrap().encode(),
                    ),
                ] {
                    sender
                        .send(Frame::new(kind, FrameFlags::empty(), id, payload).unwrap())
                        .await
                        .map_err(|error| error.to_string())?;
                }
                let mut received = Vec::new();
                loop {
                    let routed = inbox
                        .recv()
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "fetch ended before FileEnd".to_string())?;
                    let frame = routed.frame();
                    match frame.kind() {
                        FrameKind::Data => {
                            assert!(frame.flags().is_empty());
                            received.extend_from_slice(frame.payload());
                        }
                        FrameKind::FileEnd => {
                            assert_eq!(frame.flags(), FrameFlags::FINAL | FrameFlags::ACK_REQUIRED);
                            let end = WireFileEnd::decode(frame.payload()).unwrap();
                            assert_eq!(end.file_size(), data.len() as u64);
                            assert_eq!(end.digest(), *blake3::hash(data).as_bytes());
                            assert_eq!(received, data);
                            drop(routed);
                            crate::remote::fetch::acknowledge_fetch(&sender, id)
                                .await
                                .map_err(|error| error.to_string())?;
                            return Ok::<_, String>(());
                        }
                        kind => panic!("unexpected fetch response: {kind:?}"),
                    }
                }
            });
        }
        let progress = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(result) = clients.join_next().await {
                result.unwrap().unwrap();
            }
            while let Some(result) = tasks.join_next().await {
                check_joined(Some(result)).unwrap();
            }
        })
        .await;
        if progress.is_err() {
            // Wake and join admitted handlers even when demonstrating the old
            // deadlock; never abandon SOURCE owners or transport actors.
            client
                .sender()
                .fail(Arc::new(crate::remote::router::RouterError::SessionFailed(
                    "fetch opener progress deadline".into(),
                )));
            server
                .sender()
                .fail(Arc::new(crate::remote::router::RouterError::SessionFailed(
                    "fetch opener progress deadline".into(),
                )));
            while let Some(result) = clients.join_next().await {
                assert!(result.unwrap().is_err());
            }
            while let Some(result) = tasks.join_next().await {
                assert!(result.unwrap().is_err());
            }
        }
        drop(client);
        server.shutdown().await.unwrap();
        assert!(
            progress.is_ok(),
            "valid queued openers blocked admitted uploads/ACKs"
        );
        assert_eq!(std::fs::read(root.path().join("file")).unwrap(), data);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

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
            // The actual session, not a separate local observer, owns originals.
            let scan_id = StreamId::new(3);
            write_frame(
                &mut writer,
                &Frame::new(
                    FrameKind::ScanRequest,
                    FrameFlags::empty(),
                    scan_id,
                    crate::protocol::WireScanRequest {
                        scope: crate::protocol::WireScanScope::Tree,
                        respect_gitignore: false,
                        include_git_dir: false,
                        follow_symlinks: false,
                        max_depth: None,
                        unix_mode: false,
                        symlink_target: true,
                        identity: true,
                        hardlink_group: false,
                    }
                    .encode(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            loop {
                let frame = read_frame(&mut reader).await.unwrap();
                if frame.kind() == FrameKind::EntryEnd {
                    break;
                }
                assert_eq!(frame.kind(), FrameKind::Entry);
            }
            write_frame(
                &mut writer,
                &Frame::new(
                    FrameKind::Ack,
                    FrameFlags::empty(),
                    scan_id,
                    bytes::Bytes::new(),
                )
                .unwrap(),
            )
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
            let (qualified, served) = tokio::join!(
                async {
                    let namespace = client
                        .request_handle()
                        .begin_namespace_qualification()
                        .await
                        .unwrap();
                    namespace
                        .qualify(crate::engine::namespace::NamespaceParent::Root)
                        .await
                        .unwrap();
                    namespace
                        .qualify(crate::engine::namespace::NamespaceParent::Directory(
                            crate::engine::domain::RelativePath::new("admitted").unwrap(),
                        ))
                        .await
                        .unwrap();
                    namespace.seal().await.unwrap();
                },
                async {
                    for _ in 0..4 {
                        match server.next_request().await.unwrap().unwrap() {
                            IncomingRequest::NamespaceBegin(incoming) => {
                                let (request, inbox) =
                                    crate::remote::namespace::decode_request(incoming).unwrap();
                                assert_eq!(request, crate::protocol::WireNamespaceRequest::Begin);
                                server.begin_namespace(inbox).await.unwrap();
                            }
                            IncomingRequest::Namespace(incoming) => server
                                .namespace_handler()
                                .unwrap()
                                .serve(incoming)
                                .await
                                .unwrap(),
                            _ => panic!("expected namespace preflight"),
                        }
                    }
                },
            );
            let ((), ()) = (qualified, served);
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
            let mut invalid_stream = None;
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
                    invalid_stream = Some(inbox.stream_id().get());
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
                    matches!(result, Err(ServeError::Session(RemoteSessionError::Router(error))) if matches!(*error, crate::remote::router::RouterError::InvalidStreamOpen { stream_id, kind: FrameKind::Entry } if Some(stream_id) == invalid_stream))
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
