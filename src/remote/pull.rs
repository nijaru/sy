//! v3 pull execution: remote source, local destination.
//!
//! The pull inverts the push executor's role split while keeping the same
//! safety ordering. The remote is a pure SOURCE: it serves scans, content
//! hashes, and whole-file fetches, and the session router already rejects
//! mutation requests in Pull sessions. The client owns reconciliation,
//! the bounded delete journal, local transactional staging, and metadata.
//!
//! Byte strategy for this slice is whole-file fetch with per-chunk
//! compression when negotiated. Delta pull (uploading local destination
//! signatures, receiving copy ops) is a follow-up: the wire vocabulary
//! supports it, but the safety-critical ordering — complete preflight, then
//! main work, then reverse-order deletes, then finalize — must land first.

use crate::endpoint::io::StagedWriter;
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::engine::compression::CompressionPolicy;
use crate::engine::domain::{Entry, EntryKind, RelativePath, Timestamp};
use crate::engine::scheduler::{ResourceRequest, Scheduler};
use crate::engine::work::WorkItem;
use crate::remote::fetch::fetch_file;
use crate::remote::router::RouterSender;
use crate::remote::runtime::{ClientRemoteHandle, RemoteSessionError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bounded working set per in-flight fetch; mirrors the push side so the
/// scheduler byte budget is direction-symmetric.
pub const REMOTE_FETCH_WORKING_SET: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RemotePullError {
    #[error(transparent)]
    Remote(#[from] RemoteSessionError),

    #[error(transparent)]
    Transfer(#[from] crate::remote::transfer::RemoteTransferError),

    #[error(transparent)]
    Scheduler(#[from] crate::engine::scheduler::SchedulerError),

    #[error("symlink action is missing the scanned target for {0}")]
    MissingSymlinkTarget(PathBuf),

    #[error("--backup location for {0} is not representable beneath the destination root")]
    InvalidBackupPath(PathBuf),

    #[error("local destination mutation failed for {0}: {1}")]
    LocalMutation(PathBuf, std::io::Error),

    #[error("regular-file pull requires Unix mode metadata for {0}")]
    MissingScannedMode(PathBuf),

    #[error("transactional type replacement is not implemented for directory transition at {0}")]
    TransactionalDirectoryReplace(PathBuf),
}

pub type Result<T> = std::result::Result<T, RemotePullError>;

#[derive(Debug, Clone)]
pub enum RemotePullAction {
    CreateDirectory {
        source: Entry,
    },
    FetchFile {
        source: Entry,
        destination: Option<Entry>,
        metadata: PullTransferMetadata,
    },
    ReplaceSymlink {
        source: Entry,
        modified: Option<Timestamp>,
    },
    ApplyMetadata {
        source: Entry,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PullTransferMetadata {
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
}

/// Executes lowered v3 pull work against the local destination. The caller
/// (the shared controller loop) owns tree ordering, deletion commit, and
/// finalize replay; this type owns per-item admission, fetch streaming,
/// staging, and commit.
pub struct RemotePullExecutor {
    destination_root: PathBuf,
    remote: ClientRemoteHandle,
    sender: RouterSender,
    scheduler: Scheduler,
    /// --backup: enabled flag, the destination-anchored backup directory
    /// (None = beside-the-file suffix backups), and the suffix.
    backup: bool,
    backup_dir: Option<std::path::PathBuf>,
    backup_suffix: String,
    reporter: Option<Arc<crate::sync::output::SyncReporter>>,
    rate_limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// -z/--compress: per-chunk zstd for fetches. The negotiated ZSTD
    /// capability is checked before the first request, mirroring the push
    /// side's transfer_file_with_policy contract.
    compression: Option<CompressionPolicy>,
}

impl RemotePullExecutor {
    pub fn new(
        destination_root: PathBuf,
        remote: ClientRemoteHandle,
        sender: RouterSender,
        scheduler: Scheduler,
    ) -> Self {
        Self {
            destination_root,
            remote,
            sender,
            scheduler,
            backup: false,
            backup_dir: None,
            backup_suffix: String::new(),
            reporter: None,
            rate_limiter: None,
            compression: None,
        }
    }

    pub fn with_backup(mut self, backup_dir: Option<std::path::PathBuf>) -> Self {
        self.backup_dir = backup_dir;
        self
    }

    /// The --backup suffix (default `~`); only applied when --backup is on.
    pub fn with_backup_suffix(mut self, suffix: String) -> Self {
        self.backup_suffix = suffix;
        self
    }

    /// Enable --backup explicitly (the suffix alone is not a marker: an
    /// empty --suffix value must not silently disable backup).
    pub fn with_backup_enabled(mut self, enabled: bool) -> Self {
        self.backup = enabled;
        self
    }

    pub fn with_reporter(
        mut self,
        reporter: Option<Arc<crate::sync::output::SyncReporter>>,
    ) -> Self {
        self.reporter = reporter;
        self
    }

    pub fn with_compression(mut self, policy: Option<CompressionPolicy>) -> Self {
        self.compression = policy;
        self
    }

    pub fn with_rate_limiter(
        mut self,
        limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    ) -> Self {
        self.rate_limiter = limiter;
        self
    }

    fn report(
        &self,
        op: crate::sync::output::ItemizeOp,
        kind: crate::sync::output::ItemizeKind,
        path: &RelativePath,
    ) {
        if let Some(reporter) = &self.reporter {
            reporter.operation(op, kind, path.as_path());
        }
    }

    fn dest_path(&self, relative: &RelativePath) -> PathBuf {
        self.destination_root.join(relative.as_path())
    }

    pub async fn execute(
        &self,
        item: WorkItem<RemotePullAction>,
    ) -> Result<Option<crate::remote::transfer::TransferSummary>> {
        let (action, resources) = item.into_parts();
        let _permit = self.scheduler.acquire(resources).await?;

        match action {
            RemotePullAction::CreateDirectory { source } => {
                let path = self.dest_path(&source.path);
                tokio::fs::create_dir_all(&path)
                    .await
                    .map_err(|error| RemotePullError::LocalMutation(path.clone(), error))?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Directory,
                    &source.path,
                );
                Ok(None)
            }
            RemotePullAction::FetchFile {
                source,
                destination,
                metadata,
            } => {
                let is_update = destination.is_some();
                // --backup preserves the replaced destination file first, as a
                // local copy (the destination is local in a pull). A backup
                // failure aborts the fetch so the user's copy cannot be
                // silently skipped.
                if let Some(existing) = &destination {
                    if existing.is_file() && self.backup_enabled() {
                        let backup_abs = self.backup_destination_for(&existing.path)?;
                        if let Some(parent) = backup_abs.parent() {
                            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                                RemotePullError::LocalMutation(backup_abs.clone(), error)
                            })?;
                        }
                        copy_local_backup(&self.destination_root, &existing.path, &backup_abs)
                            .await
                            .map_err(|error| {
                                RemotePullError::LocalMutation(backup_abs.clone(), error)
                            })?;
                    }
                }
                let summary = self.fetch_into_staging(&source, &metadata).await?;
                let op = if is_update {
                    crate::sync::output::ItemizeOp::Update
                } else {
                    crate::sync::output::ItemizeOp::Create
                };
                self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
                Ok(Some(summary))
            }
            RemotePullAction::ReplaceSymlink { source, modified } => {
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    RemotePullError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
                let dest = self.dest_path(&source.path);
                replace_local_symlink(target, &dest).await?;
                if let Some(modified) = modified {
                    set_local_mtime(&dest, modified).await?;
                }
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Symlink,
                    &source.path,
                );
                Ok(None)
            }
            RemotePullAction::ApplyMetadata {
                source,
                unix_mode,
                modified,
            } => {
                let dest = self.dest_path(&source.path);
                if let Some(mode) = unix_mode {
                    set_local_mode(&dest, mode).await?;
                }
                if let Some(modified) = modified {
                    if source.kind != EntryKind::Symlink {
                        set_local_mtime(&dest, modified).await?;
                    }
                }
                Ok(None)
            }
        }
    }

    /// Fetch one source file into endpoint staging and commit atomically.
    ///
    /// The staged bytes are BLAKE3-verified against the server-reported
    /// digest inside `fetch_file` before the Ack is sent; metadata is applied
    /// to the staged file so a failure never touches the visible destination.
    async fn fetch_into_staging(
        &self,
        source: &Entry,
        metadata: &PullTransferMetadata,
    ) -> Result<crate::remote::transfer::TransferSummary> {
        let dest = self.dest_path(&source.path);
        let endpoint = LocalEndpoint::new(self.destination_root.clone());
        let mut staged = endpoint
            .begin_write(source.path.as_path())
            .await
            .map_err(|error| {
                let message = error.to_string();
                RemotePullError::LocalMutation(dest.clone(), std::io::Error::other(message))
            })?;
        let summary = fetch_file(
            &self.sender,
            source,
            self.remote.peer_platform(),
            staged.as_mut(),
            self.compression,
            self.rate_limiter.as_ref(),
        )
        .await?;
        if self.compression.is_some()
            && !self
                .remote
                .ready()
                .capabilities
                .contains(crate::protocol::CapabilitySet::ZSTD)
        {
            return Err(RemotePullError::Remote(RemoteSessionError::PeerLacksZstd));
        }
        // Staging is private at 0600; commit applies the final metadata. A
        // missing scanned mode is an error, matching the push side's
        // create-mode contract.
        let mode = metadata.unix_mode.ok_or_else(|| {
            RemotePullError::LocalMutation(
                dest.clone(),
                std::io::Error::other("scanned source file is missing its Unix mode"),
            )
        })?;
        set_staged_metadata(staged.as_mut(), mode, metadata.modified).await?;
        staged.commit().await.map_err(|error| {
            let message = error.to_string();
            RemotePullError::LocalMutation(dest, std::io::Error::other(message))
        })?;
        Ok(summary)
    }

    /// --backup is on when a suffix policy exists. The suffix defaults to
    /// `~`; an empty string never disables --backup (config sets a marker
    /// only when --backup was passed).
    fn backup_enabled(&self) -> bool {
        self.backup
    }

    /// Backup location for one root-relative destination path, mirroring the
    /// local engine: beside the file (name + suffix) or under --backup-dir
    /// preserving the tree shape (GNU rsync semantics). The returned path
    /// is always absolute beneath the destination (or under the absolute
    /// --backup-dir), so callers can never write relative to the process
    /// working directory.
    fn backup_destination_for(&self, relative: &RelativePath) -> Result<PathBuf> {
        let file_name = relative
            .as_path()
            .file_name()
            .ok_or_else(|| RemotePullError::InvalidBackupPath(relative.as_path().to_path_buf()))?
            .to_string_lossy()
            .into_owned();
        let backup_name = format!("{file_name}{}", self.backup_suffix);
        match &self.backup_dir {
            Some(dir) => {
                let mut backup = dir.join(relative.as_path());
                backup.set_file_name(backup_name);
                Ok(backup)
            }
            None => {
                let mut backup = self.destination_root.join(relative.as_path());
                backup.set_file_name(backup_name);
                Ok(backup)
            }
        }
    }

    /// Delete one destination-only entry after the delete-threshold gate.
    /// Local deletes follow the same protection rules as the push side:
    /// `--backup` copies regular files before removal, symlinks remove
    /// without a backup so a target is never resolved.
    pub async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                network_writes: 0,
                ..ResourceRequest::default()
            })
            .await?;
        let path = self.dest_path(&action.path);
        if self.backup_enabled() && !action.is_directory {
            let backup_abs = self.backup_destination_for(&action.path)?;
            if let Some(parent) = backup_abs.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| RemotePullError::LocalMutation(backup_abs.clone(), error))?;
            }
            let copied = copy_local_backup(&self.destination_root, &action.path, &backup_abs).await;
            match copied {
                Ok(()) => {}
                // The entry may be a symlink: removal proceeds without a
                // backup so the target is never resolved.
                Err(error) => {
                    tracing::debug!(
                        path = %action.path.as_path().display(),
                        error = %error,
                        "delete backup copy unavailable; removing without backup"
                    );
                }
            }
        }
        remove_local_entry(&path, action.is_directory)
            .await
            .map_err(|error| RemotePullError::LocalMutation(path.clone(), error))?;
        self.report(
            crate::sync::output::ItemizeOp::Delete,
            if action.is_directory {
                crate::sync::output::ItemizeKind::Directory
            } else {
                crate::sync::output::ItemizeKind::File
            },
            &action.path,
        );
        Ok(())
    }

    /// Replay reverse-order finalize metadata (directory modes/mtimes,
    /// child-before-parent).
    pub async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                network_writes: 0,
                ..ResourceRequest::default()
            })
            .await?;
        let dest = self.dest_path(&metadata.path);
        if let Some(mode) = metadata.unix_mode {
            set_local_mode(&dest, mode).await?;
        }
        if let Some(modified) = metadata.modified {
            if metadata.kind != EntryKind::Symlink {
                set_local_mtime(&dest, modified).await?;
            }
        }
        Ok(())
    }
}

