pub mod scanner;
pub mod strategy;
pub mod transfer;

use crate::error::Result;
use indicatif::{ProgressBar, ProgressStyle};
use scanner::Scanner;
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

pub struct SyncEngine {
    dry_run: bool,
    delete: bool,
    quiet: bool,
}

impl SyncEngine {
    pub fn new(dry_run: bool, delete: bool, quiet: bool) -> Self {
        Self { dry_run, delete, quiet }
    }

    pub fn sync(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
        tracing::info!("Starting sync: {} → {}", source.display(), destination.display());

        // Scan source directory
        tracing::debug!("Scanning source directory...");
        let scanner = Scanner::new(source);
        let source_files = scanner.scan()?;
        tracing::info!("Found {} items in source", source_files.len());

        // Plan sync operations
        let planner = StrategyPlanner::new();
        let mut tasks = Vec::new();

        for file in &source_files {
            let task = planner.plan_file(file, destination);
            tasks.push(task);
        }

        // Plan deletions if requested
        if self.delete {
            let deletions = planner.plan_deletions(&source_files, destination);
            tasks.extend(deletions);
        }

        // Execute sync operations
        let transferrer = Transferrer::new(self.dry_run);
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
                    .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                    .unwrap()
                    .progress_chars("#>-"),
            );
            pb
        };

        for task in tasks {
            // Update progress message
            let msg = match &task.action {
                SyncAction::Create => format!("Creating {}", task.dest_path.display()),
                SyncAction::Update => format!("Updating {}", task.dest_path.display()),
                SyncAction::Skip => format!("Skipping {}", task.dest_path.display()),
                SyncAction::Delete => format!("Deleting {}", task.dest_path.display()),
            };
            pb.set_message(msg);

            match task.action {
                SyncAction::Create => {
                    if let Some(source) = &task.source {
                        transferrer.create(source, &task.dest_path)?;
                        stats.files_created += 1;
                        if !source.is_dir {
                            stats.bytes_transferred += source.size;
                        }
                    }
                }
                SyncAction::Update => {
                    if let Some(source) = &task.source {
                        transferrer.update(source, &task.dest_path)?;
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
                    transferrer.delete(&task.dest_path)?;
                    stats.files_deleted += 1;
                }
            }

            pb.inc(1);
        }

        pb.finish_with_message("Sync complete");

        tracing::info!("Sync complete: {} created, {} updated, {} skipped, {} deleted",
            stats.files_created, stats.files_updated, stats.files_skipped, stats.files_deleted);

        Ok(stats)
    }
}
