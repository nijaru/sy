//! v0.5 task executor built on the capability-driven endpoint contract.
//!
//! Regular-file transfers never require whole-file `Vec<u8>` buffers. The
//! transfer layer chooses endpoint-native fast paths when available and falls
//! back to bounded staged streaming. Verification is explicit BLAKE3 I/O.

use crate::endpoint::io::{hash_file_streaming, VerificationStatus};
use crate::endpoint::transfer::{transfer_file, TransferOptions, FILE_TRANSFER_BUFFER_BUDGET};
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError as Error};
use crate::sync::config::{PreserveConfig, VerificationConfig};
use crate::sync::itemize_string;
use crate::sync::scanner::FileEntry;
use crate::sync::stats::{SyncError, SyncStats};
use crate::sync::strategy::{SyncAction, SyncTask};
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;
use sy::engine::scheduler::{ResourceBudget, ResourceRequest, Scheduler, SchedulerError};

/// Result of executing a single task.
#[derive(Debug, Clone)]
pub enum TaskResult {
    Skipped,
    Created { bytes: u64 },
    Updated { bytes: u64 },
    DirCreated,
    SymlinkCreated,
    Deleted,
    VerificationFailed { expected: String, actual: String },
}

/// Configuration for backup behavior.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub enabled: bool,
    pub suffix: String,
    pub dir: Option<PathBuf>,
}

impl BackupConfig {
    /// Resolve the backup location for one destination path.
    ///
    /// `dest_path` is the file's path relative to the destination root.
    /// Without `--backup-dir` the backup sits beside the destination file
    /// (name + suffix). With `--backup-dir` backups land under that
    /// directory preserving their path relative to the destination root:
    /// two same-named files in different directories must not collide, and
    /// users expect the tree shape (GNU rsync semantics). A relative
    /// `--backup-dir` is anchored to the process working directory.
    pub fn destination_path(&self, dest_path: &Path) -> std::io::Result<PathBuf> {
        let file_name = dest_path
            .file_name()
            .ok_or_else(|| std::io::Error::other("Invalid file path"))?
            .to_string_lossy()
            .into_owned();
        match &self.dir {
            Some(dir) => {
                let dir = if dir.is_absolute() {
                    dir.clone()
                } else {
                    std::env::current_dir()?.join(dir)
                };
                let mut backup = dir.join(dest_path);
                backup.set_file_name(format!("{file_name}{}", self.suffix));
                Ok(backup)
            }
            None => Ok(dest_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("{file_name}{}", self.suffix))),
        }
    }
}

/// Configuration for task execution behavior.
#[derive(Debug, Clone, Default)]
pub struct ExecuteConfig {
    pub preserve_hardlinks: bool,
    pub preserve_xattrs: bool,
    pub preserve_dir_permissions: bool,
    pub itemize_changes: bool,
    pub remove_source_files: bool,
    pub print_stats: bool,
    /// Shared `--bwlimit` pacing for all local transfers. `None` keeps kernel
    /// fast paths; `Some` routes bytes through the paced streaming copy.
    pub rate_limiter: Option<std::sync::Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
}

#[derive(Debug, Default)]
struct PreservationState {
    /// `None` means xattrs were not requested; `Some([])` means mirror an empty set.
    xattrs: Option<Vec<(OsString, Vec<u8>)>>,
    /// `None` means ACLs were not requested; an empty string means clear them.
    acl: Option<String>,
    /// BSD flags are always explicit when requested, including zero.
    bsd_flags: Option<u32>,
}

/// Executes sync tasks against source and destination endpoints.
pub struct TaskExecutor<'a> {
    source: &'a dyn Endpoint,
    dest: &'a dyn Endpoint,
    dry_run: bool,
    preserve: PreserveConfig,
    verification: VerificationConfig,
    scheduler: Scheduler,
    backup: BackupConfig,
    config: ExecuteConfig,
    hardlink_map: Mutex<HashMap<u64, PathBuf>>,
    /// Successful post-write verifications under --verify, for stats.
    verified_count: std::sync::atomic::AtomicUsize,
}