/// Copy a to-be-replaced or deleted local file to its --backup location.
/// The copy never follows symlinks: a backup must preserve the link
/// semantics decision to the remove step, and a dangling target must not
/// turn a cheap rename-class operation into a resolvable read.
async fn copy_local_backup(
    destination_root: &Path,
    relative: &RelativePath,
    backup_abs: &Path,
) -> std::result::Result<(), std::io::Error> {
    let source_abs = destination_root.join(relative.as_path());
    let source_meta = tokio::fs::symlink_metadata(&source_abs).await?;
    if source_meta.file_type().is_symlink() {
        // Preserve the link itself in the backup, matching the push side's
        // symlink-no-backup semantics: remove proceeds, the link is recreated
        // from the scanned target if a later sync wants it.
        let target = tokio::fs::read_link(&source_abs).await?;
        #[cfg(unix)]
        {
            tokio::fs::symlink(&target, backup_abs).await?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            return Ok(());
        }
    }
    if !source_meta.is_file() {
        return Ok(());
    }
    // Rename is atomic and cannot be interrupted into a partial backup; the
    // source is about to be replaced or deleted, so moving is the correct
    // preservation. A cross-device backup dir falls back to a copy.
    if tokio::fs::rename(&source_abs, backup_abs).await.is_ok() {
        return Ok(());
    }
    tokio::fs::copy(&source_abs, backup_abs).await?;
    Ok(())
}

