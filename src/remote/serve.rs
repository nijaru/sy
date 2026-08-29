use crate::remote::router::RouterConfig;
use crate::remote::runtime::{
    IncomingRequest, RemoteSessionError, ServerFileHandler, ServerMetadataHandler,
    ServerMutationHandler, ServerRemoteSession, ServerScanHandler, ServerSignatureHandler,
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
    signatures: ServerSignatureHandler,
    file: ServerFileHandler,
    metadata: ServerMetadataHandler,
    mutation: ServerMutationHandler,
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
    let handlers = RequestHandlers {
        scan: session.scan_handler(),
        signatures: session.signature_handler(),
        file: session.file_handler(),
        metadata: session.metadata_handler(),
        mutation: session.mutation_handler(),
    };
    let mut tasks = JoinSet::<RequestResult>::new();
    let mut accepting = true;

    while accepting || !tasks.is_empty() {
        if !accepting || tasks.len() >= max_tasks {
            join_one(&mut tasks).await?;
            continue;
        }

        if tasks.is_empty() {
            match session.next_request().await? {
                Some(request) => spawn_request(&mut tasks, &handlers, request),
                None => accepting = false,
            }
            continue;
        }

        tokio::select! {
            request = session.next_request() => {
                match request? {
                    Some(request) => spawn_request(&mut tasks, &handlers, request),
                    None => accepting = false,
                }
            }
            joined = tasks.join_next() => {
                check_joined(joined)?;
            }
        }
    }

    Ok(())
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

fn spawn_request(
    tasks: &mut JoinSet<RequestResult>,
    handlers: &RequestHandlers,
    request: IncomingRequest,
) {
    match request {
        IncomingRequest::Scan(incoming) => {
            let handler = handlers.scan.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Signatures(incoming) => {
            let handler = handlers.signatures.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::File(incoming) => {
            let handler = handlers.file.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Metadata(incoming) => {
            let handler = handlers.metadata.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        IncomingRequest::Mutation(incoming) => {
            let handler = handlers.mutation.clone();
            tasks.spawn(async move {
                handler
                    .serve(incoming)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
    }
}

async fn join_one(tasks: &mut JoinSet<RequestResult>) -> Result<()> {
    check_joined(tasks.join_next().await)
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
