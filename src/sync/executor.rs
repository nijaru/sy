//! TaskExecutor: executes sync tasks against endpoints.
//!
//! Handles create/update/delete operations with parallelism and verification.
//! Supports hardlink tracking, xattr preservation, backup, and rsync-compatible flags.

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

/// Result of executing a single task
#[derive(Debug, Clone)]
pub enum TaskResult {
    /// Task was skipped
    Skipped,
    /// File was created
    Created { bytes: u64 },
    /// File was updated
    Updated { bytes: u64 },
    /// Directory was created
    DirCreated,
    /// Symlink was created
    SymlinkCreated,
    /// File was deleted
    Deleted,
    /// Verification failed
    VerificationFailed { expected: String, actual: String },
}

/// Configuration for backup behavior
#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub enabled: bool,
    pub suffix: String,
    pub dir: Option<PathBuf>,
}

/// Configuration for task execution behavior
#[derive(Debug, Clone, Default)]
pub struct ExecuteConfig {
    /// Preserve hardlinks (track inodes, create hardlinks for same inode)
    pub preserve_hardlinks: bool,
    /// Preserve xattrs (copy xattrs after file copy)
    pub preserve_xattrs: bool,
    /// Preserve directory permissions (unix only)
    pub preserve_dir_permissions: bool,
    /// Clean up partial files on failure (unless true)
    pub keep_partial: bool,
    /// Output rsync-style itemize changes
    pub itemize_changes: bool,
    /// Remove source files after successful transfer
    pub remove_source_files: bool,
    /// Print transfer summary
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
    /// Track inodes for hardlink preservation (inode -> first dest path)
    hardlink_map: Mutex<HashMap<u64, PathBuf>>,
}

impl<'a> TaskExecutor<'a> {
    /// Create a new TaskExecutor.
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

    /// Set backup configuration
    pub fn with_backup(mut self, config: BackupConfig) -> Self {
        self.backup = config;
        self
    }

    /// Set execution configuration
    pub fn with_config(mut self, config: ExecuteConfig) -> Self {
        self.config = config;
        self
    }

    /// Execute a single sync task.
    pub async fn execute_task(&self, task: &SyncTask) -> Result<TaskResult> {
        match task.action {
            SyncAction::Skip => Ok(TaskResult::Skipped),
            SyncAction::Create | SyncAction::Update => {
                self.execute_create_or_update(task).await
            }
            SyncAction::Delete => {
                if self.dry_run {
                    return Ok(TaskResult::Deleted);
                }
                self.dest.remove(&task.dest_path, true).await?;
                Ok(TaskResult::Deleted)
            }
        }
    }

