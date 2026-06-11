//! TaskExecutor: executes sync tasks against endpoints.
//!
//! Handles create/update/delete operations with parallelism and verification.

use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError as Error};
use crate::sync::config::{PreserveConfig, VerificationConfig};
use crate::sync::scanner::FileEntry;
use crate::sync::stats::{SyncError, SyncStats};
use crate::sync::strategy::{SyncAction, SyncTask};
use futures::stream::{self, StreamExt};
use std::path::{Path, PathBuf};
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

/// Executes sync tasks against source and destination endpoints.
#[allow(dead_code)] // Wired in by main.rs rewrite (Phase 4 completion)
pub struct TaskExecutor<'a> {
    source: &'a dyn Endpoint,
    dest: &'a dyn Endpoint,
    dry_run: bool,
    #[allow(dead_code)] // Will be used for xattr/hardlink/acl preservation
    preserve: PreserveConfig,
    verification: VerificationConfig,
    max_concurrent: usize,
}

#[allow(dead_code)] // Wired in by main.rs rewrite (Phase 4 completion)
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
        }
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
            self.dest.create_dir_all(&task.dest_path).await?;
            Ok(TaskResult::DirCreated)
        } else if source_entry.is_symlink {
            self.execute_symlink(source_entry, task).await
        } else {
            self.execute_file_copy(source_entry, task).await
        }
    }

    /// Execute symlink creation based on preserve config.
    async fn execute_symlink(&self, source_entry: &FileEntry, task: &SyncTask) -> Result<TaskResult> {
        #[cfg(unix)]
        {
            let source_path = self.source.root().join(&*source_entry.relative_path);
            let target = std::fs::read_link(&source_path)?;
            self.dest.create_symlink(&target, &task.dest_path).await?;
            Ok(TaskResult::SymlinkCreated)
        }
        #[cfg(not(unix))]
        {
            let _ = (source_entry, task);
            Err(SyncError::Io(std::io::Error::other("Symlinks not supported on this platform")))
        }
    }

    /// Execute file copy with optional verification.
    async fn execute_file_copy(&self, source_entry: &FileEntry, task: &SyncTask) -> Result<TaskResult> {
        // Read source file
        let data = self.source.read_file(&source_entry.relative_path).await?;
        let meta = self.source.metadata(&source_entry.relative_path).await?;

        // Write to destination
        self.dest.write_file(&task.dest_path, &data, &meta).await?;

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
        if task.action == SyncAction::Create {
            Ok(TaskResult::Created { bytes })
        } else {
            Ok(TaskResult::Updated { bytes })
        }
    }

    /// Execute a batch of tasks in parallel.
    pub async fn execute_batch(&self, tasks: Vec<SyncTask>) -> Result<SyncStats> {
        let start = Instant::now();
        let mut stats = SyncStats::default();

        // Process tasks in parallel with bounded concurrency
        let results: Vec<Result<TaskResult>> = stream::iter(tasks.iter())
            .map(|task| async move { self.execute_task(task).await })
            .buffer_unordered(self.max_concurrent)
            .collect()
            .await;

        // Aggregate results
        for result in results {
            match result? {
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

        stats.duration = start.elapsed();
        Ok(stats)
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

        // Create source file
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

        // Create source file
        std::fs::write(src_dir.path().join("test.txt"), "updated").unwrap();
        // Create old dest file
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

        // Create source dir
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

        // Create dest file to delete
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

        // Create source file
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
        assert!(!dst_dir.path().join("test.txt").exists()); // Not actually created
    }

    #[tokio::test]
    async fn test_dry_run_delete_does_not_modify() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create dest file
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
        assert!(dst_dir.path().join("keep.txt").exists()); // Not actually deleted
    }

    #[tokio::test]
    async fn test_verify_transfer() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create matching files
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

        // Create mismatched files
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

        // Create source file
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
                verify_on_write: true, // Enable verification
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
        // Should succeed (files match)
        assert!(matches!(result, TaskResult::Created { .. }));
    }

    #[tokio::test]
    async fn test_execute_batch_parallel() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create multiple source files
        for i in 0..5 {
            std::fs::write(
                src_dir.path().join(format!("file{}.txt", i)),
                format!("content{}", i),
            ).unwrap();
        }

        let source = LocalEndpoint::new(src_dir.path().to_path_buf());
        let dest = LocalEndpoint::new(dst_dir.path().to_path_buf());
        let executor = test_executor(&source, &dest);

        // Create tasks
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
        assert_eq!(stats.bytes_transferred, 40); // 5 files * 8 bytes

        // Verify all files exist
        for i in 0..5 {
            let path = dst_dir.path().join(format!("file{}.txt", i));
            assert!(path.exists());
        }
    }

    #[tokio::test]
    async fn test_execute_batch_mixed_actions() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create source file for update
        std::fs::write(src_dir.path().join("update.txt"), "new").unwrap();
        // Create old dest file
        std::fs::write(dst_dir.path().join("update.txt"), "old").unwrap();

        // Create dest file for deletion
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

        // Verify file was updated
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("update.txt")).unwrap(), "new");
        // Verify file was deleted
        assert!(!dst_dir.path().join("delete.txt").exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_execute_symlink_preserve() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create target file
        std::fs::write(src_dir.path().join("target.txt"), "target").unwrap();
        // Create symlink
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

        // Verify symlink exists and points to correct target
        let dest_link = dst_dir.path().join("link.txt");
        let meta = std::fs::symlink_metadata(&dest_link).unwrap();
        assert!(meta.is_symlink());
        assert_eq!(std::fs::read_link(&dest_link).unwrap().to_str().unwrap(), "target.txt");
    }

    #[tokio::test]
    async fn test_create_nested_directories() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create nested source dir
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

        // Create empty file
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
}