/// Stage a symlink beside the destination and rename it into place so a
/// link-over-file (or link-over-link) replacement is atomic.
async fn replace_local_symlink(target: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| RemotePullError::LocalMutation(parent.to_path_buf(), error))?;
        }
        let temp = crate::temp_file::TempFileGuard::temp_path_for(dest);
        let guard = crate::temp_file::TempFileGuard::new(&temp);
        tokio::fs::symlink(target, &temp)
            .await
            .map_err(|error| RemotePullError::LocalMutation(temp.clone(), error))?;
        tokio::fs::rename(&temp, dest)
            .await
            .map_err(|error| RemotePullError::LocalMutation(dest.to_path_buf(), error))?;
        drop(guard);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (target, dest);
        Err(RemotePullError::LocalMutation(
            dest.to_path_buf(),
            std::io::Error::other("symlinks are not supported on this platform"),
        ))
    }
}

async fn remove_local_entry(
    path: &Path,
    is_directory: bool,
) -> std::result::Result<(), std::io::Error> {
    let meta = tokio::fs::symlink_metadata(path).await?;
    if meta.file_type().is_symlink() {
        tokio::fs::remove_file(path).await
    } else if meta.is_dir() {
        if is_directory {
            tokio::fs::remove_dir_all(path).await
        } else {
            // The journal says file but the tree grew a directory beneath
            // the scan: refuse rather than recursively delete un-planned
            // entries.
            Err(std::io::Error::other(
                "destination entry changed to a directory since the scan",
            ))
        }
    } else {
        tokio::fs::remove_file(path).await
    }
}

