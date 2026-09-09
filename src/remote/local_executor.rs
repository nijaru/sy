//! Local-sync executor for the shared v3 controller pipeline (item 6).
//!
//! The third `SyncPlanExecutor` implementation after push and pull: same
//! safety-ordered controller loop (preorder directories, bounded concurrent
//! leaves, reverse-order deletes, journal-owned finalize), same planner and
//! reconciler, with the local filesystem on both sides.
//!
//! File transfers route through `endpoint::transfer::transfer_file`, which
//! keeps the native fast paths (clonefile/copy_file_range, reflink+patch,
//! sparse extent copy) and transactional staging; those strategies are the
//! entire point of local sync and must not regress behind a generic
//! streaming loop.

use crate::endpoint::transfer::{transfer_file, TransferOptions, TransferResult};
use crate::engine::domain::{Entry, EntryKind, RelativePath, Timestamp};
use crate::engine::scheduler::{ResourceRequest, Scheduler};
use crate::engine::work::WorkItem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Working set per in-flight local transfer; mirrors the remote executors so
/// the scheduler byte budget is symmetric across directions.
pub const LOCAL_FILE_WORKING_SET: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LocalSyncError {
    #[error(transparent)]
    Scheduler(#[from] crate::engine::scheduler::SchedulerError),

    #[error("local sync destination mutation failed for {0}: {1}")]
    Destination(PathBuf, std::io::Error),

    #[error("local sync source mutation failed for {0}: {1}")]
    Source(PathBuf, std::io::Error),

    #[error("symlink action is missing the scanned target for {0}")]
    MissingSymlinkTarget(PathBuf),

    #[error("regular-file transfer requires scanned Unix mode metadata for {0}")]
    MissingScannedMode(PathBuf),

    #[error("--backup location for {0} is not representable")]
    InvalidBackupPath(PathBuf),

    #[error("staged verification failed for {path}: expected {expected}, got {actual}")]
    VerificationFailed {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

pub type Result<T> = std::result::Result<T, LocalSyncError>;

#[derive(Debug, Clone)]
pub enum LocalSyncAction {
    CreateDirectory {
        source: Entry,
    },
    TransferFile {
        source: Entry,
        destination: Option<Entry>,
        metadata: LocalTransferMetadata,
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
pub struct LocalTransferMetadata {
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
}

pub struct LocalSyncExecutor {
    source_root: PathBuf,
    destination_root: PathBuf,
    scheduler: Scheduler,
    /// --backup: enabled marker, backup directory (None = beside the file),
    /// and suffix, mirroring the other executors.
    backup: bool,
    backup_dir: Option<PathBuf>,
    backup_suffix: String,
    /// --copy-links: transfers stat through source symlinks so the target's
    /// bytes and size flow through the native fast paths.
    follow_symlinks: bool,
    /// Shared --bwlimit pacing; `None` keeps the kernel fast paths.
    rate_limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// --remove-source-files: remove a committed or parity-verified source
    /// entry after the destination commit is acknowledged.
    remove_source_files: bool,
    /// Post-write BLAKE3 verification (--verify).
    verify_on_write: bool,
    reporter: Option<Arc<crate::sync::output::SyncReporter>>,
}

impl LocalSyncExecutor {
    pub fn new(source_root: PathBuf, destination_root: PathBuf, scheduler: Scheduler) -> Self {
        Self {
            source_root,
            destination_root,
            scheduler,
            backup: false,
            backup_dir: None,
            backup_suffix: String::new(),
            follow_symlinks: false,
            rate_limiter: None,
            remove_source_files: false,
            verify_on_write: false,
            reporter: None,
        }
    }

    pub fn with_backup(mut self, enabled: bool, dir: Option<PathBuf>, suffix: String) -> Self {
        self.backup = enabled;
        self.backup_dir = dir;
        self.backup_suffix = suffix;
        self
    }

    pub fn with_follow_symlinks(mut self, follow: bool) -> Self {
        self.follow_symlinks = follow;
        self
    }

    pub fn with_rate_limiter(
        mut self,
        limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    ) -> Self {
        self.rate_limiter = limiter;
        self
    }

    pub fn with_remove_source_files(mut self, enabled: bool) -> Self {
        self.remove_source_files = enabled;
        self
    }

    pub fn with_verify_on_write(mut self, enabled: bool) -> Self {
        self.verify_on_write = enabled;
        self
    }

    pub fn with_reporter(
        mut self,
        reporter: Option<Arc<crate::sync::output::SyncReporter>>,
    ) -> Self {
        self.reporter = reporter;
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

    fn source_path(&self, relative: &RelativePath) -> PathBuf {
        self.source_root.join(relative.as_path())
    }

    fn destination_path(&self, relative: &RelativePath) -> PathBuf {
        self.destination_root.join(relative.as_path())
    }

    /// Backup location for one destination-relative path, matching the
    /// other executors: beside the file (name + suffix) or under --backup-dir
    /// with the tree shape preserved (GNU rsync semantics). Always
    /// destination-anchored — never relative to the process CWD.
    fn backup_destination_for(&self, relative: &RelativePath) -> Result<PathBuf> {
        let file_name = relative
            .as_path()
            .file_name()
            .ok_or_else(|| LocalSyncError::InvalidBackupPath(relative.as_path().to_path_buf()))?
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
                let mut backup = self.destination_path(relative);
                backup.set_file_name(backup_name);
                Ok(backup)
            }
        }
    }

    /// Preserve a to-be-replaced destination file. COPY semantics: the
    /// original stays in place so it can still serve as the reflink/delta
    /// basis for the replacement transfer's staging. The copy never follows
    /// symlinks; a backup that resolved a link target would copy the wrong
    /// tree (and could touch a target outside both roots).
    async fn backup_replacement_file(&self, relative: &RelativePath) -> Result<()> {
        let source = self.destination_path(relative);
        let backup = self.backup_destination_for(relative)?;
        let metadata = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|error| LocalSyncError::Destination(source.clone(), error))?;
        if metadata.file_type().is_symlink() {
            // Preserve the link itself; the replacement proceeds, and the
            // link is recreated from the scanned target.
            let target = tokio::fs::read_link(&source)
                .await
                .map_err(|error| LocalSyncError::Destination(source.clone(), error))?;
            if let Some(parent) = backup.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| LocalSyncError::Destination(backup.clone(), error))?;
            }
            tokio::fs::symlink(&target, &backup)
                .await
                .map_err(|error| LocalSyncError::Destination(backup.clone(), error))?;
            return Ok(());
        }
        if !metadata.is_file() {
            return Ok(());
        }
        if let Some(parent) = backup.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| LocalSyncError::Destination(backup.clone(), error))?;
        }
        tokio::fs::copy(&source, &backup)
            .await
            .map_err(|error| LocalSyncError::Destination(backup.clone(), error))?;
        Ok(())
    }

    /// Preserve a to-be-DELETED destination entry. MOVE semantics: rename
    /// is atomic, cannot leave a partial backup, and IS the deletion for
    /// the original path. Returns `true` when the entry was moved away (the
    /// caller must NOT remove it again); a cross-device --backup-dir falls
    /// back to a copy and the caller removes the original normally.
    async fn backup_deleted_entry(&self, relative: &RelativePath) -> Result<bool> {
        let source = self.destination_path(relative);
        let backup = self.backup_destination_for(relative)?;
        let metadata = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|error| LocalSyncError::Destination(source.clone(), error))?;
        if let Some(parent) = backup.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| LocalSyncError::Destination(backup.clone(), error))?;
        }
        if metadata.file_type().is_symlink() {
            // Preserve the link itself; the target is never resolved.
            let target = tokio::fs::read_link(&source)
                .await
                .map_err(|error| LocalSyncError::Destination(source.clone(), error))?;
            tokio::fs::symlink(&target, &backup)
                .await
                .map_err(|error| LocalSyncError::Destination(backup.clone(), error))?;
            tokio::fs::remove_file(&source)
                .await
                .map_err(|error| LocalSyncError::Destination(source.clone(), error))?;
            return Ok(true);
        }
        if !metadata.is_file() {
            return Ok(false);
        }
        if tokio::fs::rename(&source, &backup).await.is_ok() {
            return Ok(true);
        }
        tokio::fs::copy(&source, &backup)
            .await
            .map_err(|error| LocalSyncError::Destination(backup.clone(), error))?;
        Ok(false)
    }

    async fn execute(
        &self,
        item: WorkItem<LocalSyncAction>,
    ) -> Result<Option<crate::remote::transfer::TransferSummary>> {
        let (action, resources) = item.into_parts();
        let _permit = self.scheduler.acquire(resources).await?;

        match action {
            LocalSyncAction::CreateDirectory { source } => {
                let path = self.destination_path(&source.path);
                tokio::fs::create_dir_all(&path)
                    .await
                    .map_err(|error| LocalSyncError::Destination(path.clone(), error))?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Directory,
                    &source.path,
                );
                Ok(None)
            }
            LocalSyncAction::TransferFile {
                source,
                destination,
                metadata,
            } => {
                let is_update = destination.is_some();
                if self.backup && destination.as_ref().is_some_and(|entry| entry.is_file()) {
                    self.backup_replacement_file(&source.path).await?;
                }
                let transfer = self
                    .transfer_source_file(&source, &destination, &metadata)
                    .await?;
                self.remove_committed_source(&source).await?;
                let op = if is_update {
                    crate::sync::output::ItemizeOp::Update
                } else {
                    crate::sync::output::ItemizeOp::Create
                };
                self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
                Ok(Some(transfer))
            }
            LocalSyncAction::ReplaceSymlink { source, modified } => {
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    LocalSyncError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
                self.replace_symlink(target, &source.path).await?;
                if let Some(modified) = modified {
                    self.set_mtime(&self.destination_path(&source.path), modified)
                        .await?;
                }
                self.remove_committed_source(&source).await?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Symlink,
                    &source.path,
                );
                Ok(None)
            }
            LocalSyncAction::ApplyMetadata {
                source,
                unix_mode,
                modified,
            } => {
                let path = self.destination_path(&source.path);
                if let Some(mode) = unix_mode {
                    self.set_mode(&path, mode).await?;
                }
                if let Some(modified) = modified {
                    if source.kind != EntryKind::Symlink {
                        self.set_mtime(&path, modified).await?;
                    }
                }
                Ok(None)
            }
        }
    }

    /// One file through the transfer layer's staging, strategy selection,
    /// verification, and atomic commit.
    async fn transfer_source_file(
        &self,
        source: &Entry,
        destination: &Option<Entry>,
        metadata: &LocalTransferMetadata,
    ) -> Result<crate::remote::transfer::TransferSummary> {
        let source_endpoint = crate::endpoint::local::LocalEndpoint::new(self.source_root.clone());
        let destination_endpoint =
            crate::endpoint::local::LocalEndpoint::new(self.destination_root.clone());
        let result: TransferResult = transfer_file(
            &source_endpoint,
            source.path.as_path(),
            &destination_endpoint,
            source.path.as_path(),
            TransferOptions {
                // A type replacement (file over symlink, link over file)
                // must not treat the mismatched destination as a delta
                // basis: staging replaces it wholesale.
                update: destination.is_some(),
                verify: self.verify_on_write,
                follow_symlinks: self.follow_symlinks,
                rate_limiter: self.rate_limiter.clone(),
            },
        )
        .await
        .map_err(|error| match error {
            crate::error::SyncError::Io(io) => {
                LocalSyncError::Destination(self.destination_path(&source.path), io)
            }
            other => LocalSyncError::Destination(
                self.destination_path(&source.path),
                std::io::Error::other(other.to_string()),
            ),
        })?;
        // Fail-fast on staged verification mismatch: the previous
        // destination is intact (the transfer layer aborted staging), and
        // the sync surfaces the failure instead of tolerating it.
        if let crate::endpoint::io::VerificationStatus::Failed { expected, actual } =
            result.verification
        {
            return Err(LocalSyncError::VerificationFailed {
                path: self.destination_path(&source.path),
                expected: expected.to_hex().to_string(),
                actual: actual.to_hex().to_string(),
            });
        }
        // The transfer layer applies staged metadata itself for native
        // strategies; the final mode/mtime land through the explicit calls
        // below so every strategy path shares one ordering.
        let destination_path = self.destination_path(&source.path);
        if let Some(mode) = metadata.unix_mode {
            self.set_mode(&destination_path, mode).await?;
        }
        if let Some(modified) = metadata.modified {
            if source.kind != EntryKind::Symlink {
                self.set_mtime(&destination_path, modified).await?;
            }
        }
        // The native strategies do not compute a whole-file digest on the
        // happy path (verification is staged-hash based and surfaced
        // above); the summary carries byte accounting for the controller's
        // counters.
        Ok(crate::remote::transfer::TransferSummary {
            file_size: source.size,
            digest: [0_u8; 32],
            literal_bytes: result.bytes_written,
            reused_bytes: 0,
        })
    }

    /// Stage a symlink beside the destination and rename it into place so a
    /// link-over-file (or link-over-link) replacement is atomic.
    async fn replace_symlink(&self, target: &Path, relative: &RelativePath) -> Result<()> {
        let dest = self.destination_path(relative);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| LocalSyncError::Destination(parent.to_path_buf(), error))?;
        }
        let temp = crate::temp_file::TempFileGuard::temp_path_for(&dest);
        let guard = crate::temp_file::TempFileGuard::new(&temp);
        tokio::fs::symlink(target, &temp)
            .await
            .map_err(|error| LocalSyncError::Destination(temp.clone(), error))?;
        tokio::fs::rename(&temp, &dest)
            .await
            .map_err(|error| LocalSyncError::Destination(dest.clone(), error))?;
        guard.defuse();
        Ok(())
    }

    async fn set_mode(&self, path: &Path, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .await
                .map_err(|error| LocalSyncError::Destination(path.to_path_buf(), error))?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    /// mtime through the link: a symlink's own timestamp matters, not its
    /// target's (filetime's symlink variant on Unix).
    async fn set_mtime(&self, path: &Path, modified: Timestamp) -> Result<()> {
        let time = filetime::FileTime::from_unix_time(modified.seconds(), modified.nanoseconds());
        filetime::set_symlink_file_times(path, time, time)
            .map_err(|error| LocalSyncError::Destination(path.to_path_buf(), error))?;
        Ok(())
    }

    /// --remove-source-files: remove the source entry after the destination
    /// commit. The scan identity is re-checked first — a source replaced or
    /// modified after the scan must not be removed, because the moved bytes
    /// would not be the verified ones. Only non-directories move; empty
    /// directories stay, matching rsync.
    async fn remove_committed_source(&self, source: &Entry) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        let path = self.source_path(&source.path);
        let Some(expected) = source.identity else {
            return Err(LocalSyncError::Source(
                path.clone(),
                std::io::Error::other("source entry changed between scan and removal"),
            ));
        };
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|_| {
            LocalSyncError::Source(
                path.clone(),
                std::io::Error::other("source entry changed between scan and removal"),
            )
        })?;
        let kind = if metadata.file_type().is_symlink() {
            EntryKind::Symlink
        } else if metadata.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let current = crate::endpoint::local_identity::metadata_identity(&metadata, kind)
            .ok_or_else(|| {
                LocalSyncError::Source(
                    path.clone(),
                    std::io::Error::other("source entry changed between scan and removal"),
                )
            })?;
        if current != expected {
            return Err(LocalSyncError::Source(
                path.clone(),
                std::io::Error::other("source entry changed between scan and removal"),
            ));
        }
        if kind == EntryKind::Directory {
            return Ok(());
        }
        tokio::fs::remove_file(&path)
            .await
            .map_err(|error| LocalSyncError::Source(path.clone(), error))?;
        Ok(())
    }

    /// Delete one destination-only entry after the delete-threshold gate. A
    /// non-empty directory is kept without an error (its surviving content
    /// is legitimate, e.g. --backup repopulation, matching rsync); only a
    /// genuinely empty directory is removed.
    async fn execute_delete(&self, action: crate::engine::delete_plan::DeleteAction) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                ..ResourceRequest::default()
            })
            .await?;
        let mut moved_by_backup = false;
        if self.backup && !action.is_directory {
            // A backup failure of a symlink is tolerated (the target is
            // never resolved); regular files must back up before removal.
            // A move-semantics backup already removed the original path.
            match self.backup_deleted_entry(&action.path).await {
                Ok(moved) => moved_by_backup = moved,
                Err(error) => {
                    let is_symlink =
                        tokio::fs::symlink_metadata(self.destination_path(&action.path))
                            .await
                            .map(|meta| meta.file_type().is_symlink())
                            .unwrap_or(false);
                    if !is_symlink {
                        return Err(error);
                    }
                }
            }
        }
        if moved_by_backup {
            self.report(
                crate::sync::output::ItemizeOp::Delete,
                crate::sync::output::ItemizeKind::File,
                &action.path,
            );
            return Ok(());
        }
        let path = self.destination_path(&action.path);
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|error| LocalSyncError::Destination(path.clone(), error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|error| LocalSyncError::Destination(path.clone(), error))?;
        } else if action.is_directory {
            match tokio::fs::remove_dir(&path).await {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::DirectoryNotEmpty
                        || error.raw_os_error() == Some(66)
                        || error.raw_os_error() == Some(39) =>
                {
                    // Kept without an error: the directory holds surviving
                    // legitimate content (a protected descendant or a --backup
                    // file), mirroring the legacy journal replay.
                    tracing::debug!(
                        path = %path.display(),
                        "kept non-empty destination directory"
                    );
                }
                Err(error) => return Err(LocalSyncError::Destination(path.clone(), error)),
            }
        } else {
            // The plan says file-like, but the destination grew a directory
            // beneath the scan: refuse rather than recursively delete
            // un-planned entries.
            return Err(LocalSyncError::Destination(
                path,
                std::io::Error::other("destination entry changed to a directory since the scan"),
            ));
        }
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

    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                ..ResourceRequest::default()
            })
            .await?;
        let path = self.destination_path(&metadata.path);
        if let Some(mode) = metadata.unix_mode {
            self.set_mode(&path, mode).await?;
        }
        if let Some(modified) = metadata.modified {
            if metadata.kind != EntryKind::Symlink {
                self.set_mtime(&path, modified).await?;
            }
        }
        Ok(())
    }

    /// --remove-source-files for parity-verified entries the planner marked
    /// Skip::Unchanged: no transfer ran, but the destination holds the exact
    /// verified bytes, so the source may move.
    async fn remove_verified_parity_source(&self, source: &Entry) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        self.remove_committed_source(source).await
    }
}

