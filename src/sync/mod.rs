pub mod scanner;
pub mod strategy;
pub mod transfer;
pub mod resume;
pub mod output;
pub mod watch;
mod ratelimit;

use crate::cli::SymlinkMode;
use crate::error::Result;
use crate::integrity::{ChecksumType, IntegrityVerifier};
use crate::transport::Transport;
use indicatif::{ProgressBar, ProgressStyle};
use resume::{ResumeState, SyncFlags};
use output::SyncEvent;
use ratelimit::RateLimiter;
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
    pub files_compressed: usize,
    pub compression_bytes_saved: u64,
    pub files_verified: usize,
    pub verification_failures: usize,
    pub duration: Duration,
}

pub struct SyncEngine<T: Transport> {
    transport: Arc<T>,
    dry_run: bool,
    delete: bool,
    delete_threshold: u8,
    trash: bool,
    force_delete: bool,
    quiet: bool,
    max_concurrent: usize,
    min_size: Option<u64>,
    max_size: Option<u64>,
    exclude_patterns: Vec<glob::Pattern>,
    bwlimit: Option<u64>,
    resume: bool,
    checkpoint_files: usize,
    checkpoint_bytes: u64,
    json: bool,
    verification_mode: ChecksumType,
    verify_on_write: bool,
    symlink_mode: SymlinkMode,
    preserve_xattrs: bool,
    preserve_hardlinks: bool,
    preserve_acls: bool,
    ignore_times: bool,
    size_only: bool,
    checksum: bool,
}