async fn set_local_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(|error| RemotePullError::LocalMutation(path.to_path_buf(), error))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

async fn set_local_mtime(path: &Path, modified: Timestamp) -> Result<()> {
    #[cfg(unix)]
    {
        set_mtime_via_filetime(path, modified).await
    }
    #[cfg(not(unix))]
    {
        let _ = modified;
        Ok(())
    }
}

#[cfg(unix)]
async fn set_mtime_via_filetime(path: &Path, modified: Timestamp) -> Result<()> {
    let time = filetime::FileTime::from_unix_time(modified.seconds(), modified.nanoseconds());
    let symlink_followed = false;
    filetime::set_symlink_file_times(path, time, time).map_err(|error| {
        let _ = symlink_followed;
        RemotePullError::LocalMutation(path.to_path_buf(), error)
    })?;
    Ok(())
}

#[cfg(unix)]
async fn set_staged_metadata(
    staged: &mut dyn StagedWriter,
    mode: u32,
    modified: Option<Timestamp>,
) -> Result<()> {
    use crate::endpoint::FileMetadata;
    // Without --times the committed file keeps its natural creation time
    // (rsync semantics: no -t means the destination mtime is "now"); with
    // --times it carries the source's scanned mtime.
    let modified = modified
        .map(system_time_from_timestamp)
        .unwrap_or_else(std::time::SystemTime::now);
    let metadata = FileMetadata {
        size: 0,
        modified,
        is_dir: false,
        is_symlink: false,
        mode,
    };
    staged.set_metadata(&metadata).await.map_err(|error| {
        let message = error.to_string();
        RemotePullError::LocalMutation(PathBuf::new(), std::io::Error::other(message))
    })?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_staged_metadata(
    staged: &mut dyn StagedWriter,
    _mode: u32,
    _modified: Option<Timestamp>,
) -> Result<()> {
    // Non-Unix staged metadata is applied nowhere else in 0.5; keep the
    // contract loud if ever reached.
    Err(RemotePullError::LocalMutation(
        PathBuf::new(),
        std::io::Error::other("staged metadata is unix-only in the v3 pull"),
    ))
}

fn system_time_from_timestamp(timestamp: Timestamp) -> std::time::SystemTime {
    if timestamp.seconds() >= 0 {
        std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::new(timestamp.seconds() as u64, timestamp.nanoseconds())
    } else {
        std::time::SystemTime::UNIX_EPOCH
            - std::time::Duration::new((-(timestamp.seconds() as i128)) as u64, 0)
    }
}

impl crate::remote::push_controller::SyncPlanExecutor for RemotePullExecutor {
    type Action = RemotePullAction;
    type Error = RemotePullError;

    fn lower(
        &self,
        op: crate::engine::domain::SyncOp,
        policy: crate::remote::push::RemotePushPolicy,
    ) -> std::result::Result<
        crate::remote::push_controller::LoweredSyncWork<RemotePullAction>,
        RemotePullError,
    > {
        let lowered = crate::remote::pull_lower::lower_pull_op(
            op,
            crate::remote::pull_lower::PullLowerPolicy {
                preserve_permissions: policy.preserve_permissions,
                preserve_times: policy.preserve_times,
            },
        )?;
        Ok(crate::remote::push_controller::LoweredSyncWork {
            main: lowered.main,
            finalize: lowered.finalize,
        })
    }

    fn is_directory_action(&self, action: &RemotePullAction) -> bool {
        matches!(action, RemotePullAction::CreateDirectory { .. })
    }

    fn is_leaf_action(&self, _action: &RemotePullAction) -> bool {
        true
    }

    fn leaf_resources(&self, action: &RemotePullAction) -> ResourceRequest {
        crate::remote::pull_lower::action_resources(action)
    }

    async fn execute(
        &self,
        item: WorkItem<RemotePullAction>,
    ) -> std::result::Result<Option<crate::remote::transfer::TransferSummary>, RemotePullError>
    {
        RemotePullExecutor::execute(self, item).await
    }

    async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> std::result::Result<(), RemotePullError> {
        RemotePullExecutor::execute_delete(self, action).await
    }

    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> std::result::Result<(), RemotePullError> {
        RemotePullExecutor::execute_finalize(self, metadata).await
    }

    /// Pulls have no local source to remove; parity skips are just skips.
    async fn remove_verified_parity_source(
        &self,
        _source: &Entry,
    ) -> std::result::Result<(), RemotePullError> {
        Ok(())
    }

    fn on_execute_error(
        &self,
        error: &RemotePullError,
    ) -> crate::remote::push_controller::RemotePushControllerError {
        crate::remote::push_controller::RemotePushControllerError::Worker(error.to_string())
    }
}
