//! Replacement backups through the real v3 client and `sy __serve` fetch path.
#![cfg(unix)]

use futures::StreamExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use sy::engine::domain::{Entry, EntryKind, SyncOp};
use sy::engine::planner::ExecutionPolicy;
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::remote::pull::{RemotePullError, RemotePullExecutor};
use sy::remote::pull_lower::lower_pull_op;
use sy::remote::runtime::ClientRemoteSession;
use sy::rooted_fs::RootedFs;
use tokio::process::{Child, Command};

const ORIGINAL: &[u8] = b"precious original destination";
const REPLACEMENT: &[u8] = b"new remote file contents";
const FILE: &str = "nested/file";

struct Pull {
    source_root: tempfile::TempDir,
    destination_root: tempfile::TempDir,
    source: Entry,
    destination: Entry,
    client: ClientRemoteSession,
    child: Child,
}

impl Pull {
    async fn new() -> Self {
        let source_root = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        for root in [&source_root, &destination_root] {
            std::fs::create_dir(root.path().join("nested")).unwrap();
        }
        std::fs::write(source_root.path().join(FILE), REPLACEMENT).unwrap();
        std::fs::write(destination_root.path().join(FILE), ORIGINAL).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg("__serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = ClientRemoteSession::connect(
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
            sy::protocol::Operation::Pull,
            source_root.path(),
            Default::default(),
        )
        .await
        .unwrap();
        let mut request = ScanRequest::default();
        request.metadata.unix_mode = true;
        let mut source_entries = client.scan(request).await.unwrap();
        let mut source = None;
        while let Some(entry) = source_entries.next().await {
            let entry = entry.unwrap();
            if entry.is_file() {
                source = Some(entry);
            }
        }
        let mut destination_entries = sy::endpoint::local_entry_scan::local_entry_stream(
            destination_root.path().to_path_buf(),
            request,
        );
        let mut destination = None;
        while let Some(entry) = destination_entries.next().await {
            let entry = entry.unwrap();
            if entry.is_file() {
                destination = Some(entry);
            }
        }
        Self {
            source_root,
            destination_root,
            source: source.unwrap(),
            destination: destination.unwrap(),
            client,
            child,
        }
    }

    async fn replace(
        &self,
        backup_dir: Option<PathBuf>,
        suffix: &str,
    ) -> Result<(), RemotePullError> {
        let executor = RemotePullExecutor::new(
            self.destination_root.path().to_path_buf(),
            self.client.request_handle(),
            self.client.sender(),
            Scheduler::new(ResourceBudget::default()).unwrap(),
        )
        .with_backup_enabled(true)
        .with_backup(backup_dir)
        .with_backup_suffix(suffix.into());
        let work = lower_pull_op(
            SyncOp::Update {
                source: self.source.clone(),
                destination: self.destination.clone(),
            },
            ExecutionPolicy::default(),
        )
        .unwrap()
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), executor.execute(work))
            .await
            .expect("pull replacement stalled")
            .map(|_| ())
    }

    async fn assert_original_intact(&self) {
        assert_eq!(
            std::fs::read(self.destination_root.path().join(FILE)).unwrap(),
            ORIGINAL
        );
        let rooted = RootedFs::open(self.destination_root.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(
            rooted
                .path_identity_blocking(&self.destination.path)
                .unwrap(),
            Some((EntryKind::File, self.destination.identity.unwrap()))
        );
    }

    async fn finish(self, success: bool) {
        drop(self.client);
        let mut child = self.child;
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("remote agent did not exit")
            .unwrap();
        assert_eq!(status.success(), success, "agent exit: {status}");
    }
}

#[tokio::test]
async fn pull_replacement_backup_commits_valid_fetch_with_suffix_and_backup_dir() {
    // Beside-file default, custom suffix/tree-shaped external directory, and
    // an empty suffix with a distinct backup directory all retain their policy.
    for (external, suffix) in [(false, "~"), (true, ".old"), (true, "")] {
        let pull = Pull::new().await;
        let backup_root = tempfile::tempdir().unwrap();
        let backup_dir = external.then(|| backup_root.path().join("backups"));
        let backup = backup_dir
            .as_deref()
            .unwrap_or(pull.destination_root.path())
            .join(format!("{FILE}{suffix}"));
        pull.replace(backup_dir, suffix).await.unwrap();
        assert_eq!(
            std::fs::read(pull.destination_root.path().join(FILE)).unwrap(),
            REPLACEMENT
        );
        assert_eq!(std::fs::read(backup).unwrap(), ORIGINAL);
        assert_eq!(
            std::fs::read(pull.source_root.path().join(FILE)).unwrap(),
            REPLACEMENT
        );
        pull.finish(true).await;
    }
}

#[tokio::test]
async fn pull_fetch_failure_after_backup_keeps_original_destination() {
    let pull = Pull::new().await;
    // A real source-side open failure after the completed source scan, not a
    // mocked fetch result. The client creates its backup before requesting bytes.
    std::fs::remove_file(pull.source_root.path().join(FILE)).unwrap();
    assert!(matches!(
        pull.replace(None, "~").await,
        Err(RemotePullError::Transfer(_))
    ));
    assert_eq!(
        std::fs::read(pull.destination_root.path().join("nested/file~")).unwrap(),
        ORIGINAL
    );
    pull.assert_original_intact().await;
    assert_eq!(
        std::fs::read_dir(pull.destination_root.path().join("nested"))
            .unwrap()
            .count(),
        2,
        "aborted fetch must not leave staging behind"
    );
    pull.finish(false).await;
}

#[tokio::test]
async fn pull_raced_destination_refuses_to_replace_existing_backup() {
    for symlink in [false, true] {
        let pull = Pull::new().await;
        let file = pull.destination_root.path().join(FILE);
        let backup = pull.destination_root.path().join("nested/file~");
        std::fs::write(&backup, b"previous backup").unwrap();
        // Keep the original inode alive so the replacement cannot reuse it.
        std::fs::rename(&file, file.with_extension("held")).unwrap();
        if symlink {
            std::os::unix::fs::symlink(pull.source_root.path().join(FILE), &file).unwrap();
        } else {
            std::fs::write(&file, b"concurrent replacement").unwrap();
        }
        assert!(matches!(
            pull.replace(None, "~").await,
            Err(RemotePullError::DeleteBackup(_))
        ));
        assert_eq!(std::fs::read(&backup).unwrap(), b"previous backup");
        if symlink {
            assert_eq!(
                std::fs::read_link(&file).unwrap(),
                pull.source_root.path().join(FILE)
            );
        } else {
            assert_eq!(std::fs::read(&file).unwrap(), b"concurrent replacement");
        }
        pull.finish(true).await;
    }
}

#[tokio::test]
async fn pull_backup_publication_failure_aborts_replacement() {
    let pull = Pull::new().await;
    let backup = pull.destination_root.path().join("nested/file~");
    std::fs::create_dir(&backup).unwrap();
    std::fs::write(backup.join("child"), b"existing backup child").unwrap();
    assert!(matches!(
        pull.replace(None, "~").await,
        Err(RemotePullError::DeleteBackup(_))
    ));
    pull.assert_original_intact().await;
    assert_eq!(
        std::fs::read(backup.join("child")).unwrap(),
        b"existing backup child"
    );
    pull.finish(true).await;
}