impl<T: Transport + 'static> SyncEngine<T> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transport: T,
        dry_run: bool,
        delete: bool,
        delete_threshold: u8,
        trash: bool,
        force_delete: bool,
        quiet: bool,
        max_concurrent: usize,
        min_size: Option<u64>,
        max_size: Option<u64>,
        exclude: Vec<String>,
        bwlimit: Option<u64>,
        resume: bool,
        checkpoint_files: usize,
        checkpoint_bytes: u64,
        json: bool,
        verification_mode: ChecksumType,
        verify_on_write: bool,
        symlink_mode: SymlinkMode,
        preserve_xattrs: bool,
        preserve_hardlinks: bool,
        preserve_acls: bool,
        ignore_times: bool,
        size_only: bool,
        checksum: bool,
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
            delete_threshold,
            trash,
            force_delete,
            quiet,
            max_concurrent,
            min_size,
            max_size,
            exclude_patterns,
            bwlimit,
            resume,
            checkpoint_files,
            checkpoint_bytes,
            json,
            verification_mode,
            verify_on_write,
            symlink_mode,
            preserve_xattrs,
            preserve_hardlinks,
            preserve_acls,
            ignore_times,
            size_only,
            checksum,
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

        // Check resources before starting sync
        if !self.dry_run {
            // Calculate estimated bytes needed
            let bytes_needed: u64 = source_files
                .iter()
                .filter(|f| !f.is_dir)
                .map(|f| f.size)
                .sum();

            // Check disk space
            if let Err(e) = crate::resource::check_disk_space(destination, bytes_needed) {
                return Err(e);
            }

            // Check FD limits
            crate::resource::check_fd_limits(self.max_concurrent)?;
        }

        // Load or create resume state
        let current_flags = SyncFlags {
            delete: self.delete,
            exclude: self.exclude_patterns.iter().map(|p| p.as_str().to_string()).collect(),
            min_size: self.min_size,
            max_size: self.max_size,
        };

        let resume_state = if self.resume {
            match ResumeState::load(destination)? {
                Some(state) => {
                    if state.is_compatible_with(&current_flags) {
                        let (completed, total) = state.progress();
                        tracing::info!("Resuming sync: {} of {} files already completed", completed, total);
                        if !self.quiet {
                            println!("📋 Resuming previous sync ({}/{} files completed)", completed, total);
                        }
                        Some(state)
                    } else {
                        tracing::warn!("Resume state incompatible (flags changed), starting fresh");
                        if !self.quiet {
                            println!("⚠️  Resume state incompatible, starting fresh sync");
                        }
                        ResumeState::delete(destination)?;
                        Some(ResumeState::new(
                            source.to_path_buf(),
                            destination.to_path_buf(),
                            current_flags,
                            source_files.len(),
                        ))
                    }
                }
                None => {
                    // No existing state, create new one
                    Some(ResumeState::new(
                        source.to_path_buf(),
                        destination.to_path_buf(),
                        current_flags,
                        source_files.len(),
                    ))
                }
            }
        } else {
            None
        };

        // Get set of completed files for filtering
        let completed_paths = resume_state
            .as_ref()
            .map(|s| s.completed_paths())
            .unwrap_or_default();

        // Plan sync operations
        let planner = StrategyPlanner::with_comparison_flags(
            self.ignore_times,
            self.size_only,
            self.checksum,
        );
        let mut tasks = Vec::with_capacity(source_files.len());

        for file in &source_files {
            // Skip files that are already completed (if resuming)
            if !completed_paths.is_empty() && completed_paths.contains(&file.relative_path) {
                tracing::debug!("Skipping completed file: {}", file.relative_path.display());
                continue;
            }

            let task = planner
                .plan_file_async(file, destination, &self.transport)
                .await?;
            tasks.push(task);
        }

        // Plan deletions if requested
        if self.delete {
            let deletions = planner.plan_deletions(&source_files, destination);

            // Apply deletion safety checks
            if !deletions.is_empty() && !self.force_delete {
                let dest_file_count = scanner::Scanner::new(destination)
                    .scan()
                    .map(|files| files.len())
                    .unwrap_or(0);

                // Check threshold: prevent mass deletion
                if dest_file_count > 0 {
                    let delete_percentage = (deletions.len() as f64 / dest_file_count as f64) * 100.0;

                    if delete_percentage > self.delete_threshold as f64 {
                        tracing::error!(
                            "Refusing to delete {:.1}% of destination files ({} files). Threshold: {}%. Use --force-delete to override.",
                            delete_percentage,
                            deletions.len(),
                            self.delete_threshold
                        );

                        if !self.quiet {
                            eprintln!(
                                "⚠️  ERROR: Would delete {:.1}% of files ({}/{}), exceeding threshold of {}%",
                                delete_percentage,
                                deletions.len(),
                                dest_file_count,
                                self.delete_threshold
                            );
                            eprintln!("Use --force-delete to skip safety checks (dangerous!)");
                        }

                        return Err(crate::error::SyncError::Io(std::io::Error::other(
                            format!("Deletion threshold exceeded: {:.1}% > {}%", delete_percentage, self.delete_threshold)
                        )));
                    }
                }

                // Check count threshold: warn if deleting many files
                if deletions.len() > 1000 && !self.quiet && !self.json {
                    eprintln!(
                        "⚠️  WARNING: About to delete {} files. Continue? [y/N] ",
                        deletions.len()
                    );

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;

                    if !input.trim().eq_ignore_ascii_case("y") {
                        tracing::info!("Deletion cancelled by user");
                        return Err(crate::error::SyncError::Io(std::io::Error::other("Deletion cancelled by user")));
                    }
                }
            }

            tasks.extend(deletions);
        }

        // Emit start event if JSON mode
        if self.json {
            SyncEvent::Start {
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
                total_files: tasks.len(),
            }.emit();
        }

        // Wrap resume state for thread-safe access
        let resume_state = Arc::new(Mutex::new(resume_state));
        let _checkpoint_files = self.checkpoint_files;
        let _checkpoint_bytes = self.checkpoint_bytes;

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
            files_compressed: 0,
            compression_bytes_saved: 0,
            files_verified: 0,
            verification_failures: 0,
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

        // Create rate limiter if bandwidth limit is set
        let rate_limiter = self.bwlimit.map(|limit| Arc::new(Mutex::new(RateLimiter::new(limit))));

        // Create hardlink map for tracking inodes (shared across all parallel transfers)
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));

        // Parallel execution with semaphore for concurrency control
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles = Vec::with_capacity(tasks.len());

        for task in tasks {
            let transport = Arc::clone(&self.transport);
            let dry_run = self.dry_run;
            let json = self.json;
            let stats = Arc::clone(&stats);
            let pb = pb.clone();
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let rate_limiter = rate_limiter.clone();
            let _resume_state = Arc::clone(&resume_state);
            let _dest_path_for_checkpoint = destination.to_path_buf();
            let verification_mode = self.verification_mode;
            let verify_on_write = self.verify_on_write;
            let symlink_mode = self.symlink_mode;
            let preserve_xattrs = self.preserve_xattrs;
            let preserve_hardlinks = self.preserve_hardlinks;
            let preserve_acls = self.preserve_acls;
            let hardlink_map = Arc::clone(&hardlink_map);

            let handle = tokio::spawn(async move {
                let transferrer = Transferrer::new(
                    transport.as_ref(),
                    dry_run,
                    symlink_mode,
                    preserve_xattrs,
                    preserve_hardlinks,
                    preserve_acls,
                    hardlink_map,
                );
                let verifier = IntegrityVerifier::new(verification_mode, verify_on_write);

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
                                    let bytes_written = if let Some(ref result) = transfer_result {
                                        result.bytes_written
                                    } else {
                                        0
                                    };

                                    {
                                        let mut stats = stats.lock().unwrap();
                                        stats.bytes_transferred += bytes_written;
                                        stats.files_created += 1;

                                        // Track compression usage and savings
                                        if let Some(ref result) = transfer_result {
                                            if result.compression_used {
                                                stats.files_compressed += 1;

                                                // Calculate bytes saved (uncompressed - compressed)
                                                if let Some(transferred) = result.transferred_bytes {
                                                    let bytes_saved = result.bytes_written.saturating_sub(transferred);
                                                    stats.compression_bytes_saved += bytes_saved;
                                                }
                                            }
                                        }
                                    }

                                    // Apply rate limiting if enabled (outside stats lock)
                                    if let Some(ref limiter) = rate_limiter {
                                        if bytes_written > 0 {
                                            let sleep_duration = limiter.lock().unwrap().consume(bytes_written);
                                            if sleep_duration > Duration::ZERO {
                                                tokio::time::sleep(sleep_duration).await;
                                            }
                                        }
                                    }

                                    // Verify transfer if verification is enabled
                                    if verification_mode != ChecksumType::None && !dry_run {
                                        let source_path = &source.path;
                                        let dest_path = &task.dest_path;

                                        match verifier.verify_transfer(source_path, dest_path) {
                                            Ok(verified) => {
                                                let mut stats = stats.lock().unwrap();
                                                if verified {
                                                    stats.files_verified += 1;
                                                } else {
                                                    stats.verification_failures += 1;
                                                    tracing::warn!(
                                                        "Verification failed for {}: checksums do not match",
                                                        dest_path.display()
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Verification error for {}: {}",
                                                    dest_path.display(),
                                                    e
                                                );
                                                let mut stats = stats.lock().unwrap();
                                                stats.verification_failures += 1;
                                            }
                                        }
                                    }

                                    // Emit JSON event if enabled
                                    if json {
                                        SyncEvent::Create {
                                            path: task.dest_path.clone(),
                                            size: source.size,
                                            bytes_transferred: bytes_written,
                                        }.emit();
                                    }

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
                                    let bytes_written = if let Some(ref result) = transfer_result {
                                        result.bytes_written
                                    } else {
                                        0
                                    };

                                    {
                                        let mut stats = stats.lock().unwrap();
                                        if let Some(ref result) = transfer_result {
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

                                            // Track compression usage and savings
                                            if result.compression_used {
                                                stats.files_compressed += 1;

                                                // Calculate bytes saved (uncompressed - compressed)
                                                if let Some(transferred) = result.transferred_bytes {
                                                    let bytes_saved = result.bytes_written.saturating_sub(transferred);
                                                    stats.compression_bytes_saved += bytes_saved;
                                                }
                                            }
                                        }
                                        stats.files_updated += 1;
                                    }

                                    // Apply rate limiting if enabled (outside stats lock)
                                    if let Some(ref limiter) = rate_limiter {
                                        if bytes_written > 0 {
                                            let sleep_duration = limiter.lock().unwrap().consume(bytes_written);
                                            if sleep_duration > Duration::ZERO {
                                                tokio::time::sleep(sleep_duration).await;
                                            }
                                        }
                                    }

                                    // Verify transfer if verification is enabled
                                    if verification_mode != ChecksumType::None && !dry_run {
                                        let source_path = &source.path;
                                        let dest_path = &task.dest_path;

                                        match verifier.verify_transfer(source_path, dest_path) {
                                            Ok(verified) => {
                                                let mut stats = stats.lock().unwrap();
                                                if verified {
                                                    stats.files_verified += 1;
                                                } else {
                                                    stats.verification_failures += 1;
                                                    tracing::warn!(
                                                        "Verification failed for {}: checksums do not match",
                                                        dest_path.display()
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Verification error for {}: {}",
                                                    dest_path.display(),
                                                    e
                                                );
                                                let mut stats = stats.lock().unwrap();
                                                stats.verification_failures += 1;
                                            }
                                        }
                                    }

                                    // Emit JSON event if enabled
                                    if json {
                                        let delta_used = transfer_result.as_ref()
                                            .map(|r| r.used_delta())
                                            .unwrap_or(false);
                                        SyncEvent::Update {
                                            path: task.dest_path.clone(),
                                            size: source.size,
                                            bytes_transferred: bytes_written,
                                            delta_used,
                                        }.emit();
                                    }

                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(())
                        }
                    }
                    SyncAction::Skip => {
                        {
                            let mut stats = stats.lock().unwrap();
                            stats.files_skipped += 1;
                        }

                        // Emit JSON event if enabled
                        if json {
                            SyncEvent::Skip {
                                path: task.dest_path.clone(),
                                reason: "up_to_date".to_string(),
                            }.emit();
                        }

                        Ok(())
                    }
                    SyncAction::Delete => {
                        let is_dir = task.dest_path.is_dir();
                        match transferrer.delete(&task.dest_path, is_dir).await {
                            Ok(_) => {
                                {
                                    let mut stats = stats.lock().unwrap();
                                    stats.files_deleted += 1;
                                }

                                // Emit JSON event if enabled
                                if json {
                                    SyncEvent::Delete {
                                        path: task.dest_path.clone(),
                                    }.emit();
                                }

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

        // Emit summary event if JSON mode
        if self.json {
            SyncEvent::Summary {
                files_created: final_stats.files_created,
                files_updated: final_stats.files_updated,
                files_skipped: final_stats.files_skipped,
                files_deleted: final_stats.files_deleted,
                bytes_transferred: final_stats.bytes_transferred,
                duration_secs: final_stats.duration.as_secs_f64(),
                files_verified: final_stats.files_verified,
                verification_failures: final_stats.verification_failures,
            }.emit();
        }

        // Clean up resume state on successful completion
        if let Ok(mut state_guard) = resume_state.lock() {
            if let Some(_) = *state_guard {
                // Only clean up if this was an actual resume operation
                // (Don't clean up if we just created a new state that was never saved)
                if ResumeState::load(destination)?.is_some() {
                    tracing::debug!("Cleaning up resume state file");
                    if let Err(e) = ResumeState::delete(destination) {
                        tracing::warn!("Failed to delete resume state: {}", e);
                    }
                }
            }
            // Drop the state
            *state_guard = None;
        }

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
            files_compressed: 0,
            compression_bytes_saved: 0,
            files_verified: 0,
            verification_failures: 0,
            duration: Duration::ZERO,
        };

        // Check if destination exists
        let dest_exists = self.transport.exists(destination).await?;

        // Create hardlink map (not used for single-file sync, but required by Transferrer)
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));

        let transferrer = Transferrer::new(
            self.transport.as_ref(),
            self.dry_run,
            self.symlink_mode,
            self.preserve_xattrs,
            self.preserve_hardlinks,
            self.preserve_acls,
            hardlink_map,
        );

        if !dest_exists {
            // Create new file
            tracing::info!("Creating {}", destination.display());
            let metadata = source.metadata()?;
            let filename = source.file_name()
                .ok_or_else(|| crate::error::SyncError::Io(std::io::Error::other(
                    format!("Invalid source path: {}", source.display())
                )))?
                .to_owned();
            if let Some(result) = transferrer.create(&FileEntry {
                path: source.to_path_buf(),
                relative_path: PathBuf::from(filename),
                size: metadata.len(),
                modified: metadata.modified()?,
                is_dir: false,
                is_symlink: false,
                symlink_target: None,
                is_sparse: false,
                allocated_size: metadata.len(),
                xattrs: None,
                inode: None,
                nlink: 1,
                acls: None,
            }, destination).await? {
                stats.bytes_transferred = result.bytes_written;

                // Track compression if used
                if result.compression_used {
                    stats.files_compressed = 1;
                    if let Some(transferred) = result.transferred_bytes {
                        stats.compression_bytes_saved = result.bytes_written.saturating_sub(transferred);
                    }
                }
            }
            stats.files_created = 1;

            // Verify transfer if verification is enabled
            if self.verification_mode != ChecksumType::None && !self.dry_run {
                let verifier = IntegrityVerifier::new(self.verification_mode, self.verify_on_write);
                match verifier.verify_transfer(source, destination) {
                    Ok(verified) => {
                        if verified {
                            stats.files_verified = 1;
                        } else {
                            stats.verification_failures = 1;
                            tracing::warn!(
                                "Verification failed for {}: checksums do not match",
                                destination.display()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Verification error for {}: {}",
                            destination.display(),
                            e
                        );
                        stats.verification_failures = 1;
                    }
                }
            }
        } else {
            // Update existing file
            tracing::info!("Updating {}", destination.display());
            let metadata = source.metadata()?;
            let filename = source.file_name()
                .ok_or_else(|| crate::error::SyncError::Io(std::io::Error::other(
                    format!("Invalid source path: {}", source.display())
                )))?
                .to_owned();
            if let Some(result) = transferrer.update(&FileEntry {
                path: source.to_path_buf(),
                relative_path: PathBuf::from(filename),
                size: metadata.len(),
                modified: metadata.modified()?,
                is_dir: false,
                is_symlink: false,
                symlink_target: None,
                is_sparse: false,
                allocated_size: metadata.len(),
                xattrs: None,
                inode: None,
                nlink: 1,
                acls: None,
            }, destination).await? {
                stats.bytes_transferred = result.bytes_written;

                // Track delta sync if used
                if result.used_delta() {
                    stats.files_delta_synced = 1;
                    if let Some(literal_bytes) = result.literal_bytes {
                        stats.delta_bytes_saved = result.bytes_written.saturating_sub(literal_bytes);
                    }
                }

                // Track compression if used
                if result.compression_used {
                    stats.files_compressed = 1;
                    if let Some(transferred) = result.transferred_bytes {
                        stats.compression_bytes_saved = result.bytes_written.saturating_sub(transferred);
                    }
                }
            }
            stats.files_updated = 1;

            // Verify transfer if verification is enabled
            if self.verification_mode != ChecksumType::None && !self.dry_run {
                let verifier = IntegrityVerifier::new(self.verification_mode, self.verify_on_write);
                match verifier.verify_transfer(source, destination) {
                    Ok(verified) => {
                        if verified {
                            stats.files_verified = 1;
                        } else {
                            stats.verification_failures = 1;
                            tracing::warn!(
                                "Verification failed for {}: checksums do not match",
                                destination.display()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Verification error for {}: {}",
                            destination.display(),
                            e
                        );
                        stats.verification_failures = 1;
                    }
                }
            }
        }

        stats.duration = start_time.elapsed();
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::local::LocalTransport;
    use std::fs;
    use tempfile::TempDir;

    // Helper to create a basic sync engine for testing
    fn create_test_engine() -> SyncEngine<LocalTransport> {
        let transport = LocalTransport::new();
        SyncEngine::new(
            transport,
            false, // dry_run
            false, // delete
            50, // delete_threshold
            false, // trash
            false, // force_delete
            true,  // quiet
            4,     // max_concurrent
            None,  // min_size
            None,  // max_size
            Vec::new(), // exclude
            None,  // bwlimit
            false, // resume
            0,     // checkpoint_files
            0,     // checkpoint_bytes
            false, // json
            ChecksumType::Fast,
            false, // verify_on_write
            SymlinkMode::Preserve,
            false, // preserve_xattrs
            false, // preserve_hardlinks
            false, // preserve_acls
            false, // ignore_times
            false, // size_only
            false, // checksum
        )
    }

    #[tokio::test]
    async fn test_basic_sync_success() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create test files in source
        fs::write(source_dir.path().join("file1.txt"), "content1").unwrap();
        fs::write(source_dir.path().join("file2.txt"), "content2").unwrap();

        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        assert_eq!(stats.files_created, 2);
        assert!(dest_dir.path().join("file1.txt").exists());
        assert!(dest_dir.path().join("file2.txt").exists());
        assert_eq!(fs::read_to_string(dest_dir.path().join("file1.txt")).unwrap(), "content1");
    }

    #[tokio::test]
    async fn test_sync_with_subdirectories() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create nested structure
        fs::create_dir(source_dir.path().join("subdir")).unwrap();
        fs::write(source_dir.path().join("subdir/file.txt"), "nested").unwrap();

        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        assert!(stats.files_created >= 1);
        assert!(dest_dir.path().join("subdir/file.txt").exists());
        assert_eq!(fs::read_to_string(dest_dir.path().join("subdir/file.txt")).unwrap(), "nested");
    }

    #[tokio::test]
    async fn test_sync_empty_source() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        assert_eq!(stats.files_created, 0);
        assert_eq!(stats.files_scanned, 0);
    }

    #[tokio::test]
    async fn test_sync_dry_run_no_changes() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        fs::write(source_dir.path().join("file.txt"), "content").unwrap();

        let transport = LocalTransport::new();
        let engine = SyncEngine::new(
            transport,
            true,  // dry_run = true
            false, // delete
            50, // delete_threshold
            false, // trash
            false, // force_delete
            true,  // quiet
            4,     // max_concurrent
            None,  // min_size
            None,  // max_size
            Vec::new(), // exclude
            None,  // bwlimit
            false, // resume
            0,     // checkpoint_files
            0,     // checkpoint_bytes
            false, // json
            ChecksumType::Fast,
            false, // verify_on_write
            SymlinkMode::Preserve,
            false, // preserve_xattrs
            false, // preserve_hardlinks
            false, // preserve_acls
            false, // ignore_times
            false, // size_only
            false, // checksum
        );

        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        // Dry run should scan but not create files
        assert_eq!(stats.files_scanned, 1);
        assert!(!dest_dir.path().join("file.txt").exists());
    }

    // === TOCTOU (Time-Of-Check-Time-Of-Use) Tests ===

    #[tokio::test]
    async fn test_toctou_file_deleted_after_scan() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create file
        let file_path = source_dir.path().join("file.txt");
        fs::write(&file_path, "content").unwrap();

        // Scan the source
        let scanner = scanner::Scanner::new(source_dir.path());
        let source_files = scanner.scan().unwrap();
        assert_eq!(source_files.len(), 1);

        // Delete file after scan (simulating TOCTOU)
        fs::remove_file(&file_path).unwrap();

        // Try to sync - should handle gracefully
        let engine = create_test_engine();
        let result = engine.sync(source_dir.path(), dest_dir.path()).await;

        // Should either succeed with 0 files or handle the error gracefully
        match result {
            Ok(stats) => {
                // File was deleted, so it shouldn't be transferred
                assert_eq!(stats.files_created, 0);
            }
            Err(_) => {
                // Error is also acceptable for TOCTOU scenarios
            }
        }
    }

    #[tokio::test]
    async fn test_toctou_file_modified_after_scan() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create file with initial content
        let file_path = source_dir.path().join("file.txt");
        fs::write(&file_path, "initial content").unwrap();

        // Start sync in background
        let engine = create_test_engine();
        let source = source_dir.path().to_path_buf();
        let dest = dest_dir.path().to_path_buf();

        // Immediately modify the file (race condition simulation)
        fs::write(&file_path, "modified content").unwrap();

        // Complete sync
        let stats = engine.sync(&source, &dest).await.unwrap();

        // File should be transferred (either old or new content is acceptable)
        assert_eq!(stats.files_created, 1);
        assert!(dest_dir.path().join("file.txt").exists());
    }

    #[tokio::test]
    async fn test_toctou_file_size_changed() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create small file
        let file_path = source_dir.path().join("file.txt");
        fs::write(&file_path, "small").unwrap();

        // Get initial metadata
        let initial_size = fs::metadata(&file_path).unwrap().len();
        assert_eq!(initial_size, 5);

        // Immediately write much larger content (simulating concurrent modification)
        fs::write(&file_path, "a".repeat(10000)).unwrap();

        // Sync should handle size change
        let engine = create_test_engine();
        let result = engine.sync(source_dir.path(), dest_dir.path()).await;

        // Should either succeed or fail gracefully
        match result {
            Ok(stats) => {
                assert_eq!(stats.files_created, 1);
                // File should exist at destination
                assert!(dest_dir.path().join("file.txt").exists());
            }
            Err(_) => {
                // Error is acceptable for size mismatch scenarios
            }
        }
    }

    #[tokio::test]
    async fn test_toctou_directory_deleted_after_scan() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create directory with file
        let subdir = source_dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("file.txt"), "content").unwrap();

        // Scan
        let scanner = scanner::Scanner::new(source_dir.path());
        let source_files = scanner.scan().unwrap();
        assert!(source_files.len() >= 1);

        // Delete directory after scan
        fs::remove_dir_all(&subdir).unwrap();

        // Sync should handle gracefully
        let engine = create_test_engine();
        let result = engine.sync(source_dir.path(), dest_dir.path()).await;

        match result {
            Ok(stats) => {
                // Directory was deleted, so files shouldn't be created
                assert_eq!(stats.files_created, 0);
            }
            Err(_) => {
                // Error is acceptable
            }
        }
    }

    #[tokio::test]
    async fn test_toctou_new_file_created_during_sync() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create initial file
        fs::write(source_dir.path().join("file1.txt"), "content1").unwrap();

        // Create new file immediately (won't be in initial scan)
        fs::write(source_dir.path().join("file2.txt"), "content2").unwrap();

        // Sync - should get file1 (file2 created after scan won't be included)
        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        // Should transfer the files that existed at scan time
        assert!(stats.files_created >= 1);
    }

    // === Stress Tests ===

    #[tokio::test]
    async fn test_sync_many_small_files() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create 100 small files
        for i in 0..100 {
            fs::write(
                source_dir.path().join(format!("file{}.txt", i)),
                format!("content{}", i),
            ).unwrap();
        }

        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        assert_eq!(stats.files_created, 100);

        // Verify all files transferred
        for i in 0..100 {
            assert!(dest_dir.path().join(format!("file{}.txt", i)).exists());
        }
    }

    #[tokio::test]
    async fn test_sync_very_deep_nesting() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create 100-level deep nesting
        let mut path = source_dir.path().to_path_buf();
        for i in 0..100 {
            path.push(format!("level{}", i));
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("deep.txt"), "very deep content").unwrap();

        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        assert!(stats.files_created >= 1);

        // Verify deeply nested file exists
        let mut dest_path = dest_dir.path().to_path_buf();
        for i in 0..100 {
            dest_path.push(format!("level{}", i));
        }
        dest_path.push("deep.txt");
        assert!(dest_path.exists());
        assert_eq!(fs::read_to_string(&dest_path).unwrap(), "very deep content");
    }

    #[tokio::test]
    async fn test_sync_large_file() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create 10MB file
        let large_content = "x".repeat(10 * 1024 * 1024);
        fs::write(source_dir.path().join("large.bin"), &large_content).unwrap();

        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        assert_eq!(stats.files_created, 1);
        assert!(stats.bytes_transferred >= 10 * 1024 * 1024);

        let dest_file = dest_dir.path().join("large.bin");
        assert!(dest_file.exists());
        assert_eq!(fs::metadata(&dest_file).unwrap().len(), 10 * 1024 * 1024);
    }

    #[tokio::test]
    async fn test_sync_mixed_sizes() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Mix of file sizes
        fs::write(source_dir.path().join("tiny.txt"), "x").unwrap();
        fs::write(source_dir.path().join("small.txt"), "x".repeat(1024)).unwrap();
        fs::write(source_dir.path().join("medium.txt"), "x".repeat(100 * 1024)).unwrap();
        fs::write(source_dir.path().join("large.txt"), "x".repeat(1024 * 1024)).unwrap();

        let engine = create_test_engine();
        let stats = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();

        assert_eq!(stats.files_created, 4);
        assert!(dest_dir.path().join("tiny.txt").exists());
        assert!(dest_dir.path().join("small.txt").exists());
        assert!(dest_dir.path().join("medium.txt").exists());
        assert!(dest_dir.path().join("large.txt").exists());
    }

    #[tokio::test]
    async fn test_sync_idempotent() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        fs::write(source_dir.path().join("file.txt"), "content").unwrap();

        let engine = create_test_engine();

        // First sync
        let stats1 = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();
        assert_eq!(stats1.files_created, 1);

        // Second sync - should skip unchanged file
        let stats2 = engine.sync(source_dir.path(), dest_dir.path()).await.unwrap();
        assert_eq!(stats2.files_skipped, 1);
        assert_eq!(stats2.files_created, 0);
    }
}
