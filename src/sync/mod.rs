pub mod scanner;
pub mod strategy;
pub mod transfer;

use crate::error::Result;
use crate::transport::Transport;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::{Arc, Mutex};
use strategy::{StrategyPlanner, SyncAction};
use tokio::sync::Semaphore;
use transfer::Transferrer;

#[derive(Debug)]
pub struct SyncStats {
    pub files_scanned: usize,
    pub files_created: usize,
    pub files_updated: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub bytes_transferred: u64,
}

pub struct SyncEngine<T: Transport> {
    transport: Arc<T>,
    dry_run: bool,
    delete: bool,
    quiet: bool,
    max_concurrent: usize,
}

impl<T: Transport + 'static> SyncEngine<T> {
    pub fn new(transport: T, dry_run: bool, delete: bool, quiet: bool, max_concurrent: usize) -> Self {
        Self {
            transport: Arc::new(transport),
            dry_run,
            delete,
            quiet,
            max_concurrent,
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

        // Execute sync operations in parallel
        // Thread-safe stats tracking
        let stats = Arc::new(Mutex::new(SyncStats {
            files_scanned: source_files.len(),
            files_created: 0,
            files_updated: 0,
            files_skipped: 0,
            files_deleted: 0,
            bytes_transferred: 0,
        }));

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

        // Parallel execution with semaphore for concurrency control
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles = Vec::with_capacity(tasks.len());

        for task in tasks {
            let transport = Arc::clone(&self.transport);
            let dry_run = self.dry_run;
            let stats = Arc::clone(&stats);
            let pb = pb.clone();
            let permit = semaphore.clone().acquire_owned().await.unwrap();

            let handle = tokio::spawn(async move {
                let transferrer = Transferrer::new(transport.as_ref(), dry_run);

                // Update progress message
                let msg = match &task.action {
                    SyncAction::Create => format!("Creating {}", task.dest_path.display()),
                    SyncAction::Update => format!("Updating {}", task.dest_path.display()),
                    SyncAction::Skip => format!("Skipping {}", task.dest_path.display()),
                    SyncAction::Delete => format!("Deleting {}", task.dest_path.display()),
                };

                if !matches!(task.action, SyncAction::Skip) {
                    pb.set_message(msg);
                }

                // Execute task
                let result = match task.action {
                    SyncAction::Create => {
                        if let Some(source) = &task.source {
                            match transferrer.create(source, &task.dest_path).await {
                                Ok(transfer_result) => {
                                    let mut stats = stats.lock().unwrap();
                                    if let Some(result) = transfer_result {
                                        stats.bytes_transferred += result.bytes_written;
                                    }
                                    stats.files_created += 1;
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(())
                        }
                    }
                    SyncAction::Update => {
                        if let Some(source) = &task.source {
                            match transferrer.update(source, &task.dest_path).await {
                                Ok(transfer_result) => {
                                    let mut stats = stats.lock().unwrap();
                                    if let Some(result) = transfer_result {
                                        stats.bytes_transferred += result.bytes_written;
                                    }
                                    stats.files_updated += 1;
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(())
                        }
                    }
                    SyncAction::Skip => {
                        let mut stats = stats.lock().unwrap();
                        stats.files_skipped += 1;
                        Ok(())
                    }
                    SyncAction::Delete => {
                        let is_dir = task.dest_path.is_dir();
                        match transferrer.delete(&task.dest_path, is_dir).await {
                            Ok(_) => {
                                let mut stats = stats.lock().unwrap();
                                stats.files_deleted += 1;
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    }
                };

                pb.inc(1);
                drop(permit);
                result
            });

            handles.push(handle);
        }

        // Collect all results
        let results = futures::future::join_all(handles).await;

        // Check for errors
        let mut first_error = None;
        for result in results {
            match result {
                Ok(Ok(())) => {} // Success
                Ok(Err(e)) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(crate::error::SyncError::Io(std::io::Error::other(
                            format!("Task panicked: {}", e),
                        )));
                    }
                }
            }
        }

        pb.finish_with_message("Sync complete");

        // Extract final stats
        let final_stats = Arc::try_unwrap(stats).unwrap().into_inner().unwrap();

        tracing::info!(
            "Sync complete: {} created, {} updated, {} skipped, {} deleted",
            final_stats.files_created,
            final_stats.files_updated,
            final_stats.files_skipped,
            final_stats.files_deleted
        );

        // Return first error if any occurred
        if let Some(e) = first_error {
            return Err(e);
        }

        Ok(final_stats)
    }
}