impl crate::remote::push_controller::SyncPlanExecutor for LocalSyncExecutor {
    type Action = LocalSyncAction;
    type Error = LocalSyncError;

    fn lower(
        &self,
        op: crate::engine::domain::SyncOp,
        policy: crate::remote::push::RemotePushPolicy,
    ) -> std::result::Result<
        crate::remote::push_controller::LoweredSyncWork<LocalSyncAction>,
        LocalSyncError,
    > {
        let lowered = lower_local_op(
            op,
            LocalLowerPolicy {
                preserve_permissions: policy.preserve_permissions,
                preserve_times: policy.preserve_times,
            },
        )?;
        Ok(crate::remote::push_controller::LoweredSyncWork {
            main: lowered.main,
            finalize: lowered.finalize,
        })
    }

    fn is_directory_action(&self, action: &LocalSyncAction) -> bool {
        matches!(action, LocalSyncAction::CreateDirectory { .. })
    }

    fn is_leaf_action(&self, _action: &LocalSyncAction) -> bool {
        true
    }

    fn leaf_resources(&self, action: &LocalSyncAction) -> ResourceRequest {
        action_resources(action)
    }

    async fn execute(
        &self,
        item: WorkItem<LocalSyncAction>,
    ) -> std::result::Result<Option<crate::remote::transfer::TransferSummary>, LocalSyncError> {
        LocalSyncExecutor::execute(self, item).await
    }

