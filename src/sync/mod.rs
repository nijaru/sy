pub mod scanner;
pub mod strategy;
pub mod transfer;

use crate::error::Result;
use crate::transport::Transport;
use indicatif::{ProgressBar, ProgressStyle};
use scanner::FileEntry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
    pub files_delta_synced: usize,
    pub delta_bytes_saved: u64,
    pub duration: Duration,
}

pub struct SyncEngine<T: Transport> {
    transport: Arc<T>,
    dry_run: bool,
    delete: bool,
    quiet: bool,
    max_concurrent: usize,
    min_size: Option<u64>,
    max_size: Option<u64>,
    exclude_patterns: Vec<glob::Pattern>,
}

impl<T: Transport + 'static> SyncEngine<T> {
    pub fn new(
        transport: T,
        dry_run: bool,
        delete: bool,
        quiet: bool,
        max_concurrent: usize,
        min_size: Option<u64>,
        max_size: Option<u64>,
        exclude: Vec<String>,
    ) -> Self {
        // Compile exclude patterns once at creation
        let exclude_patterns = exclude
            .into_iter()
            .filter_map(|pattern| {
                match glob::Pattern::new(&pattern) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        tracing::warn!("Invalid exclude pattern '{}': {}", pattern, e);
                        None
                    }
                }
            })
            .collect();

        Self {
            transport: Arc::new(transport),
            dry_run,
            delete,
            quiet,
            max_concurrent,
            min_size,
            max_size,
            exclude_patterns,
        }
    }

    fn should_filter_by_size(&self, file_size: u64) -> bool {
        if let Some(min) = self.min_size {
            if file_size < min {
                return true;
            }
        }
        if let Some(max) = self.max_size {
            if file_size > max {
                return true;
            }
        }
        false
    }

    fn should_exclude(&self, relative_path: &Path) -> bool {
        if self.exclude_patterns.is_empty() {
            return false;
        }

        let path_str = relative_path.to_string_lossy();
        self.exclude_patterns.iter().any(|pattern| {
            pattern.matches(&path_str) || pattern.matches(relative_path.to_str().unwrap_or(""))
        })
    }

    pub async fn sync(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
        let start_time = std::time::Instant::now();

        tracing::info!(
            "Starting sync: {} → {}",
            source.display(),
            destination.display()
        );

        // Scan source directory
        tracing::debug!("Scanning source directory...");
        let all_files = self.transport.scan(source).await?;
        let total_scanned = all_files.len();
        tracing::info!("Found {} items in source", total_scanned);

        // Filter files by size and exclude patterns
        let source_files: Vec<_> = all_files
            .into_iter()
            .filter(|file| {
                // Don't filter directories
                if file.is_dir {
                    return true;
                }
                // Apply size filter
                if self.should_filter_by_size(file.size) {
                    return false;
                }
                // Apply exclude patterns
                if self.should_exclude(&file.relative_path) {
                    return false;
                }
                true
            })
            .collect();

        if source_files.len() < total_scanned {
            let filtered_count = total_scanned - source_files.len();
            tracing::info!("Filtered out {} files", filtered_count);
        }

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
            files_delta_synced: 0,
            delta_bytes_saved: 0,
            duration: Duration::ZERO,
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

                                        // Track delta sync usage and savings
                                        if result.used_delta() {
                                            stats.files_delta_synced += 1;

                                            // Calculate bytes saved (full file size - literal bytes)
                                            if let Some(literal_bytes) = result.literal_bytes {
                                                let bytes_saved = result.bytes_written.saturating_sub(literal_bytes);
                                                stats.delta_bytes_saved += bytes_saved;
                                            }

                                            if let Some(ratio) = result.compression_ratio() {
                                                pb.set_message(format!(
                                                    "Updated {} (delta: {:.1}% literal)",
                                                    task.dest_path.display(),
                                                    ratio
                                                ));
                                            }
                                        }
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

        // Extract final stats and add duration
        let mut final_stats = Arc::try_unwrap(stats).unwrap().into_inner().unwrap();
        final_stats.duration = start_time.elapsed();

        tracing::info!(
            "Sync complete: {} created, {} updated, {} skipped, {} deleted, took {:.2}s",
            final_stats.files_created,
            final_stats.files_updated,
            final_stats.files_skipped,
            final_stats.files_deleted,
            final_stats.duration.as_secs_f64()
        );

        // Return first error if any occurred
        if let Some(e) = first_error {
            return Err(e);
        }

        Ok(final_stats)
    }

    /// Sync a single file (source is a file, not a directory)
    pub async fn sync_single_file(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
        let start_time = std::time::Instant::now();

        tracing::info!(
            "Starting single file sync: {} → {}",
            source.display(),
            destination.display()
        );

        let mut stats = SyncStats {
            files_scanned: 1,
            files_created: 0,
            files_updated: 0,
            files_skipped: 0,
            files_deleted: 0,
            bytes_transferred: 0,
            files_delta_synced: 0,
            delta_bytes_saved: 0,
            duration: Duration::ZERO,
        };

        // Check if destination exists
        let dest_exists = self.transport.exists(destination).await?;
        let transferrer = Transferrer::new(self.transport.as_ref(), self.dry_run);

        if !dest_exists {
            // Create new file
            tracing::info!("Creating {}", destination.display());
            let metadata = source.metadata()?;
            let filename = source.file_name().unwrap().to_owned();
            if let Some(result) = transferrer.create(&FileEntry {
                path: source.to_path_buf(),
                relative_path: PathBuf::from(filename),
                size: metadata.len(),
                modified: metadata.modified()?,
                is_dir: false,
            }, destination).await? {
                stats.bytes_transferred = result.bytes_written;
            }
            stats.files_created = 1;
        } else {
            // Update existing file
            tracing::info!("Updating {}", destination.display());
            let metadata = source.metadata()?;
            let filename = source.file_name().unwrap().to_owned();
            if let Some(result) = transferrer.update(&FileEntry {
                path: source.to_path_buf(),
                relative_path: PathBuf::from(filename),
                size: metadata.len(),
                modified: metadata.modified()?,
                is_dir: false,
            }, destination).await? {
                stats.bytes_transferred = result.bytes_written;

                // Track delta sync if used
                if result.used_delta() {
                    stats.files_delta_synced = 1;
                    if let Some(literal_bytes) = result.literal_bytes {
                        stats.delta_bytes_saved = result.bytes_written.saturating_sub(literal_bytes);
                    }
                }
            }
            stats.files_updated = 1;
        }

        stats.duration = start_time.elapsed();
        Ok(stats)
    }
}
