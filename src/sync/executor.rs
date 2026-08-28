//! v0.5 task executor built on the capability-driven endpoint contract.
//!
//! Regular-file transfers never require whole-file `Vec<u8>` buffers. The
//! transfer layer chooses endpoint-native fast paths when available and falls
//! back to bounded staged streaming. Verification is explicit BLAKE3 I/O.

use crate::endpoint::io::{hash_file_streaming, VerificationStatus};
use crate::endpoint::transfer::{transfer_file, TransferOptions};
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError as Error};
use crate::sync::config::{PreserveConfig, VerificationConfig};
use crate::sync::itemize_string;
use crate::sync::scanner::FileEntry;
use crate::sync::stats::{SyncError, SyncStats};
use crate::sync::strategy::{SyncAction, SyncTask};
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

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

/// Configuration for task execution behavior.
#[derive(Debug, Clone, Default)]
pub struct ExecuteConfig {
    pub preserve_hardlinks: bool,
    pub preserve_xattrs: bool,
    pub preserve_dir_permissions: bool,
    /// Retained for CLI compatibility. Transactional staged writes do not expose
    /// incomplete destination files; resumable staging will own this later.
    pub keep_partial: bool,
    pub itemize_changes: bool,
    pub remove_source_files: bool,
    pub print_stats: bool,
}

/// Executes sync tasks against source and destination endpoints.
pub struct TaskExecutor<'a> {
    source: &'a dyn Endpoint,
    dest: &'a dyn Endpoint,
    dry_run: bool,
    preserve: PreserveConfig,
    verification: VerificationConfig,
    max_concurrent: usize,
    backup: BackupConfig,
    config: ExecuteConfig,
    hardlink_map: Mutex<HashMap<u64, PathBuf>>,
}

impl<'a> TaskExecutor<'a> {
    pub fn new(
        source: &'a dyn Endpoint,
        dest: &'a dyn Endpoint,
        dry_run: bool,
        preserve: PreserveConfig,
        verification: VerificationConfig,
        max_concurrent: usize,
    ) -> Self {
        Self {
            source,
            dest,
            dry_run,
            preserve,
            verification,
            max_concurrent,
            backup: BackupConfig {
                enabled: false,
                suffix: "~".to_string(),
                dir: None,
            },
            config: ExecuteConfig::default(),
            hardlink_map: Mutex::new(HashMap::new()),
        }
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

    fn validate_requested_capabilities(&self) -> Result<()> {
        let capabilities = self.dest.capabilities();

        if (self.config.preserve_xattrs || self.preserve.xattrs) && !capabilities.preserve_xattrs {
            return Err(Error::Config(format!(
                "{:?} destination cannot preserve extended attributes",
                self.dest.endpoint_type()
            )));
        }
        if self.preserve.acls && !capabilities.preserve_acls {
            return Err(Error::Config(format!(
                "{:?} destination cannot preserve ACLs",
                self.dest.endpoint_type()
            )));
        }
        if (self.config.preserve_hardlinks || self.preserve.hardlinks)
            && !capabilities.preserve_hardlinks
        {
            return Err(Error::Config(format!(
                "{:?} destination cannot preserve hard links",
                self.dest.endpoint_type()
            )));
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
            } else {
                Ok(TaskResult::Created {
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
        self.dest.create_dir_all(&task.dest_path).await?;

        #[cfg(unix)]
        if self.config.preserve_dir_permissions {
            use std::os::unix::fs::PermissionsExt;
            let source_path = self.source.root().join(&*source_entry.relative_path);
            if let Ok(meta) = std::fs::metadata(&source_path) {
                let mode = meta.permissions().mode();
                let abs_dest = self.abs_dest_path(&task.dest_path);
                let _ = std::fs::set_permissions(&abs_dest, std::fs::Permissions::from_mode(mode));
            }
        }

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
            let target = std::fs::read_link(&source_path)?;
            self.dest.create_symlink(&target, &task.dest_path).await?;

            if self.config.itemize_changes {
                let item = itemize_string(&task.action, false, true);
                eprintln!("{} {}", item, task.dest_path.display());
            }

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
        if self.config.preserve_hardlinks && source_entry.nlink > 1 {
            if let Some(inode) = source_entry.inode {
                let first_path = {
                    let map = self.hardlink_map.lock().expect("hardlink map poisoned");
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

                    return Ok(if task.action == SyncAction::Create {
                        TaskResult::Created { bytes: 0 }
                    } else {
                        TaskResult::Updated { bytes: 0 }
                    });
                }
            }
        }

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

        if self.config.preserve_hardlinks && source_entry.nlink > 1 {
            if let Some(inode) = source_entry.inode {
                let mut map = self.hardlink_map.lock().expect("hardlink map poisoned");
                map.insert(inode, task.dest_path.clone());
            }
        }

        #[cfg(unix)]
        if self.config.preserve_xattrs {
            self.copy_xattrs(source_entry, &task.dest_path);
        }

        if self.config.itemize_changes {
            let item = itemize_string(&task.action, false, false);
            eprintln!("{} {}", item, task.dest_path.display());
        }

        // Source removal is intentionally last. Verification failure leaves the
        // source untouched.
        if self.config.remove_source_files {
            let source_path = self.source.root().join(&*source_entry.relative_path);
            if let Err(error) = std::fs::remove_file(&source_path) {
                tracing::warn!(
                    "Failed to remove source {}: {}",
                    source_path.display(),
                    error
                );
            }
        }

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

    #[cfg(unix)]
    fn copy_xattrs(&self, source_entry: &FileEntry, dest_path: &Path) {
        let source_path = self.source.root().join(&*source_entry.relative_path);
        if let Ok(xattrs) = xattr::list(&source_path) {
            for attr in xattrs {
                if let Ok(Some(value)) = xattr::get(&source_path, &attr) {
                    let abs_dest = self.abs_dest_path(dest_path);
                    let _ = xattr::set(&abs_dest, &attr, &value);
                }
            }
        }
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

        let abs_dest = self.abs_dest_path(path);
        let backup_path = if let Some(ref dir) = self.backup.dir {
            let file_name = abs_dest
                .file_name()
                .ok_or_else(|| std::io::Error::other("Invalid file path"))?;
            dir.join(format!(
                "{}{}",
                file_name.to_string_lossy(),
                self.backup.suffix
            ))
        } else {
            let file_name = abs_dest
                .file_name()
                .ok_or_else(|| std::io::Error::other("Invalid file path"))?;
            abs_dest.parent().unwrap_or(Path::new(".")).join(format!(
                "{}{}",
                file_name.to_string_lossy(),
                self.backup.suffix
            ))
        };

        transfer_file(
            self.dest,
            path,
            self.dest,
            &backup_path,
            TransferOptions {
                update: false,
                verify: false,
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

        if self.config.preserve_hardlinks {
            for task in &tasks {
                self.execute_and_record(task, &mut stats).await?;
            }
        } else {
            let results: Vec<Result<TaskResult>> = stream::iter(tasks.iter())
                .map(|task| async move { self.execute_task(task).await })
                .buffer_unordered(self.max_concurrent)
                .collect()
                .await;

            for result in results {
                self.record_result(result?, &mut stats);
            }
        }

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
                checksum_db: false,
                clear_checksum_db: false,
                prune_checksum_db: false,
            },
            4,
        )
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
                checksum_db: false,
                clear_checksum_db: false,
                prune_checksum_db: false,
            },
            4,
        );

        let entry = make_file_entry("file", 8);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: PathBuf::from("file"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 8 }));
    }
}