    async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::execute_delete(self, action).await
    }

    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::execute_finalize(self, metadata).await
    }

    async fn remove_verified_parity_source(
        &self,
        source: &Entry,
    ) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::remove_verified_parity_source(self, source).await
    }

    fn on_execute_error(
        &self,
        error: &LocalSyncError,
    ) -> crate::remote::push_controller::RemotePushControllerError {
        crate::remote::push_controller::RemotePushControllerError::Worker(error.to_string())
    }
}

pub fn action_resources(action: &LocalSyncAction) -> ResourceRequest {
    match action {
        LocalSyncAction::TransferFile { .. } => ResourceRequest {
            active_files: 1,
            buffered_bytes: LOCAL_FILE_WORKING_SET,
            metadata_ops: 0,
            cpu_tasks: 1,
            network_writes: 0,
        },
        _ => ResourceRequest {
            active_files: 0,
            buffered_bytes: 0,
            metadata_ops: 1,
            cpu_tasks: 0,
            network_writes: 0,
        },
    }
}

/// Lower one semantic planner operation to a local action. Mirrors the push
/// and pull lowering: same mode/times policy handling, same
/// directory-transition refusal (the controller replays directory
/// finalize metadata child-before-parent; a transactional directory
/// replacement is a separate design).
pub fn lower_local_op(
    op: crate::engine::domain::SyncOp,
    policy: LocalLowerPolicy,
) -> std::result::Result<LoweredLocal, LocalSyncError> {
    use crate::engine::domain::SyncOp;

    match op {
        SyncOp::Create { source } => lower_create(source, policy),
        SyncOp::Update {
            source,
            destination,
        } => lower_update(source, destination, policy),
        SyncOp::Replace {
            source,
            destination,
        } => lower_replace(source, destination, policy),
        SyncOp::Metadata {
            source,
            destination,
        } => lower_metadata(source, destination, policy),
        SyncOp::Skip { .. } => Ok(LoweredLocal::default()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalLowerPolicy {
    pub preserve_permissions: bool,
    pub preserve_times: bool,
}

/// Metadata a lowered action should apply: permission and/or mtime drift,
/// both optional (None = not requested or already equal).
pub type RequestedMetadata = Option<(Option<u32>, Option<Timestamp>)>;

#[derive(Debug, Default)]
pub struct LoweredLocal {
    pub main: Option<WorkItem<LocalSyncAction>>,
    pub finalize: Option<WorkItem<LocalSyncAction>>,
}

fn lower_create(
    source: Entry,
    policy: LocalLowerPolicy,
) -> std::result::Result<LoweredLocal, LocalSyncError> {
    match source.kind {
        EntryKind::Directory => {
            let finalize = requested_metadata(&source, None, policy, true)?
                .map(|(unix_mode, modified)| metadata_work(source.clone(), unix_mode, modified));
            Ok(LoweredLocal {
                main: Some(mutation_work(LocalSyncAction::CreateDirectory { source })),
                finalize,
            })
        }
        EntryKind::File => {
            // Staged files are private at 0600, so a committed file always
            // needs an explicit final mode; the source's scanned mode is the
            // create default (matches the other executors).
            let mode = source.unix_mode.ok_or_else(|| {
                LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = LocalTransferMetadata {
                unix_mode: Some(mode),
                modified: Some(source.modified),
            };
            Ok(LoweredLocal {
                main: Some(file_work(LocalSyncAction::TransferFile {
                    source,
                    destination: None,
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Symlink => Ok(LoweredLocal {
            main: Some(mutation_work(LocalSyncAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
    }
}

fn lower_update(
    source: Entry,
    destination: Entry,
    policy: LocalLowerPolicy,
) -> std::result::Result<LoweredLocal, LocalSyncError> {
    match source.kind {
        EntryKind::File => {
            let mode = if policy.preserve_permissions {
                source.unix_mode
            } else {
                destination.unix_mode
            }
            .ok_or_else(|| {
                LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = LocalTransferMetadata {
                unix_mode: Some(mode),
                modified: Some(source.modified),
            };
            Ok(LoweredLocal {
                main: Some(file_work(LocalSyncAction::TransferFile {
                    source,
                    destination: Some(destination),
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Directory => lower_metadata(source, destination, policy),
        EntryKind::Symlink => Ok(LoweredLocal {
            main: Some(mutation_work(LocalSyncAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
    }
}

fn lower_replace(
    source: Entry,
    _destination: Entry,
    policy: LocalLowerPolicy,
) -> std::result::Result<LoweredLocal, LocalSyncError> {
    if source.is_directory() {
        return Err(LocalSyncError::Destination(
            source.path.as_path().to_path_buf(),
            std::io::Error::other("transactional directory replacement is not implemented"),
        ));
    }
    match source.kind {
        EntryKind::File => {
            let mode = source.unix_mode.ok_or_else(|| {
                LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = LocalTransferMetadata {
                unix_mode: Some(mode),
                modified: Some(source.modified),
            };
            Ok(LoweredLocal {
                main: Some(file_work(LocalSyncAction::TransferFile {
                    source,
                    destination: None,
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Symlink => Ok(LoweredLocal {
            main: Some(mutation_work(LocalSyncAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
        EntryKind::Directory => unreachable!("directory transitions refused above"),
    }
}

fn lower_metadata(
    source: Entry,
    destination: Entry,
    policy: LocalLowerPolicy,
) -> std::result::Result<LoweredLocal, LocalSyncError> {
    let Some((unix_mode, modified)) =
        requested_metadata(&source, Some(&destination), policy, false)?
    else {
        return Ok(LoweredLocal::default());
    };
    let work = metadata_work(source.clone(), unix_mode, modified);
    if source.is_directory() {
        Ok(LoweredLocal {
            main: None,
            finalize: Some(work),
        })
    } else {
        Ok(LoweredLocal {
            main: Some(work),
            finalize: None,
        })
    }
}

fn requested_metadata(
    source: &Entry,
    destination: Option<&Entry>,
    policy: LocalLowerPolicy,
    include_requested_even_if_unknown_destination: bool,
) -> std::result::Result<RequestedMetadata, LocalSyncError> {
    let unix_mode = if policy.preserve_permissions
        && (include_requested_even_if_unknown_destination
            || destination.is_some_and(|entry| entry.unix_mode != source.unix_mode))
    {
        Some(source.unix_mode.ok_or_else(|| {
            LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
        })?)
    } else {
        None
    };
    let modified = policy
        .preserve_times
        .then_some(source.modified)
        .filter(|_| destination.is_none_or(|entry| entry.modified != source.modified));
    Ok(Some((unix_mode, modified)))
}

fn file_work(action: LocalSyncAction) -> WorkItem<LocalSyncAction> {
    let resources = action_resources(&action);
    WorkItem::new(action, resources)
}

fn mutation_work(action: LocalSyncAction) -> WorkItem<LocalSyncAction> {
    let resources = action_resources(&action);
    WorkItem::new(action, resources)
}

fn metadata_work(
    source: Entry,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
) -> WorkItem<LocalSyncAction> {
    mutation_work(LocalSyncAction::ApplyMetadata {
        source,
        unix_mode,
        modified,
    })
}
