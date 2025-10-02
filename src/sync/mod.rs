pub mod scanner;
pub mod strategy;
pub mod transfer;

use crate::error::Result;
use crate::transport::Transport;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use strategy::{StrategyPlanner, SyncAction};
use transfer::Transferrer;

pub struct SyncStats {
    pub files_scanned: usize,
    pub files_created: usize,
    pub files_updated: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub bytes_transferred: u64,
}

pub struct SyncEngine<T: Transport> {
    transport: T,
    dry_run: bool,
    delete: bool,
    quiet: bool,
}

impl<T: Transport> SyncEngine<T> {
    pub fn new(transport: T, dry_run: bool, delete: bool, quiet: bool) -> Self {
        Self {
            transport,
            dry_run,
            delete,
            quiet,
        }
    }

    pub async fn sync(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
        tracing::info!(
            "Starting sync: {} → {}",
            source.display(),
            destination.display()
        );

        // Scan source directory
        tracing::debug!("Scanning source directory...");
        let source_files = self.transport.scan(source).await?;
        tracing::info!("Found {} items in source", source_files.len());

        // Plan sync operations
        let planner = StrategyPlanner::new();
        let mut tasks = Vec::with_capacity(source_files.len());

        for file in &source_files {
            let task = planner
                .plan_file_async(file, destination, &self.transport)
                .await?;
            tasks.push(task);
        }

        // Plan deletions if requested
        if self.delete {
            let deletions = planner.plan_deletions(&source_files, destination);
            tasks.extend(deletions);
        }

        // Execute sync operations
        let transferrer = Transferrer::new(&self.transport, self.dry_run);
        let mut stats = SyncStats {
            files_scanned: source_files.len(),
            files_created: 0,
            files_updated: 0,
            files_skipped: 0,
            files_deleted: 0,
            bytes_transferred: 0,
        };

        // Create progress bar (only if not quiet)
        let pb = if self.quiet {
            ProgressBar::hidden()
        } else {
            let pb = ProgressBar::new(tasks.len() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                    .unwrap()
                    .progress_chars("#>-"),
            );
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb
        };

        // TODO: Parallel execution with tokio::spawn and semaphore for concurrency control
        // Current: Sequential execution (simple, correct)
        // Future: Parallel with Arc<Semaphore> to limit concurrent operations
        for (task_count, task) in tasks.into_iter().enumerate() {
            // Only update progress bar for actual actions or every 10 files
            let should_update = !matches!(task.action, SyncAction::Skip) || task_count % 10 == 0;

            if should_update {
                let msg = match &task.action {
                    SyncAction::Create => format!("Creating {}", task.dest_path.display()),
                    SyncAction::Update => format!("Updating {}", task.dest_path.display()),
                    SyncAction::Skip => format!("Skipping {}", task.dest_path.display()),
                    SyncAction::Delete => format!("Deleting {}", task.dest_path.display()),
                };
                pb.set_message(msg);
            }

            match task.action {
                SyncAction::Create => {
                    if let Some(source) = &task.source {
                        transferrer.create(source, &task.dest_path).await?;
                        stats.files_created += 1;
                        if !source.is_dir {
                            stats.bytes_transferred += source.size;
                        }
                    }
                }
                SyncAction::Update => {
                    if let Some(source) = &task.source {
                        transferrer.update(source, &task.dest_path).await?;
                        stats.files_updated += 1;
                        if !source.is_dir {
                            stats.bytes_transferred += source.size;
                        }
                    }
                }
                SyncAction::Skip => {
                    stats.files_skipped += 1;
                }
                SyncAction::Delete => {
                    let is_dir = task.dest_path.is_dir();
                    transferrer.delete(&task.dest_path, is_dir).await?;
                    stats.files_deleted += 1;
                }
            }

            pb.inc(1);
        }

        pb.finish_with_message("Sync complete");

        tracing::info!(
            "Sync complete: {} created, {} updated, {} skipped, {} deleted",
            stats.files_created,
            stats.files_updated,
            stats.files_skipped,
            stats.files_deleted
        );

        Ok(stats)
    }
}
