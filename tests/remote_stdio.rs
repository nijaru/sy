//! Regression tests for the v3 stdio transport boundary.
//!
//! In-memory router/protocol tests use `tokio::io::duplex`, an unbuffered
//! pipe. Real `sy __serve` sessions instead write to process stdio, where
//! `tokio::io::stdout()` wraps `std::io::Stdout` — a LineWriter that holds
//! newlineless binary frames indefinitely. The router must flush every frame
//! it reports as written, or a scan response (whose Entry payloads contain
//! newlines only by hash coincidence) stalls mid-frame and the session
//! deadlocks waiting for EntryEnd/Ack.

use std::process::Stdio;
use std::time::Duration;

use futures::StreamExt;
use tokio::process::Command;

use sy::engine::scan::ScanRequest;
use sy::protocol::Operation;
use sy::remote::runtime::ClientRemoteSession;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

/// One scan over a spawned `sy __serve` child's real stdio must terminate.
///
/// The entry and directory names deliberately contain no `\n` bytes, and
/// EntryEnd carries none at all, so without a per-frame flush the scan
/// stream can never reach its end: LineWriter only releases bytes up to a
/// newline, and the 1 KiB buffer never overflows with this payload. The
/// timeout turns that hang into a clear regression failure.
#[tokio::test]
async fn stdio_scan_completes_over_line_buffered_pipe() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::write(root.path().join("alpha"), b"alpha content").unwrap();
    std::fs::write(root.path().join("nested/beta"), b"beta content").unwrap();

    let mut child = Command::new(sy_bin())
        .arg("__serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();

    let session = ClientRemoteSession::connect(
        child_stdout,
        child_stdin,
        Operation::Push,
        root.path(),
        Default::default(),
    )
    .await
    .unwrap();

    let entries = session.scan(ScanRequest::default()).await.unwrap();

    let mut paths = Vec::new();
    let mut entries = Box::pin(entries);
    while let Some(entry) = tokio::time::timeout(Duration::from_secs(30), entries.next())
        .await
        .expect("scan stream stalled: a newlineless frame was not flushed")
    {
        paths.push(
            entry
                .expect("scan stream failed")
                .path
                .as_path()
                .to_path_buf(),
        );
    }
    paths.sort();

    assert_eq!(
        paths,
        vec![
            std::path::PathBuf::from("alpha"),
            std::path::PathBuf::from("nested"),
            std::path::PathBuf::from("nested/beta"),
        ]
    );

    // Dropping the session drops the router and its writer task, closing the
    // child's stdin pipe; the agent must observe EOF and terminate promptly.
    // Its exit status is not asserted: on a clean disconnect the reader
    // currently reports early-eof as an I/O error and exits 1, a separate
    // defect from the flush regression under test.
    drop(session);
    tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("agent did not exit after stdin EOF")
        .unwrap();
}
