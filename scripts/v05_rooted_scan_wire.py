from pathlib import Path

rooted = Path("src/rooted_fs.rs")
text = rooted.read_text()
marker = "use crate::engine::domain::{EntryKind, RelativePath, Timestamp};\n"
if "#[cfg(unix)]\nmod scan;" not in text:
    if marker not in text:
        raise SystemExit("rooted_fs import marker missing")
    text = text.replace(marker, "#[cfg(unix)]\nmod scan;\n\n" + marker, 1)
    rooted.write_text(text)

scan = Path("src/remote/scan.rs")
text = scan.read_text()
imports = "use crate::remote::router::{IncomingStream, RouterSender, SharedRouterError, StreamInbox};\n"
if "use crate::rooted_fs::RootedFs;" not in text:
    if imports not in text:
        raise SystemExit("remote scan import marker missing")
    text = text.replace(imports, imports + "use crate::rooted_fs::RootedFs;\n", 1)
old = '''pub async fn serve_incoming_scan(
    root: &Path,
    incoming: IncomingStream,
    sender: &RouterSender,
) -> Result<()> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream_id = inbox.stream_id();
    let first_frame = first.frame();
    require_stream(first_frame, stream_id)?;
    let request = decode_scan_request(first_frame)?;
    drop(first);

    serve_scan(root, request, sender, stream_id).await?;
    receive_scan_ack(&mut inbox, stream_id).await
}

async fn serve_scan(
    root: &Path,
    request: ScanRequest,
    sender: &RouterSender,
    stream_id: StreamId,
) -> Result<()> {
    require_data_stream(stream_id)?;
    let mut entries =
        crate::endpoint::local_entry_scan::local_entry_stream(root.to_path_buf(), request);
'''
new = '''pub async fn serve_incoming_scan_rooted(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
) -> Result<()> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream_id = inbox.stream_id();
    let first_frame = first.frame();
    require_stream(first_frame, stream_id)?;
    let request = decode_scan_request(first_frame)?;
    drop(first);

    serve_scan(rooted, request, sender, stream_id).await?;
    receive_scan_ack(&mut inbox, stream_id).await
}

#[cfg(test)]
pub async fn serve_incoming_scan(
    root: &Path,
    incoming: IncomingStream,
    sender: &RouterSender,
) -> Result<()> {
    let rooted = RootedFs::open(root.to_path_buf())
        .await
        .map_err(|error| RemoteScanError::LocalScan(Box::new(error)))?;
    serve_incoming_scan_rooted(rooted, incoming, sender).await
}

async fn serve_scan(
    rooted: RootedFs,
    request: ScanRequest,
    sender: &RouterSender,
    stream_id: StreamId,
) -> Result<()> {
    require_data_stream(stream_id)?;
    let mut entries = rooted.entry_stream(request);
'''
if old not in text:
    raise SystemExit("remote scan serve block marker missing")
scan.write_text(text.replace(old, new, 1))

runtime = Path("src/remote/runtime.rs")
text = runtime.read_text()
old_import = "use crate::remote::scan::{request_scan, serve_incoming_scan};"
if old_import not in text:
    raise SystemExit("runtime scan import marker missing")
text = text.replace(old_import, "use crate::remote::scan::{request_scan, serve_incoming_scan_rooted};", 1)
old = '''#[derive(Clone)]
pub struct ServerScanHandler {
    root: PathBuf,
    sender: RouterSender,
}

impl ServerScanHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> crate::remote::scan::Result<()> {
        serve_incoming_scan(&self.root, incoming, &self.sender).await
    }
}
'''
new = '''#[derive(Clone)]
pub struct ServerScanHandler {
    rooted: RootedFs,
    sender: RouterSender,
}

impl ServerScanHandler {
    pub async fn serve(&self, incoming: IncomingStream) -> crate::remote::scan::Result<()> {
        serve_incoming_scan_rooted(self.rooted.clone(), incoming, &self.sender).await
    }
}
'''
if old not in text:
    raise SystemExit("runtime scan handler marker missing")
text = text.replace(old, new, 1)
old = '''        ServerScanHandler {
            root: self.opened.root.clone(),
            sender: self.router.sender(),
        }'''
new = '''        ServerScanHandler {
            rooted: self.opened.rooted.clone(),
            sender: self.router.sender(),
        }'''
if old not in text:
    raise SystemExit("runtime scan handler construction marker missing")
runtime.write_text(text.replace(old, new, 1))