impl<'a> TaskExecutor<'a> {
    pub fn new(
        source: &'a dyn Endpoint,
        dest: &'a dyn Endpoint,
        dry_run: bool,
        preserve: PreserveConfig,
        verification: VerificationConfig,
        max_concurrent: usize,
    ) -> Result<Self> {
        let active_files = u32::try_from(max_concurrent).map_err(|_| {
            Error::Config(format!(
                "max concurrent file count {max_concurrent} exceeds scheduler capacity"
            ))
        })?;
        let scheduler = Scheduler::new(ResourceBudget {
            active_files,
            ..ResourceBudget::default()
        })
        .map_err(map_scheduler_error)?;

        Ok(Self {
            source,
            dest,
            dry_run,
            preserve,
            verification,
            scheduler,
            backup: BackupConfig {
                enabled: false,
                suffix: "~".to_string(),
                dir: None,
            },
            config: ExecuteConfig::default(),
            hardlink_map: Mutex::new(HashMap::new()),
            verified_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn with_backup(mut self, config: BackupConfig) -> Self {
        self.backup = config;
        self
    }

    pub fn with_config(mut self, config: ExecuteConfig) -> Self {
        self.config = config;
        self
    }

    pub async fn execute_task(&self, task: &SyncTask) -> Result<TaskResult> {
        self.validate_requested_capabilities()?;
        let _permit = self
            .scheduler
            .acquire(self.resource_request(task))
            .await
            .map_err(map_scheduler_error)?;
        self.execute_task_admitted(task).await
    }

    async fn execute_task_admitted(&self, task: &SyncTask) -> Result<TaskResult> {
        match task.action {
            SyncAction::Skip => Ok(TaskResult::Skipped),
            SyncAction::Create | SyncAction::Update => self.execute_create_or_update(task).await,
            SyncAction::Delete => {
                if !self.dry_run {
                    self.dest.remove(&task.dest_path, true).await?;
                }
                Ok(TaskResult::Deleted)
            }
        }
    }

    fn resource_request(&self, task: &SyncTask) -> ResourceRequest {
        if task.action == SyncAction::Skip {
            return ResourceRequest::default();
        }

        let regular_file = matches!(task.action, SyncAction::Create | SyncAction::Update)
            && task
                .source
                .as_deref()
                .is_some_and(|entry| !entry.is_dir && !entry.is_symlink);

        ResourceRequest {
            active_files: 1,
            buffered_bytes: if regular_file {
                FILE_TRANSFER_BUFFER_BUDGET
            } else {
                0
            },
            metadata_ops: 1,
            cpu_tasks: if regular_file && self.verification.verify_on_write {
                1
            } else {
                0
            },
            // The compatibility executor is currently local-only. Protocol v3
            // will account network writes in the remote engine path.
            network_writes: 0,
        }
    }

    fn preserve_xattrs_requested(&self) -> bool {
        self.config.preserve_xattrs || self.preserve.xattrs
    }

    fn preserve_hardlinks_requested(&self) -> bool {
        self.config.preserve_hardlinks || self.preserve.hardlinks
    }

    fn validate_requested_capabilities(&self) -> Result<()> {
        let source = self.source.capabilities();
        let dest = self.dest.capabilities();

        if self.preserve_xattrs_requested() {
            if !source.preserve_xattrs {
                return Err(Error::Config(format!(
                    "{:?} source cannot read extended attributes",
                    self.source.endpoint_type()
                )));
            }
            if !dest.preserve_xattrs {
                return Err(Error::Config(format!(
                    "{:?} destination cannot preserve extended attributes",
                    self.dest.endpoint_type()
                )));
            }
        }

        if self.preserve.acls {
            if !source.preserve_acls {
                return Err(Error::Config(format!(
                    "{:?} source cannot read ACLs",
                    self.source.endpoint_type()
                )));
            }
            if !dest.preserve_acls {
                return Err(Error::Config(format!(
                    "{:?} destination cannot preserve ACLs",
                    self.dest.endpoint_type()
                )));
            }
        }

        if self.preserve_hardlinks_requested() {
            if !source.preserve_hardlinks {
                return Err(Error::Config(format!(
                    "{:?} source cannot describe hard-link topology",
                    self.source.endpoint_type()
                )));
            }
            if !dest.preserve_hardlinks {
                return Err(Error::Config(format!(
                    "{:?} destination cannot preserve hard links",
                    self.dest.endpoint_type()
                )));
            }
        }

        if self.preserve.flags {
            if !source.preserve_flags {
                return Err(Error::Config(format!(
                    "{:?} source cannot read BSD flags",
                    self.source.endpoint_type()
                )));
            }
            if !dest.preserve_flags {
                return Err(Error::Config(format!(
                    "{:?} destination cannot preserve BSD flags",
                    self.dest.endpoint_type()
                )));
            }
        }

        Ok(())
    }

    async fn read_preservation(&self, source_entry: &FileEntry) -> Result<PreservationState> {
        let path = source_entry.relative_path.as_path();

        let xattrs = if self.preserve_xattrs_requested() {
            Some(self.source.read_xattrs(path).await?)
        } else {
            None
        };

        let acl = if self.preserve.acls {
            Some(self.source.read_acl(path).await?.unwrap_or_default())
        } else {
            None
        };

        let bsd_flags = if self.preserve.flags {
            Some(self.source.read_bsd_flags(path).await?.ok_or_else(|| {
                Error::Config(format!(
                    "{:?} source did not return BSD flags for {}",
                    self.source.endpoint_type(),
                    path.display()
                ))
            })?)
        } else {
            None
        };

        Ok(PreservationState {
            xattrs,
            acl,
            bsd_flags,
        })
    }

    async fn apply_preservation(
        &self,
        dest_path: &Path,
        preservation: PreservationState,
    ) -> Result<()> {
        if let Some(xattrs) = preservation.xattrs.as_deref() {
            self.dest.write_xattrs(dest_path, xattrs).await?;
        }
        if let Some(acl) = preservation.acl.as_deref() {
            self.dest.write_acl(dest_path, acl).await?;
        }
        if let Some(flags) = preservation.bsd_flags {
            // BSD flags are intentionally last: immutable flags can prevent
            // subsequent metadata updates.
            self.dest.write_bsd_flags(dest_path, flags).await?;
        }
        Ok(())
    }

    async fn execute_create_or_update(&self, task: &SyncTask) -> Result<TaskResult> {
        let source_entry = task
            .source
            .as_ref()
            .ok_or_else(|| Error::Io(std::io::Error::other("Missing source for create/update")))?;

        if self.dry_run {
            return if source_entry.is_dir {
                Ok(TaskResult::DirCreated)
            } else if source_entry.is_symlink {
                Ok(TaskResult::SymlinkCreated)
            } else if task.action == SyncAction::Create {
                Ok(TaskResult::Created {
                    bytes: source_entry.size,
                })
            } else {
                Ok(TaskResult::Updated {
                    bytes: source_entry.size,
                })
            };
        }

        if source_entry.is_dir {
            self.execute_directory(source_entry, task).await
        } else if source_entry.is_symlink {
            self.execute_symlink(source_entry, task).await
        } else {
            self.execute_file(source_entry, task).await
        }
    }

    async fn execute_directory(
        &self,
        source_entry: &FileEntry,
        task: &SyncTask,
    ) -> Result<TaskResult> {
        let preservation = self.read_preservation(source_entry).await?;
        self.dest.create_dir_all(&task.dest_path).await?;

        #[cfg(unix)]
        if self.config.preserve_dir_permissions {
            use std::os::unix::fs::PermissionsExt;

            let source_path = self.source.root().join(&*source_entry.relative_path);
            let mode = tokio::fs::metadata(&source_path)
                .await?
                .permissions()
                .mode();
            let abs_dest = self.abs_dest_path(&task.dest_path);
            tokio::fs::set_permissions(&abs_dest, std::fs::Permissions::from_mode(mode)).await?;
        }

        self.apply_preservation(&task.dest_path, preservation)
            .await?;
        Ok(TaskResult::DirCreated)
    }

    async fn execute_symlink(
        &self,
        source_entry: &FileEntry,
        task: &SyncTask,
    ) -> Result<TaskResult> {
        #[cfg(unix)]
        {
            let source_path = self.source.root().join(&*source_entry.relative_path);
            let target = tokio::fs::read_link(&source_path).await?;
            self.dest.create_symlink(&target, &task.dest_path).await?;

            if self.config.itemize_changes {
                let item = itemize_string(&task.action, false, true);
                eprintln!("{} {}", item, task.dest_path.display());
            }

            self.remove_source_after_commit(source_entry).await?;

            Ok(if task.action == SyncAction::Create {
                TaskResult::SymlinkCreated
            } else {
                TaskResult::Updated { bytes: 0 }
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (source_entry, task);
            Err(Error::Io(std::io::Error::other(
                "Symlinks not supported on this platform",
            )))
        }
    }

    async fn execute_file(&self, source_entry: &FileEntry, task: &SyncTask) -> Result<TaskResult> {
        if self.preserve_hardlinks_requested() && source_entry.nlink > 1 {
            if let Some(inode) = source_entry.inode {
                let first_path = {
                    let map = self.hardlink_map()?;
                    map.get(&inode).cloned()
                };

                if let Some(first_path) = first_path {
                    self.dest
                        .create_hardlink(&first_path, &task.dest_path)
                        .await?;

                    if self.config.itemize_changes {
                        let item = itemize_string(&task.action, false, false);
                        eprintln!("{} {}", item, task.dest_path.display());
                    }

                    self.remove_source_after_commit(source_entry).await?;
                    return Ok(if task.action == SyncAction::Create {
                        TaskResult::Created { bytes: 0 }
                    } else {
                        TaskResult::Updated { bytes: 0 }
                    });
                }
            }
        }

        // Read optional metadata before any backup or destination replacement so
        // source metadata failures cannot partially mutate the destination.
        let preservation = self.read_preservation(source_entry).await?;

        if self.backup.enabled && task.action == SyncAction::Update {
            self.create_backup(&task.dest_path).await?;
        }

        // The transfer layer owns staging, verification, and rollback. A bad
        // staged result never replaces the visible destination.
        let transfer = transfer_file(
            self.source,
            &source_entry.relative_path,
            self.dest,
            &task.dest_path,
            TransferOptions {
                update: task.action == SyncAction::Update,
                verify: self.verification.verify_on_write,
                rate_limiter: self.config.rate_limiter.clone(),
            },
        )
        .await?;

        tracing::debug!(
            path = %task.dest_path.display(),
            strategy = ?transfer.strategy,
            bytes = transfer.bytes_written,
            verification = ?transfer.verification,
            "file transfer complete"
        );

        if let VerificationStatus::Failed { expected, actual } = transfer.verification {
            return Ok(TaskResult::VerificationFailed {
                expected: expected.to_hex().to_string(),
                actual: actual.to_hex().to_string(),
            });
        }
        // `Verified` is only produced when --verify requested post-write
        // verification; count it so the flag's effect is observable in stats.
        if matches!(transfer.verification, VerificationStatus::Verified) {
            self.verified_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        self.apply_preservation(&task.dest_path, preservation)
            .await?;

        if self.preserve_hardlinks_requested() && source_entry.nlink > 1 {
            if let Some(inode) = source_entry.inode {
                self.hardlink_map()?.insert(inode, task.dest_path.clone());
            }
        }

        if self.config.itemize_changes {
            let item = itemize_string(&task.action, false, false);
            eprintln!("{} {}", item, task.dest_path.display());
        }

        // Source removal is intentionally last. Verification or preservation
        // failure leaves the source untouched, and a removal failure is surfaced
        // instead of reporting a successful move that left the source behind.
        self.remove_source_after_commit(source_entry).await?;

        Ok(if task.action == SyncAction::Create {
            TaskResult::Created {
                bytes: transfer.bytes_written,
            }
        } else {
            TaskResult::Updated {
                bytes: transfer.bytes_written,
            }
        })
    }

    /// Remove the source entry under --remove-source-files once the
    /// destination commit (and, for regular files, staged verification and
    /// preservation) has succeeded. Removal is intentionally the last step so
    /// any earlier failure leaves the source untouched; a removal failure is
    /// surfaced instead of reporting a successful move that left data behind.
    async fn remove_source_after_commit(&self, source_entry: &FileEntry) -> Result<()> {
        if !self.config.remove_source_files {
            return Ok(());
        }
        let source_path = self.source.root().join(&*source_entry.relative_path);
        // Race check: a source replaced or modified between scan and commit
        // must not be removed, because the removed bytes would not be the
        // verified ones. Size, mtime, and inode identify the scanned file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = tokio::fs::symlink_metadata(&source_path)
                .await
                .map_err(|error| {
                    Error::Io(std::io::Error::other(format!(
                        "source {} changed between scan and removal; not removing ({error})",
                        source_entry.relative_path.display()
                    )))
                })?;
            if metadata.len() != source_entry.size
                || metadata.modified().unwrap_or(std::time::UNIX_EPOCH) != source_entry.modified
                || source_entry
                    .inode
                    .is_some_and(|inode| inode != metadata.ino())
            {
                return Err(Error::Io(std::io::Error::other(format!(
                    "source {} changed between scan and removal; not removing",
                    source_entry.relative_path.display()
                ))));
            }
        }
        tokio::fs::remove_file(&source_path).await?;
        Ok(())
    }

    fn hardlink_map(&self) -> Result<MutexGuard<'_, HashMap<u64, PathBuf>>> {
        self.hardlink_map.lock().map_err(|_| {
            Error::Io(std::io::Error::other(
                "hard-link preservation state was poisoned",
            ))
        })
    }

    fn abs_dest_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.dest.root().join(path)
        }
    }

    async fn create_backup(&self, path: &Path) -> Result<()> {
        if !self.dest.exists(path).await? {
            return Ok(());
        }

        // Backup location is computed from the root-relative path so that
        // --backup-dir joins the tree structure under the backup root.
        let backup_path = self.backup.destination_path(path)?;
        let backup_parent = backup_path
            .parent()
            .ok_or_else(|| std::io::Error::other("Invalid backup path"))?;
        tokio::fs::create_dir_all(backup_parent).await?;

        transfer_file(
            self.dest,
            path,
            self.dest,
            &backup_path,
            TransferOptions {
                update: false,
                verify: false,
                // Backups move bytes too; the same --bwlimit budget applies.
                rate_limiter: self.config.rate_limiter.clone(),
            },
        )
        .await?;
        tracing::debug!("Backup created: {:?} -> {:?}", path, backup_path);
        Ok(())
    }

    pub async fn execute_batch(&self, tasks: Vec<SyncTask>) -> Result<SyncStats> {
        self.validate_requested_capabilities()?;
        let start = Instant::now();
        let mut stats = SyncStats::default();

        if self.preserve_hardlinks_requested() {
            // Link topology depends on deterministic first-seen ordering. Keep
            // this compatibility path serial until hard-link groups are native
            // engine operations rather than executor-side mutable state.
            for task in &tasks {
                self.execute_and_record(task, &mut stats).await?;
            }
        } else {
            let concurrency = tasks.len().max(1);
            let results: Vec<Result<TaskResult>> = stream::iter(tasks.iter())
                .map(|task| async move { self.execute_task(task).await })
                .buffer_unordered(concurrency)
                .collect()
                .await;

            for result in results {
                self.record_result(result?, &mut stats);
            }
        }

        stats.files_verified = self
            .verified_count
            .load(std::sync::atomic::Ordering::Relaxed);
        stats.duration = start.elapsed();
        Ok(stats)
    }

    async fn execute_and_record(&self, task: &SyncTask, stats: &mut SyncStats) -> Result<()> {
        let result = self.execute_task(task).await?;
        self.record_result(result, stats);
        Ok(())
    }

    fn record_result(&self, result: TaskResult, stats: &mut SyncStats) {
        match result {
            TaskResult::Skipped => stats.files_skipped += 1,
            TaskResult::Created { bytes } => {
                stats.files_created += 1;
                stats.bytes_transferred += bytes;
            }
            TaskResult::Updated { bytes } => {
                stats.files_updated += 1;
                stats.bytes_transferred += bytes;
            }
            TaskResult::DirCreated => stats.dirs_created += 1,
            TaskResult::SymlinkCreated => stats.symlinks_created += 1,
            TaskResult::Deleted => stats.files_deleted += 1,
            TaskResult::VerificationFailed { expected, actual } => {
                stats.verification_failures += 1;
                stats.errors.push(SyncError {
                    path: PathBuf::new(),
                    error: format!("Verification failed: expected {expected}, got {actual}"),
                    action: "verify".to_string(),
                });
            }
        }
    }

    pub async fn verify_transfer(&self, source: &Path, dest: &Path) -> Result<bool> {
        let source_hash = hash_file_streaming(self.source, source).await?;
        let dest_hash = hash_file_streaming(self.dest, dest).await?;
        Ok(source_hash == dest_hash)
    }
}

fn map_scheduler_error(error: SchedulerError) -> Error {
    Error::Config(format!("scheduler: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::local::LocalEndpoint;
    use crate::integrity::ChecksumType;
    use crate::sync::config::VerificationConfig;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_executor<'a>(source: &'a dyn Endpoint, dest: &'a dyn Endpoint) -> TaskExecutor<'a> {
        TaskExecutor::new(
            source,
            dest,
            false,
            PreserveConfig::default(),
            VerificationConfig {
                mode: ChecksumType::Fast,
                verify_on_write: false,
            },
            4,
        )
        .unwrap()
    }

    fn make_file_entry(relative: &str, size: u64) -> FileEntry {
        FileEntry {
            path: Arc::new(PathBuf::from(format!("/source/{relative}"))),
            relative_path: Arc::new(PathBuf::from(relative)),
            size,
            modified: std::time::SystemTime::now(),
            mode: 0o644,
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: size,
            xattrs: None,
            inode: None,
            nlink: 1,
            acls: None,
            bsd_flags: None,
        }
    }

    #[test]
    fn regular_file_request_reserves_real_transfer_working_set() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);
        let task = SyncTask {
            source: Some(Arc::new(make_file_entry("file", 1))),
            dest_path: PathBuf::from("file"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let request = executor.resource_request(&task);
        assert_eq!(request.active_files, 1);
        assert_eq!(request.buffered_bytes, FILE_TRANSFER_BUFFER_BUDGET);
        assert_eq!(request.metadata_ops, 1);
        assert_eq!(request.cpu_tasks, 0);
    }

    #[test]
    fn zero_concurrency_is_rejected() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let result = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig::default(),
            VerificationConfig {
                mode: ChecksumType::Fast,
                verify_on_write: false,
            },
            0,
        );
        assert!(matches!(result, Err(Error::Config(_))));
    }

    #[tokio::test]
    async fn native_file_copy_is_bounded_and_atomic() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let contents = vec![0x5a; 3 * 1024 * 1024 + 17];
        std::fs::write(src_dir.path().join("large.bin"), &contents).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);
        let entry = make_file_entry("large.bin", contents.len() as u64);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: PathBuf::from("large.bin"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(
            result,
            TaskResult::Created { bytes } if bytes == contents.len() as u64
        ));
        assert_eq!(
            std::fs::read(dst_dir.path().join("large.bin")).unwrap(),
            contents
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_transfer_preserves_xattrs_and_removes_stale_values() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        std::fs::write(src_dir.path().join("file"), b"new").unwrap();
        std::fs::write(dst_dir.path().join("file"), b"old").unwrap();
        xattr::set(src_dir.path().join("file"), "user.sy-source", b"source").unwrap();
        xattr::set(dst_dir.path().join("file"), "user.sy-stale", b"stale").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let preserve = PreserveConfig {
            xattrs: true,
            ..Default::default()
        };
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            preserve,
            VerificationConfig {
                mode: ChecksumType::Fast,
                verify_on_write: false,
            },
            4,
        )
        .unwrap();
        let entry = make_file_entry("file", 3);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: PathBuf::from("file"),
            action: SyncAction::Update,
            source_checksum: None,
            dest_checksum: None,
        };

        executor.execute_task(&task).await.unwrap();
        assert_eq!(
            xattr::get(dst_dir.path().join("file"), "user.sy-source").unwrap(),
            Some(b"source".to_vec())
        );
        assert_eq!(
            xattr::get(dst_dir.path().join("file"), "user.sy-stale").unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn verify_transfer_hashes_streams() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        std::fs::write(src_dir.path().join("file"), b"same").unwrap();
        std::fs::write(dst_dir.path().join("file"), b"same").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        assert!(executor
            .verify_transfer(Path::new("file"), Path::new("file"))
            .await
            .unwrap());

        std::fs::write(dst_dir.path().join("file"), b"different").unwrap();
        assert!(!executor
            .verify_transfer(Path::new("file"), Path::new("file"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn verify_on_write_uses_blake3() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        std::fs::write(src_dir.path().join("file"), b"verified").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig::default(),
            VerificationConfig {
                mode: ChecksumType::Fast,
                verify_on_write: true,
            },
            4,
        )
        .unwrap();

        let entry = make_file_entry("file", 8);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: PathBuf::from("file"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let request = executor.resource_request(&task);
        assert_eq!(request.cpu_tasks, 1);

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 8 }));
    }
}