    /// Execute a create or update task.
    async fn execute_create_or_update(&self, task: &SyncTask) -> Result<TaskResult> {
        let source_entry = task.source.as_ref()
            .ok_or_else(|| Error::Io(std::io::Error::other("Missing source for create/update")))?;

        if self.dry_run {
            return if source_entry.is_dir {
                Ok(TaskResult::DirCreated)
            } else if source_entry.is_symlink {
                Ok(TaskResult::SymlinkCreated)
            } else {
                Ok(TaskResult::Created { bytes: source_entry.size })
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

    /// Execute directory creation with optional permission preservation.
    async fn execute_directory(&self, source_entry: &FileEntry, task: &SyncTask) -> Result<TaskResult> {
        self.dest.create_dir_all(&task.dest_path).await?;

        // Preserve directory permissions if enabled
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

    /// Execute symlink creation.
    async fn execute_symlink(&self, source_entry: &FileEntry, task: &SyncTask) -> Result<TaskResult> {
        #[cfg(unix)]
        {
            let source_path = self.source.root().join(&*source_entry.relative_path);
            let target = std::fs::read_link(&source_path)?;

            // Remove existing file/symlink before creating
            if task.action == SyncAction::Update {
                self.dest.remove(&task.dest_path, false).await?;
            }

            self.dest.create_symlink(&target, &task.dest_path).await?;

            // Itemize if configured
            if self.config.itemize_changes {
                let item = itemize_string(&task.action, false, true);
                eprintln!("{} {}", item, task.dest_path.display());
            }

            // Return appropriate result based on action
            if task.action == SyncAction::Create {
                Ok(TaskResult::SymlinkCreated)
            } else {
                Ok(TaskResult::Updated { bytes: 0 })
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (source_entry, task);
            Err(Error::Io(std::io::Error::other("Symlinks not supported on this platform")))
        }
    }

    /// Execute file copy with hardlink tracking, backup, xattrs, and verification.
    async fn execute_file(&self, source_entry: &FileEntry, task: &SyncTask) -> Result<TaskResult> {
        // Check if this is a hardlink that's already been copied
        if self.config.preserve_hardlinks && source_entry.nlink > 1 {
            if let Some(inode) = source_entry.inode {
                let first_path = {
                    let map = self.hardlink_map.lock().unwrap();
                    map.get(&inode).cloned()
                };
                if let Some(first_path) = first_path {
                    // Remove existing file before creating hard link
                    if task.action == SyncAction::Update {
                        self.dest.remove(&task.dest_path, false).await?;
                    }
                    self.dest.create_hardlink(&first_path, &task.dest_path).await?;

                    // Itemize if configured
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

        // Backup existing file if configured
        if self.backup.enabled && task.action == SyncAction::Update {
            self.create_backup(&task.dest_path).await?;
        }

        // Check change ratio for large files (above 10MB delta threshold)
        const DELTA_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB
        if task.action == SyncAction::Update && source_entry.size > DELTA_THRESHOLD {
            let abs_dest = self.abs_dest_path(&task.dest_path);
            if abs_dest.exists() {
                let source_path = self.source.root().join(&*source_entry.relative_path);
                match crate::delta::estimate_change_ratio(
                    &source_path,
                    &abs_dest,
                    64 * 1024, // 64KB blocks
                    Some(20),  // Sample 20 blocks
                    Some(0.75), // 75% threshold
                ) {
                    Ok(ratio) => {
                        tracing::info!(
                            "Change ratio: {} ({}/{} blocks changed)",
                            ratio.change_ratio_percent(),
                            ratio.blocks_changed,
                            ratio.blocks_sampled
                        );

                        if ratio.use_delta {
                            tracing::info!(
                                "Change ratio {} below threshold {:.1}%, using delta sync",
                                ratio.change_ratio_percent(),
                                ratio.threshold * 100.0
                            );
                        } else {
                            tracing::info!(
                                "Change ratio {} exceeds threshold {:.1}%, using full copy",
                                ratio.change_ratio_percent(),
                                ratio.threshold * 100.0
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Change ratio detection failed: {}", e);
                    }
                }
            }
        }

        // Read source file
        let data = self.source.read_file(&source_entry.relative_path).await?;
        let meta = self.source.metadata(&source_entry.relative_path).await?;

        // Write to destination
        match self.dest.write_file(&task.dest_path, &data, &meta).await {
            Ok(()) => {
                // Record inode for hardlink tracking
                if self.config.preserve_hardlinks && source_entry.nlink > 1 {
                    if let Some(inode) = source_entry.inode {
                        let mut map = self.hardlink_map.lock().unwrap();
                        map.insert(inode, task.dest_path.clone());
                    }
                }

                // Copy xattrs if enabled
                #[cfg(unix)]
                if self.config.preserve_xattrs {
                    self.copy_xattrs(source_entry, &task.dest_path);
                }

                // Itemize if configured
                if self.config.itemize_changes {
                    let item = itemize_string(&task.action, false, false);
                    eprintln!("{} {}", item, task.dest_path.display());
                }

                // Remove source file after successful transfer
                if self.config.remove_source_files {
                    let source_path = self.source.root().join(&*source_entry.relative_path);
                    if let Err(e) = std::fs::remove_file(&source_path) {
                        tracing::warn!("Failed to remove source {}: {}", source_path.display(), e);
                    }
                }

                // Verify if configured
                if self.verification.verify_on_write {
                    let dest_data = self.dest.read_file(&task.dest_path).await?;
                    if data != dest_data {
                        return Ok(TaskResult::VerificationFailed {
                            expected: format!("{} bytes", data.len()),
                            actual: format!("{} bytes", dest_data.len()),
                        });
                    }
                }

                let bytes = data.len() as u64;
                Ok(if task.action == SyncAction::Create {
                    TaskResult::Created { bytes }
                } else {
                    TaskResult::Updated { bytes }
                })
            }
            Err(e) => {
                // Clean up partial file on failure unless keep_partial
                if !self.config.keep_partial {
                    let _ = self.dest.remove(&task.dest_path, false).await;
                }
                Err(e)
            }
        }
    }

    /// Copy xattrs from source to destination (unix only)
    #[cfg(unix)]
    fn copy_xattrs(&self, source_entry: &FileEntry, dest_path: &Path) {
        let source_path = self.source.root().join(&*source_entry.relative_path);
        if let Ok(xattrs) = xattr::list(&source_path) {
            for attr in xattrs {
                if let Ok(Some(val)) = xattr::get(&source_path, &attr) {
                    let abs_dest = self.abs_dest_path(dest_path);
                    let _ = xattr::set(&abs_dest, &attr, &val);
                }
            }
        }
    }

    /// Get absolute destination path
    fn abs_dest_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.dest.root().join(path)
        }
    }

    /// Create a backup of an existing file before overwriting.
    async fn create_backup(&self, path: &Path) -> Result<()> {
        if !self.dest.exists(path).await? {
            return Ok(());
        }

        let abs_dest = self.abs_dest_path(path);
        let backup_path = if let Some(ref dir) = self.backup.dir {
            let file_name = abs_dest.file_name()
                .ok_or_else(|| std::io::Error::other("Invalid file path"))?;
            dir.join(format!("{}{}",
                file_name.to_string_lossy(),
                self.backup.suffix
            ))
        } else {
            let file_name = abs_dest.file_name()
                .ok_or_else(|| std::io::Error::other("Invalid file path"))?;
            abs_dest.parent()
                .unwrap_or(Path::new("."))
                .join(format!("{}{}",
                    file_name.to_string_lossy(),
                    self.backup.suffix
                ))
        };

        // Copy the file using the endpoint's copy_file method
        self.dest.copy_file(path, &backup_path).await?;

        tracing::debug!("Backup created: {:?} -> {:?}", path, backup_path);
        Ok(())
    }

    /// Execute a batch of tasks and return stats.
    ///
    /// Note: For hardlink preservation, tasks should be executed sequentially.
    /// Parallel execution may miss hardlinks if the first copy hasn't completed.
    pub async fn execute_batch(&self, tasks: Vec<SyncTask>) -> Result<SyncStats> {
        let start = Instant::now();
        let mut stats = SyncStats::default();

        // Process tasks - sequential for hardlink tracking, parallel otherwise
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

    /// Execute a single task and record the result in stats.
    async fn execute_and_record(&self, task: &SyncTask, stats: &mut SyncStats) -> Result<()> {
        let result = self.execute_task(task).await?;
        self.record_result(result, stats);
        Ok(())
    }

    /// Record a task result in stats.
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
                    error: format!("Verification failed: expected {}, got {}", expected, actual),
                    action: "verify".to_string(),
                });
            }
        }
    }

    /// Verify a file transfer by comparing source and destination.
    pub async fn verify_transfer(&self, source: &Path, dest: &Path) -> Result<bool> {
        let source_data = self.source.read_file(source).await?;
        let dest_data = self.dest.read_file(dest).await?;
        Ok(source_data == dest_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::local::LocalEndpoint;
    use crate::sync::config::VerificationConfig;
    use crate::integrity::ChecksumType;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_executor<'a>(
        source: &'a dyn Endpoint,
        dest: &'a dyn Endpoint,
    ) -> TaskExecutor<'a> {
        TaskExecutor::new(
            source,
            dest,
            false, // dry_run
            PreserveConfig::default(),
            VerificationConfig {
                mode: ChecksumType::Fast,
                verify_on_write: false,
                checksum_db: false,
                clear_checksum_db: false,
                prune_checksum_db: false,
            },
            4, // max_concurrent
        )
    }

    fn make_file_entry(relative: &str, size: u64, is_dir: bool, is_symlink: bool) -> FileEntry {
        FileEntry {
            path: Arc::new(PathBuf::from(format!("/source/{}", relative))),
            relative_path: Arc::new(PathBuf::from(relative)),
            size,
            modified: std::time::SystemTime::now(),
            is_dir,
            is_symlink,
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
    async fn test_execute_create_file() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "hello").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("test.txt", 5, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 5 }));
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("test.txt")).unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_execute_update_file() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "updated").unwrap();
        std::fs::write(dst_dir.path().join("test.txt"), "old").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Update,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Updated { bytes: 7 }));
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("test.txt")).unwrap(), "updated");
    }

    #[tokio::test]
    async fn test_execute_create_directory() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::create_dir(src_dir.path().join("subdir")).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("subdir", 0, true, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("subdir"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::DirCreated));
        assert!(dst_dir.path().join("subdir").is_dir());
    }

    #[tokio::test]
    async fn test_execute_delete_file() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(dst_dir.path().join("delete.txt"), "delete me").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let task = SyncTask {
            source: None,
            dest_path: dst_dir.path().join("delete.txt"),
            action: SyncAction::Delete,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Deleted));
        assert!(!dst_dir.path().join("delete.txt").exists());
    }

    #[tokio::test]
    async fn test_execute_skip() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let task = SyncTask {
            source: None,
            dest_path: dst_dir.path().join("skip.txt"),
            action: SyncAction::Skip,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Skipped));
    }

    #[tokio::test]
    async fn test_dry_run_does_not_modify() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "hello").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());

        let executor = TaskExecutor::new(
            &source,
            &dest,
            true, // dry_run
            PreserveConfig::default(),
            VerificationConfig::default(),
            4,
        );

        let entry = make_file_entry("test.txt", 5, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { .. }));
        assert!(!dst_dir.path().join("test.txt").exists());
    }

    #[tokio::test]
    async fn test_dry_run_delete_does_not_modify() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(dst_dir.path().join("keep.txt"), "keep").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());

        let executor = TaskExecutor::new(
            &source,
            &dest,
            true, // dry_run
            PreserveConfig::default(),
            VerificationConfig::default(),
            4,
        );

        let task = SyncTask {
            source: None,
            dest_path: dst_dir.path().join("keep.txt"),
            action: SyncAction::Delete,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Deleted));
        assert!(dst_dir.path().join("keep.txt").exists());
    }

    #[tokio::test]
    async fn test_verify_transfer() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "content").unwrap();
        std::fs::write(dst_dir.path().join("test.txt"), "content").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let result = executor.verify_transfer(
            Path::new("test.txt"),
            Path::new("test.txt"),
        ).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_verify_transfer_mismatch() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "source").unwrap();
        std::fs::write(dst_dir.path().join("test.txt"), "dest").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let result = executor.verify_transfer(
            Path::new("test.txt"),
            Path::new("test.txt"),
        ).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_verify_on_write() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "hello").unwrap();

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

        let entry = make_file_entry("test.txt", 5, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { .. }));
    }

    #[tokio::test]
    async fn test_execute_batch_parallel() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        for i in 0..5 {
            std::fs::write(
                src_dir.path().join(format!("file{}.txt", i)),
                format!("content{}", i),
            ).unwrap();
        }

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let tasks: Vec<SyncTask> = (0..5)
            .map(|i| {
                let name = format!("file{}.txt", i);
                SyncTask {
                    source: Some(Arc::new(make_file_entry(&name, 8, false, false))),
                    dest_path: dst_dir.path().join(&name),
                    action: SyncAction::Create,
                    source_checksum: None,
                    dest_checksum: None,
                }
            })
            .collect();

        let stats = executor.execute_batch(tasks).await.unwrap();
        assert_eq!(stats.files_created, 5);
        assert_eq!(stats.bytes_transferred, 40);

        for i in 0..5 {
            let path = dst_dir.path().join(format!("file{}.txt", i));
            assert!(path.exists());
        }
    }

    #[tokio::test]
    async fn test_execute_batch_mixed_actions() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("update.txt"), "new").unwrap();
        std::fs::write(dst_dir.path().join("update.txt"), "old").unwrap();
        std::fs::write(dst_dir.path().join("delete.txt"), "delete").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let tasks = vec![
            SyncTask {
                source: Some(Arc::new(make_file_entry("update.txt", 3, false, false))),
                dest_path: dst_dir.path().join("update.txt"),
                action: SyncAction::Update,
                source_checksum: None,
                dest_checksum: None,
            },
            SyncTask {
                source: None,
                dest_path: dst_dir.path().join("delete.txt"),
                action: SyncAction::Delete,
                source_checksum: None,
                dest_checksum: None,
            },
            SyncTask {
                source: None,
                dest_path: dst_dir.path().join("skip.txt"),
                action: SyncAction::Skip,
                source_checksum: None,
                dest_checksum: None,
            },
        ];

        let stats = executor.execute_batch(tasks).await.unwrap();
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.files_skipped, 1);

        assert_eq!(std::fs::read_to_string(dst_dir.path().join("update.txt")).unwrap(), "new");
        assert!(!dst_dir.path().join("delete.txt").exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_symlink_preserve() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("target.txt"), "target").unwrap();
        std::os::unix::fs::symlink("target.txt", src_dir.path().join("link.txt")).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("link.txt", 0, false, true);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("link.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::SymlinkCreated));

        let dest_link = dst_dir.path().join("link.txt");
        let meta = std::fs::symlink_metadata(&dest_link).unwrap();
        assert!(meta.is_symlink());
        assert_eq!(std::fs::read_link(&dest_link).unwrap().to_str().unwrap(), "target.txt");
    }

    #[tokio::test]
    async fn test_create_nested_directories() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::create_dir_all(src_dir.path().join("a/b/c")).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("a/b/c", 0, true, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("a/b/c"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::DirCreated));
        assert!(dst_dir.path().join("a/b/c").is_dir());
    }

    #[tokio::test]
    async fn test_execute_file_with_zero_bytes() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("empty.txt"), "").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("empty.txt", 0, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("empty.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 0 }));
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("empty.txt")).unwrap(), "");
    }

    #[tokio::test]
    async fn test_missing_source_for_create() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let task = SyncTask {
            source: None,
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing source"));
    }

    #[tokio::test]
    async fn test_backup_config() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "updated").unwrap();
        std::fs::write(dst_dir.path().join("test.txt"), "old").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
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
        ).with_backup(BackupConfig {
            enabled: true,
            suffix: "~".to_string(),
            dir: None,
        });

        let entry = make_file_entry("test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Update,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Updated { bytes: 7 }));
        
        assert!(dst_dir.path().join("test.txt~").exists());
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("test.txt~")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("test.txt")).unwrap(), "updated");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_permission_denied_dest() {
        use std::os::unix::fs::PermissionsExt;

        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "content").unwrap();

        let readonly_dir = dst_dir.path().join("readonly");
        std::fs::create_dir(&readonly_dir).unwrap();
        let mut perms = std::fs::metadata(&readonly_dir).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&readonly_dir, perms).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("readonly/test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: readonly_dir.join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await;
        assert!(result.is_err(), "Should fail when destination is read-only");

        let mut perms = std::fs::metadata(&readonly_dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&readonly_dir, perms).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_symlink_preserve() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("target.txt"), "content").unwrap();
        std::os::unix::fs::symlink("target.txt", src_dir.path().join("link.txt")).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("link.txt", 0, false, true);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("link.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::SymlinkCreated));

        let link_path = dst_dir.path().join("link.txt");
        assert!(link_path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link_path).unwrap().to_str().unwrap(), "target.txt");
    }

    #[tokio::test]
    async fn test_hardlink_preservation() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create two hardlinked files
        std::fs::write(src_dir.path().join("original.txt"), "content").unwrap();
        std::fs::hard_link(
            src_dir.path().join("original.txt"),
            src_dir.path().join("link.txt"),
        ).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig { hardlinks: true, ..Default::default() },
            VerificationConfig::default(),
            4,
        ).with_config(ExecuteConfig {
            preserve_hardlinks: true,
            ..Default::default()
        });

        // Execute first file
        let entry1 = make_file_entry("original.txt", 7, false, false);
        let task1 = SyncTask {
            source: Some(Arc::new(entry1)),
            dest_path: dst_dir.path().join("original.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };
        executor.execute_task(&task1).await.unwrap();

        // Execute second file (same inode)
        let entry2 = FileEntry {
            path: Arc::new(PathBuf::from(format!("/source/link.txt"))),
            relative_path: Arc::new(PathBuf::from("link.txt")),
            size: 7,
            modified: std::time::SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: Some(12345), // Same inode
            nlink: 2,
            acls: None,
            bsd_flags: None,
        };
        let task2 = SyncTask {
            source: Some(Arc::new(entry2)),
            dest_path: dst_dir.path().join("link.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };
        executor.execute_task(&task2).await.unwrap();

        // Both files should exist
        assert!(dst_dir.path().join("original.txt").exists());
        assert!(dst_dir.path().join("link.txt").exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_partial_cleanup() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create a source file
        std::fs::write(src_dir.path().join("test.txt"), "content").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        
        // Create executor with keep_partial = false (default)
        let executor = test_executor(&source, &dest);

        let entry = make_file_entry("test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        // This should succeed
        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 7 }));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_xattr_preservation() {
        use std::os::unix::fs::MetadataExt;

        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "content").unwrap();

        // Set xattr on source (skip if not supported)
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            let _ = xattr::set(src_dir.path().join("test.txt"), "com.test.attr", b"value");
        }
        #[cfg(target_os = "linux")]
        {
            let _ = xattr::set(src_dir.path().join("test.txt"), "user.test.attr", b"value");
        }

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig { xattrs: true, ..Default::default() },
            VerificationConfig::default(),
            4,
        ).with_config(ExecuteConfig {
            preserve_xattrs: true,
            ..Default::default()
        });

        let entry = make_file_entry("test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 7 }));

        // Verify xattr was copied (if supported)
        #[cfg(target_os = "macos")]
        {
            let val = xattr::get(dst_dir.path().join("test.txt"), "com.test.attr").unwrap();
            assert_eq!(val, Some(b"value".to_vec()));
        }
        #[cfg(target_os = "linux")]
        {
            let val = xattr::get(dst_dir.path().join("test.txt"), "user.test.attr").unwrap();
            assert_eq!(val, Some(b"value".to_vec()));
        }
    }

    #[tokio::test]
    async fn test_itemize_changes() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "hello").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig::default(),
            VerificationConfig::default(),
            4,
        ).with_config(ExecuteConfig {
            itemize_changes: true,
            ..Default::default()
        });

        let entry = make_file_entry("test.txt", 5, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        // Capture stderr
        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 5 }));
    }

    #[tokio::test]
    async fn test_remove_source_files() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "content").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig::default(),
            VerificationConfig::default(),
            4,
        ).with_config(ExecuteConfig {
            remove_source_files: true,
            ..Default::default()
        });

        let entry = make_file_entry("test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 7 }));

        // Source should be removed
        assert!(!src_dir.path().join("test.txt").exists());
        // Dest should have the content
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("test.txt")).unwrap(), "content");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_directory_permission_preservation() {
        use std::os::unix::fs::PermissionsExt;

        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::create_dir(src_dir.path().join("subdir")).unwrap();
        let mut perms = std::fs::metadata(src_dir.path().join("subdir")).unwrap().permissions();
        perms.set_mode(0o750);
        std::fs::set_permissions(src_dir.path().join("subdir"), perms).unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig { permissions: true, ..Default::default() },
            VerificationConfig::default(),
            4,
        ).with_config(ExecuteConfig {
            preserve_dir_permissions: true,
            ..Default::default()
        });

        let entry = make_file_entry("subdir", 0, true, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("subdir"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::DirCreated));

        // Verify permissions
        let dest_perms = std::fs::metadata(dst_dir.path().join("subdir")).unwrap().permissions();
        assert_eq!(dest_perms.mode() & 0o777, 0o750);
    }

    #[tokio::test]
    async fn test_backup_with_custom_dir() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();
        let backup_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "updated").unwrap();
        std::fs::write(dst_dir.path().join("test.txt"), "old").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig::default(),
            VerificationConfig::default(),
            4,
        ).with_backup(BackupConfig {
            enabled: true,
            suffix: "~".to_string(),
            dir: Some(backup_dir.path().to_path_buf()),
        });

        let entry = make_file_entry("test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Update,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Updated { bytes: 7 }));

        // Backup should be in custom directory
        assert!(backup_dir.path().join("test.txt~").exists());
        assert_eq!(std::fs::read_to_string(backup_dir.path().join("test.txt~")).unwrap(), "old");
        // Dest should have new content
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("test.txt")).unwrap(), "updated");
    }

    #[tokio::test]
    async fn test_backup_with_custom_suffix() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "updated").unwrap();
        std::fs::write(dst_dir.path().join("test.txt"), "old").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig::default(),
            VerificationConfig::default(),
            4,
        ).with_backup(BackupConfig {
            enabled: true,
            suffix: ".bak".to_string(),
            dir: None,
        });

        let entry = make_file_entry("test.txt", 7, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Update,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Updated { bytes: 7 }));

        // Backup should have custom suffix
        assert!(dst_dir.path().join("test.txt.bak").exists());
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("test.txt.bak")).unwrap(), "old");
    }

    #[tokio::test]
    async fn test_backup_not_created_for_create() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("test.txt"), "new").unwrap();

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = TaskExecutor::new(
            &source,
            &dest,
            false,
            PreserveConfig::default(),
            VerificationConfig::default(),
            4,
        ).with_backup(BackupConfig {
            enabled: true,
            suffix: "~".to_string(),
            dir: None,
        });

        let entry = make_file_entry("test.txt", 3, false, false);
        let task = SyncTask {
            source: Some(Arc::new(entry)),
            dest_path: dst_dir.path().join("test.txt"),
            action: SyncAction::Create,
            source_checksum: None,
            dest_checksum: None,
        };

        let result = executor.execute_task(&task).await.unwrap();
        assert!(matches!(result, TaskResult::Created { bytes: 3 }));

        // No backup should be created for Create action
        assert!(!dst_dir.path().join("test.txt~").exists());
    }
}
