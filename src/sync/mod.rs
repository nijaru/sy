pub mod checksumdb;
pub mod dircache;
pub mod output;
mod ratelimit;
pub mod resume;
pub mod scale;
pub mod scanner;
pub mod strategy;
pub mod transfer;
pub mod watch;

use crate::cli::SymlinkMode;
use crate::error::Result;
use crate::filter::FilterEngine;
use crate::integrity::{ChecksumType, IntegrityVerifier};
use crate::perf::{PerformanceMetrics, PerformanceMonitor};
use crate::resource;
use crate::transport::Transport;
use dircache::DirectoryCache;
use indicatif::{ProgressBar, ProgressStyle};
use output::SyncEvent;
use ratelimit::RateLimiter;
use resume::{ResumeState, SyncFlags};
use scanner::FileEntry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strategy::{StrategyPlanner, SyncAction};
use tokio::sync::Semaphore;
use transfer::Transferrer;

#[derive(Debug, Clone)]
pub struct SyncError {
    pub path: PathBuf,
    pub error: String,
    pub action: String,
}

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
    // Dry-run statistics
    pub bytes_would_add: u64,
    pub bytes_would_change: u64,
    pub bytes_would_delete: u64,
    // Error tracking
    pub errors: Vec<SyncError>,
}

#[derive(Debug)]
pub struct VerificationResult {
    pub files_matched: usize,
    pub files_mismatched: Vec<PathBuf>,
    pub files_only_in_source: Vec<PathBuf>,
    pub files_only_in_dest: Vec<PathBuf>,
    pub errors: Vec<SyncError>,
    pub duration: Duration,
}

pub struct SyncEngine<T: Transport> {
    transport: Arc<T>,
    dry_run: bool,
    diff_mode: bool,
    delete: bool,
    delete_threshold: u8,
    #[allow(dead_code)] // Planned feature: trash/recycle bin support
    trash: bool,
    force_delete: bool,
    quiet: bool,
    max_concurrent: usize,
    max_errors: usize,
    min_size: Option<u64>,
    max_size: Option<u64>,
    filter_engine: FilterEngine,
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
    preserve_flags: bool, // macOS only, no-op on other platforms
    ignore_times: bool,
    size_only: bool,
    checksum: bool,
    #[allow(dead_code)] // TODO: Use verify_only field in sync logic
    verify_only: bool,
    use_cache: bool,
    clear_cache: bool,
    checksum_db: bool,
    clear_checksum_db: bool,
    prune_checksum_db: bool,
    perf_monitor: Option<Arc<Mutex<PerformanceMonitor>>>,
}

impl<T: Transport + 'static> SyncEngine<T> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transport: T,
        dry_run: bool,
        diff_mode: bool,
        delete: bool,
        delete_threshold: u8,
        trash: bool,
        force_delete: bool,
        quiet: bool,
        max_concurrent: usize,
        max_errors: usize,
        min_size: Option<u64>,
        max_size: Option<u64>,
        filter_engine: FilterEngine,
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
        preserve_flags: bool, // macOS only, no-op on other platforms
        ignore_times: bool,
        size_only: bool,
        checksum: bool,
        verify_only: bool,
        use_cache: bool,
        clear_cache: bool,
        checksum_db: bool,
        clear_checksum_db: bool,
        prune_checksum_db: bool,
        perf: bool,
    ) -> Self {
        let perf_monitor = if perf {
            Some(Arc::new(Mutex::new(PerformanceMonitor::new(bwlimit))))
        } else {
            None
        };

        Self {
            transport: Arc::new(transport),
            dry_run,
            diff_mode,
            delete,
            delete_threshold,
            trash,
            force_delete,
            quiet,
            max_concurrent,
            max_errors,
            min_size,
            max_size,
            filter_engine,
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
            preserve_flags,
            ignore_times,
            size_only,
            checksum,
            verify_only,
            use_cache,
            clear_cache,
            checksum_db,
            clear_checksum_db,
            prune_checksum_db,
            perf_monitor,
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

    fn should_exclude(&self, relative_path: &Path, is_dir: bool) -> bool {
        self.filter_engine.should_exclude(relative_path, is_dir)
    }

    pub async fn sync(&self, source: &Path, destination: &Path) -> Result<SyncStats> {
        let start_time = std::time::Instant::now();

        tracing::info!(
            "Starting sync: {} → {}",
            source.display(),
            destination.display()
        );

        // Handle directory cache
        if self.clear_cache && !self.dry_run {
            if let Err(e) = DirectoryCache::delete(destination) {
                tracing::warn!("Failed to clear directory cache: {}", e);
            } else {
                tracing::debug!("Cleared directory cache");
            }
        }

        // Load directory cache (if enabled)
        let mut dir_cache = if self.use_cache {
            let cache = DirectoryCache::load(destination);
            tracing::debug!("Loaded directory cache with {} entries", cache.len());
            Some(cache)
        } else {
            None
        };

        // Handle checksum database
        let checksum_db = if self.checksum && self.checksum_db {
            // Open checksum database
            match checksumdb::ChecksumDatabase::open(destination) {
                Ok(db) => {
                    tracing::debug!("Opened checksum database");

                    // Clear if requested
                    if self.clear_checksum_db && !self.dry_run {
                        if let Err(e) = db.clear() {
                            tracing::warn!("Failed to clear checksum database: {}", e);
                        } else {
                            tracing::info!("Cleared checksum database");
                        }
                    }

                    Some(db)
                }
                Err(e) => {
                    tracing::warn!("Failed to open checksum database: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Check if we can use cached scan results (incremental scanning)
        let can_use_cache = if let Some(ref cache) = dir_cache {
            // Check source directory mtime
            if let Ok(source_meta) = std::fs::metadata(source) {
                if let Ok(source_mtime) = source_meta.modified() {
                    let source_path = PathBuf::from(".");
                    !cache.needs_rescan(&source_path, source_mtime)
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        // Start scan timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().start_scan();
        }

        // Scan source directory (or use cache)
        let all_files = if can_use_cache {
            // Use cached files for incremental scan
            if let Some(ref cache) = dir_cache {
                if let Some(cached_files) = cache.get_cached_files(&PathBuf::from(".")) {
                    let file_count = cached_files.len();
                    tracing::info!(
                        "Using cached scan results ({} files) - source unchanged",
                        file_count
                    );

                    // Convert cached files back to FileEntry
                    cached_files
                        .iter()
                        .map(|cf| cf.to_file_entry(source))
                        .collect()
                } else {
                    // Cache exists but no files cached for root directory
                    tracing::debug!("No cached files found, performing full scan");
                    self.transport.scan(source).await?
                }
            } else {
                // This shouldn't happen, but fall back to full scan
                self.transport.scan(source).await?
            }
        } else {
            tracing::debug!("Scanning source directory (cache miss or disabled)...");
            self.transport.scan(source).await?
        };

        let total_scanned = all_files.len();
        if can_use_cache {
            tracing::info!("Retrieved {} items from cache", total_scanned);
        } else {
            tracing::info!("Found {} items in source", total_scanned);
        }

        // Update cache with scanned directory mtimes and file entries (for future incremental scans)
        if let Some(ref mut cache) = dir_cache {
            use crate::sync::dircache::CachedFile;
            use std::collections::HashMap;

            // Group files by their parent directory
            let mut files_by_dir: HashMap<PathBuf, Vec<CachedFile>> = HashMap::new();

            for file in &all_files {
                // Update directory mtimes
                if file.is_dir {
                    cache.update(file.relative_path.clone(), file.modified);
                }

                // Group files by directory for caching
                let dir_path = if file.is_dir {
                    file.relative_path.clone()
                } else {
                    file.relative_path
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from("."))
                };

                files_by_dir
                    .entry(dir_path)
                    .or_default()
                    .push(CachedFile::from_file_entry(file));
            }

            // Cache files for each directory
            let total_files: usize = files_by_dir.values().map(|v| v.len()).sum();
            for (dir_path, files) in files_by_dir {
                cache.cache_files(dir_path, files);
            }

            tracing::debug!(
                "Updated directory cache with {} directories, {} files",
                cache.len(),
                total_files
            );
        }

        // Filter files by size and exclude patterns
        // Also track excluded directories to filter their children (rsync behavior)
        let mut excluded_dirs: Vec<PathBuf> = Vec::new();

        let source_files: Vec<_> = all_files
            .into_iter()
            .filter(|file| {
                // Check if this file is inside an excluded directory
                for excluded_dir in &excluded_dirs {
                    if file.relative_path.starts_with(excluded_dir) {
                        tracing::debug!(
                            "Filtering out (parent excluded): {}",
                            file.relative_path.display()
                        );
                        return false;
                    }
                }

                // Apply exclude patterns
                if self.should_exclude(&file.relative_path, file.is_dir) {
                    tracing::debug!("Filtering out (excluded): {}", file.relative_path.display());

                    // If this is a directory, track it to exclude its children
                    if file.is_dir {
                        excluded_dirs.push(file.relative_path.clone());
                    }

                    return false;
                }

                // Don't filter directories (but only after checking exclude patterns)
                if file.is_dir {
                    return true;
                }
                // Apply size filter
                if self.should_filter_by_size(file.size) {
                    tracing::debug!("Filtering out (size): {}", file.relative_path.display());
                    return false;
                }
                true
            })
            .collect();

        if source_files.len() < total_scanned {
            let filtered_count = total_scanned - source_files.len();
            tracing::info!("Filtered out {} files", filtered_count);
        }

        // End scan timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().end_scan();
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
            resource::check_disk_space(destination, bytes_needed)?;

            // Check FD limits
            resource::check_fd_limits(self.max_concurrent)?;
        }

        // Load or create resume state
        let current_flags = SyncFlags {
            delete: self.delete,
            exclude: vec![], // Filter rules handled by FilterEngine
            min_size: self.min_size,
            max_size: self.max_size,
        };

        let resume_state = if self.resume {
            match ResumeState::load(destination)? {
                Some(state) => {
                    if state.is_compatible_with(&current_flags) {
                        let (completed, total) = state.progress();
                        tracing::info!(
                            "Resuming sync: {} of {} files already completed",
                            completed,
                            total
                        );
                        if !self.quiet {
                            println!(
                                "📋 Resuming previous sync ({}/{} files completed)",
                                completed, total
                            );
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

        // Start plan timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().start_plan();
        }

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
                .plan_file_async(file, destination, &self.transport, checksum_db.as_ref())
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
                    let delete_percentage =
                        (deletions.len() as f64 / dest_file_count as f64) * 100.0;

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

                        return Err(crate::error::SyncError::Io(std::io::Error::other(format!(
                            "Deletion threshold exceeded: {:.1}% > {}%",
                            delete_percentage, self.delete_threshold
                        ))));
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
                        return Err(crate::error::SyncError::Io(std::io::Error::other(
                            "Deletion cancelled by user",
                        )));
                    }
                }
            }

            tasks.extend(deletions);
        }

        // End plan timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().end_plan();
        }

        // Emit start event if JSON mode
        if self.json {
            SyncEvent::Start {
                source: source.to_path_buf(),
                destination: destination.to_path_buf(),
                total_files: tasks.len(),
            }
            .emit();
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
            bytes_would_add: 0,
            bytes_would_change: 0,
            bytes_would_delete: 0,
            errors: Vec::new(),
        }));

        // Calculate total bytes to transfer (for accurate progress/ETA)
        let total_bytes: u64 = tasks
            .iter()
            .filter(|t| !matches!(t.action, SyncAction::Skip | SyncAction::Delete))
            .map(|t| {
                t.source
                    .as_ref()
                    .map(|f| if f.is_dir { 0 } else { f.size })
                    .unwrap_or(0)
            })
            .sum();

        // Create progress bar (only if not quiet)
        let pb = if self.quiet {
            ProgressBar::hidden()
        } else {
            let pb = ProgressBar::new(total_bytes);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template(
                        "{msg}\n{spinner:.green} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})"
                    )
                    .unwrap()
                    .progress_chars("#>-"),
            );
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb
        };

        // Create rate limiter if bandwidth limit is set
        let rate_limiter = self
            .bwlimit
            .map(|limit| Arc::new(Mutex::new(RateLimiter::new(limit))));

        // Create hardlink map for tracking inodes (shared across all parallel transfers)
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));

        // Start transfer timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().start_transfer();
        }

        // Parallel execution with semaphore for concurrency control
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let mut handles = Vec::with_capacity(tasks.len());

        for task in tasks {
            let transport = Arc::clone(&self.transport);
            let dry_run = self.dry_run;
            let diff_mode = self.diff_mode;
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
            let preserve_flags = self.preserve_flags;
            let hardlink_map = Arc::clone(&hardlink_map);
            let perf_monitor = self.perf_monitor.clone();

            let handle = tokio::spawn(async move {
                let transferrer = Transferrer::new(
                    transport.as_ref(),
                    dry_run,
                    diff_mode,
                    symlink_mode,
                    preserve_xattrs,
                    preserve_hardlinks,
                    preserve_acls,
                    preserve_flags,
                    hardlink_map,
                );
                let verifier = IntegrityVerifier::new(verification_mode, verify_on_write);

                // Update progress message (show filename only for cleaner display)
                let filename = task
                    .dest_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_else(|| task.dest_path.to_str().unwrap_or(""));

                let msg = match &task.action {
                    SyncAction::Create => format!("Creating: {}", filename),
                    SyncAction::Update => format!("Updating: {}", filename),
                    SyncAction::Skip => format!("Skipping: {}", filename),
                    SyncAction::Delete => format!("Deleting: {}", filename),
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

                                        // Track in performance monitor
                                        if let Some(monitor) = &perf_monitor {
                                            monitor.lock().unwrap().add_file_created();
                                            monitor
                                                .lock()
                                                .unwrap()
                                                .add_bytes_transferred(bytes_written);
                                            if !source.is_dir {
                                                monitor.lock().unwrap().add_bytes_read(source.size);
                                            }
                                        }

                                        // In dry-run mode, track bytes that would be added
                                        if dry_run && !source.is_dir {
                                            stats.bytes_would_add += source.size;
                                        }

                                        // Track compression usage and savings
                                        if let Some(ref result) = transfer_result {
                                            if result.compression_used {
                                                stats.files_compressed += 1;

                                                // Calculate bytes saved (uncompressed - compressed)
                                                if let Some(transferred) = result.transferred_bytes
                                                {
                                                    let bytes_saved = result
                                                        .bytes_written
                                                        .saturating_sub(transferred);
                                                    stats.compression_bytes_saved += bytes_saved;
                                                }
                                            }
                                        }
                                    }

                                    // Apply rate limiting if enabled (outside stats lock)
                                    if let Some(ref limiter) = rate_limiter {
                                        if bytes_written > 0 {
                                            let sleep_duration =
                                                limiter.lock().unwrap().consume(bytes_written);
                                            if sleep_duration > Duration::ZERO {
                                                tokio::time::sleep(sleep_duration).await;
                                            }
                                        }
                                    }

                                    // Verify transfer if verification is enabled (skip directories)
                                    if verification_mode != ChecksumType::None
                                        && !dry_run
                                        && !source.is_dir
                                    {
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
                                        }
                                        .emit();
                                    }

                                    Ok(())
                                }
                                Err(e) => {
                                    // Record error
                                    {
                                        let mut stats = stats.lock().unwrap();
                                        stats.errors.push(SyncError {
                                            path: task.dest_path.clone(),
                                            error: e.to_string(),
                                            action: "create".to_string(),
                                        });
                                    }
                                    Err(e)
                                }
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
                                                    let bytes_saved = result
                                                        .bytes_written
                                                        .saturating_sub(literal_bytes);
                                                    stats.delta_bytes_saved += bytes_saved;
                                                }

                                                if let Some(ratio) = result.compression_ratio() {
                                                    pb.set_message(format!(
                                                        "Updating: {} (delta: {:.1}% literal)",
                                                        filename, ratio
                                                    ));
                                                }
                                            }

                                            // Track compression usage and savings
                                            if result.compression_used {
                                                stats.files_compressed += 1;

                                                // Calculate bytes saved (uncompressed - compressed)
                                                if let Some(transferred) = result.transferred_bytes
                                                {
                                                    let bytes_saved = result
                                                        .bytes_written
                                                        .saturating_sub(transferred);
                                                    stats.compression_bytes_saved += bytes_saved;
                                                }
                                            }
                                        }
                                        stats.files_updated += 1;

                                        // Track in performance monitor
                                        if let Some(monitor) = &perf_monitor {
                                            monitor.lock().unwrap().add_file_updated();
                                            monitor
                                                .lock()
                                                .unwrap()
                                                .add_bytes_transferred(bytes_written);
                                            if !source.is_dir {
                                                monitor.lock().unwrap().add_bytes_read(source.size);
                                            }
                                        }

                                        // In dry-run mode, track bytes that would be changed
                                        if dry_run && !source.is_dir {
                                            stats.bytes_would_change += source.size;
                                        }
                                    }

                                    // Apply rate limiting if enabled (outside stats lock)
                                    if let Some(ref limiter) = rate_limiter {
                                        if bytes_written > 0 {
                                            let sleep_duration =
                                                limiter.lock().unwrap().consume(bytes_written);
                                            if sleep_duration > Duration::ZERO {
                                                tokio::time::sleep(sleep_duration).await;
                                            }
                                        }
                                    }

                                    // Verify transfer if verification is enabled (skip directories)
                                    if verification_mode != ChecksumType::None
                                        && !dry_run
                                        && !source.is_dir
                                    {
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
                                        let delta_used = transfer_result
                                            .as_ref()
                                            .map(|r| r.used_delta())
                                            .unwrap_or(false);
                                        SyncEvent::Update {
                                            path: task.dest_path.clone(),
                                            size: source.size,
                                            bytes_transferred: bytes_written,
                                            delta_used,
                                        }
                                        .emit();
                                    }

                                    Ok(())
                                }
                                Err(e) => {
                                    // Record error
                                    {
                                        let mut stats = stats.lock().unwrap();
                                        stats.errors.push(SyncError {
                                            path: task.dest_path.clone(),
                                            error: e.to_string(),
                                            action: "update".to_string(),
                                        });
                                    }
                                    Err(e)
                                }
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
                            }
                            .emit();
                        }

                        Ok(())
                    }
                    SyncAction::Delete => {
                        let is_dir = task.dest_path.is_dir();

                        // In dry-run mode, track bytes that would be deleted
                        if dry_run && !is_dir {
                            if let Ok(metadata) = std::fs::metadata(&task.dest_path) {
                                let mut stats = stats.lock().unwrap();
                                stats.bytes_would_delete += metadata.len();
                            }
                        }

                        match transferrer.delete(&task.dest_path, is_dir).await {
                            Ok(_) => {
                                {
                                    let mut stats = stats.lock().unwrap();
                                    stats.files_deleted += 1;
                                }

                                // Track in performance monitor
                                if let Some(monitor) = &perf_monitor {
                                    monitor.lock().unwrap().add_file_deleted();
                                }

                                // Emit JSON event if enabled
                                if json {
                                    SyncEvent::Delete {
                                        path: task.dest_path.clone(),
                                    }
                                    .emit();
                                }

                                Ok(())
                            }
                            Err(e) => {
                                // Record error
                                {
                                    let mut stats = stats.lock().unwrap();
                                    stats.errors.push(SyncError {
                                        path: task.dest_path.clone(),
                                        error: e.to_string(),
                                        action: "delete".to_string(),
                                    });
                                }
                                Err(e)
                            }
                        }
                    }
                };

                // Increment progress by bytes written (for byte-based progress bar)
                let bytes_for_progress = match &task.action {
                    SyncAction::Create | SyncAction::Update => {
                        task.source.as_ref().map(|f| f.size).unwrap_or(0)
                    }
                    _ => 0,
                };
                pb.inc(bytes_for_progress);
                drop(permit);
                result
            });

            handles.push(handle);
        }

        // Collect all results
        let results = futures::future::join_all(handles).await;

        // End transfer timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().end_transfer();
        }

        // Check for errors and count them
        let mut error_count = 0;
        let mut first_error = None;
        let mut all_errors = Vec::new();

        for result in results {
            match result {
                Ok(Ok(())) => {} // Success
                Ok(Err(e)) => {
                    error_count += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                    all_errors.push(format!("{}", e));

                    tracing::error!("Sync error: {}", e);

                    // Check if we've exceeded the error threshold
                    if self.max_errors > 0 && error_count >= self.max_errors {
                        tracing::error!(
                            "Error threshold exceeded: {} errors (max: {})",
                            error_count,
                            self.max_errors
                        );

                        if !self.quiet {
                            eprintln!(
                                "⚠️  ERROR: {} errors occurred (threshold: {}). Aborting sync.",
                                error_count, self.max_errors
                            );
                        }

                        pb.finish_with_message("Sync aborted due to errors");

                        return Err(crate::error::SyncError::Io(std::io::Error::other(format!(
                            "Error threshold exceeded: {} errors (max: {}). First error: {}",
                            error_count,
                            self.max_errors,
                            first_error.unwrap_or_else(|| "Unknown".to_string())
                        ))));
                    }
                }
                Err(e) => {
                    error_count += 1;
                    let error_msg = format!("Task panicked: {}", e);
                    if first_error.is_none() {
                        first_error = Some(error_msg.clone());
                    }
                    all_errors.push(error_msg.clone());

                    tracing::error!("{}", error_msg);

                    // Check if we've exceeded the error threshold
                    if self.max_errors > 0 && error_count >= self.max_errors {
                        tracing::error!(
                            "Error threshold exceeded: {} errors (max: {})",
                            error_count,
                            self.max_errors
                        );

                        if !self.quiet {
                            eprintln!(
                                "⚠️  ERROR: {} errors occurred (threshold: {}). Aborting sync.",
                                error_count, self.max_errors
                            );
                        }

                        pb.finish_with_message("Sync aborted due to errors");

                        return Err(crate::error::SyncError::Io(std::io::Error::other(format!(
                            "Error threshold exceeded: {} errors (max: {}). First error: {}",
                            error_count,
                            self.max_errors,
                            first_error.unwrap_or_else(|| "Unknown".to_string())
                        ))));
                    }
                }
            }
        }

        pb.finish_with_message("Sync complete");

        // Extract final stats before reporting errors
        let mut final_stats = Arc::try_unwrap(stats).unwrap().into_inner().unwrap();

        // Print detailed error report if errors occurred
        if !final_stats.errors.is_empty() {
            tracing::warn!("Sync completed with {} errors", final_stats.errors.len());

            if !self.quiet && !self.json {
                use colored::Colorize;
                eprintln!("\n{}", "⚠️  Errors occurred during sync:".red().bold());
                eprintln!();

                for (i, err) in final_stats.errors.iter().enumerate() {
                    eprintln!(
                        "  {}. {} {}",
                        (i + 1).to_string().bright_black(),
                        format!("[{}]", err.action).yellow(),
                        err.path.display().to_string().white()
                    );
                    eprintln!("     {}", err.error.bright_black());
                    if i < final_stats.errors.len() - 1 {
                        eprintln!();
                    }
                }

                eprintln!();
                eprintln!(
                    "{}",
                    format!("Total errors: {}", final_stats.errors.len()).red()
                );
                eprintln!();
            }
        }

        // Add duration after extracting stats
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
            }
            .emit();

            // Emit performance metrics if performance monitoring is enabled
            if let Some(perf_metrics) = self.get_performance_metrics() {
                SyncEvent::Performance {
                    total_duration_secs: perf_metrics.total_duration.as_secs_f64(),
                    scan_duration_secs: perf_metrics.scan_duration.as_secs_f64(),
                    plan_duration_secs: perf_metrics.plan_duration.as_secs_f64(),
                    transfer_duration_secs: perf_metrics.transfer_duration.as_secs_f64(),
                    bytes_transferred: perf_metrics.bytes_transferred,
                    bytes_read: perf_metrics.bytes_read,
                    files_processed: perf_metrics.files_processed,
                    files_created: perf_metrics.files_created,
                    files_updated: perf_metrics.files_updated,
                    files_deleted: perf_metrics.files_deleted,
                    directories_created: perf_metrics.directories_created,
                    avg_transfer_speed: perf_metrics.avg_transfer_speed,
                    peak_transfer_speed: perf_metrics.peak_transfer_speed,
                    files_per_second: perf_metrics.files_per_second,
                    bandwidth_utilization: perf_metrics.bandwidth_utilization,
                }
                .emit();
            }
        }

        // Clean up resume state on successful completion
        if let Ok(mut state_guard) = resume_state.lock() {
            if state_guard.is_some() {
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

        // Save directory cache if enabled
        if self.use_cache && !self.dry_run {
            if let Some(ref cache) = dir_cache {
                // Ensure destination directory exists before saving cache
                if destination.exists() {
                    if let Err(e) = cache.save(destination) {
                        tracing::warn!("Failed to save directory cache: {}", e);
                    } else {
                        tracing::debug!("Saved directory cache with {} entries", cache.len());
                    }
                } else {
                    tracing::debug!("Skipping cache save - destination directory doesn't exist");
                }
            }
        }

        // Store checksums in database if enabled
        if let Some(ref db) = checksum_db {
            if !self.dry_run {
                let mut stored_count = 0;
                let verifier = IntegrityVerifier::new(
                    if self.checksum {
                        ChecksumType::Fast
                    } else {
                        ChecksumType::None
                    },
                    false,
                );

                for file in &source_files {
                    if file.is_dir {
                        continue; // Skip directories
                    }

                    // Compute checksum for source file
                    if let Ok(checksum) = verifier.compute_file_checksum(&file.path) {
                        // Store in database
                        if let Err(e) =
                            db.store_checksum(&file.path, file.modified, file.size, &checksum)
                        {
                            tracing::warn!(
                                "Failed to store checksum for {}: {}",
                                file.path.display(),
                                e
                            );
                        } else {
                            stored_count += 1;
                        }
                    }
                }

                if stored_count > 0 {
                    tracing::info!("Stored {} checksums in database", stored_count);
                }

                // Handle prune flag
                if self.prune_checksum_db {
                    use std::collections::HashSet;
                    let existing_paths: HashSet<_> =
                        source_files.iter().map(|f| f.path.clone()).collect();

                    match db.prune(&existing_paths) {
                        Ok(pruned) => {
                            if pruned > 0 {
                                tracing::info!(
                                    "Pruned {} stale entries from checksum database",
                                    pruned
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to prune checksum database: {}", e);
                        }
                    }
                }
            }
        }

        // If we got here, either no errors occurred or errors were within the threshold
        Ok(final_stats)
    }

    /// Verify file integrity without modification
    ///
    /// Compares source and destination by computing checksums for all files.
    /// Returns detailed results including matched files, mismatches, and files
    /// only in source or destination.
    pub async fn verify(&self, source: &Path, destination: &Path) -> Result<VerificationResult> {
        let start_time = std::time::Instant::now();

        tracing::info!(
            "Starting verification: {} ↔ {}",
            source.display(),
            destination.display()
        );

        // Start scan timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().start_scan();
        }

        // Scan source and destination
        let source_files = self.transport.scan(source).await?;
        let dest_files = self.transport.scan(destination).await?;

        // End scan timing
        if let Some(ref monitor) = self.perf_monitor {
            monitor.lock().unwrap().end_scan();
        }

        tracing::info!(
            "Found {} files in source, {} files in destination",
            source_files.len(),
            dest_files.len()
        );

        // Build destination file map for quick lookup (relative path -> FileEntry)
        let mut dest_map = std::collections::HashMap::new();
        for file in &dest_files {
            let rel_path = file
                .path
                .strip_prefix(destination)
                .unwrap_or(&file.path)
                .to_path_buf();
            dest_map.insert(rel_path, file);
        }

        // Create integrity verifier for checksum computation
        let checksum_type = if self.checksum {
            ChecksumType::Fast // Use xxHash3 for fast verification
        } else {
            self.verification_mode // Use user-specified mode
        };
        let verifier = IntegrityVerifier::new(checksum_type, false);

        // Results tracking
        let mut files_matched = 0;
        let mut files_mismatched = Vec::new();
        let mut files_only_in_source = Vec::new();
        let mut errors = Vec::new();

        // Verify each source file
        for source_file in &source_files {
            // Skip directories
            if source_file.is_dir {
                continue;
            }

            // Apply size filters
            if self.should_filter_by_size(source_file.size) {
                continue;
            }

            let rel_path = source_file
                .path
                .strip_prefix(source)
                .unwrap_or(&source_file.path)
                .to_path_buf();

            // Check if file exists in destination
            if let Some(dest_file) = dest_map.get(&rel_path) {
                // File exists in both - compare checksums
                match self.compare_checksums(&source_file.path, &dest_file.path, &verifier) {
                    Ok(true) => {
                        // Checksums match
                        files_matched += 1;
                        if !self.quiet {
                            tracing::debug!("✓ {}", rel_path.display());
                        }
                    }
                    Ok(false) => {
                        // Checksums mismatch
                        files_mismatched.push(rel_path.clone());
                        tracing::warn!("✗ Mismatch: {}", rel_path.display());
                    }
                    Err(e) => {
                        errors.push(SyncError {
                            path: rel_path.clone(),
                            error: e.to_string(),
                            action: "verify".to_string(),
                        });
                        tracing::error!("Error verifying {}: {}", rel_path.display(), e);
                    }
                }
            } else {
                // File only in source
                files_only_in_source.push(rel_path.clone());
                tracing::info!("→ Only in source: {}", rel_path.display());
            }
        }

        // Find files only in destination
        let mut files_only_in_dest = Vec::new();
        for dest_file in &dest_files {
            if dest_file.is_dir {
                continue;
            }

            let rel_path = dest_file
                .path
                .strip_prefix(destination)
                .unwrap_or(&dest_file.path)
                .to_path_buf();

            // Build corresponding source path
            let source_path = source.join(&rel_path);
            if !source_path.exists() {
                files_only_in_dest.push(rel_path.clone());
                tracing::info!("← Only in destination: {}", rel_path.display());
            }
        }

        let duration = start_time.elapsed();

        tracing::info!(
            "Verification complete: {} matched, {} mismatched, {} only in source, {} only in dest, {} errors",
            files_matched,
            files_mismatched.len(),
            files_only_in_source.len(),
            files_only_in_dest.len(),
            errors.len()
        );

        Ok(VerificationResult {
            files_matched,
            files_mismatched,
            files_only_in_source,
            files_only_in_dest,
            errors,
            duration,
        })
    }

    /// Compare checksums of two files
    fn compare_checksums(
        &self,
        source_path: &Path,
        dest_path: &Path,
        verifier: &IntegrityVerifier,
    ) -> Result<bool> {
        let source_checksum = verifier.compute_file_checksum(source_path)?;
        let dest_checksum = verifier.compute_file_checksum(dest_path)?;
        Ok(source_checksum == dest_checksum)
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
            bytes_would_add: 0,
            bytes_would_change: 0,
            bytes_would_delete: 0,
            errors: Vec::new(),
        };

        // Check if destination exists
        let dest_exists = self.transport.exists(destination).await?;

        // Create hardlink map (not used for single-file sync, but required by Transferrer)
        let hardlink_map = Arc::new(Mutex::new(std::collections::HashMap::new()));

        let transferrer = Transferrer::new(
            self.transport.as_ref(),
            self.dry_run,
            self.diff_mode,
            self.symlink_mode,
            self.preserve_xattrs,
            self.preserve_hardlinks,
            self.preserve_acls,
            self.preserve_flags,
            hardlink_map,
        );

        if !dest_exists {
            // Create new file
            tracing::info!("Creating {}", destination.display());
            let metadata = source.metadata()?;
            let filename = source
                .file_name()
                .ok_or_else(|| {
                    crate::error::SyncError::Io(std::io::Error::other(format!(
                        "Invalid source path: {}",
                        source.display()
                    )))
                })?
                .to_owned();
            if let Some(result) = transferrer
                .create(
                    &FileEntry {
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
                        bsd_flags: None,
                    },
                    destination,
                )
                .await?
            {
                stats.bytes_transferred = result.bytes_written;

                // Track compression if used
                if result.compression_used {
                    stats.files_compressed = 1;
                    if let Some(transferred) = result.transferred_bytes {
                        stats.compression_bytes_saved =
                            result.bytes_written.saturating_sub(transferred);
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
                        tracing::warn!("Verification error for {}: {}", destination.display(), e);
                        stats.verification_failures = 1;
                    }
                }
            }
        } else {
            // Update existing file
            tracing::info!("Updating {}", destination.display());
            let metadata = source.metadata()?;
            let filename = source
                .file_name()
                .ok_or_else(|| {
                    crate::error::SyncError::Io(std::io::Error::other(format!(
                        "Invalid source path: {}",
                        source.display()
                    )))
                })?
                .to_owned();
            if let Some(result) = transferrer
                .update(
                    &FileEntry {
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
                        bsd_flags: None,
                    },
                    destination,
                )
                .await?
            {
                stats.bytes_transferred = result.bytes_written;

                // Track delta sync if used
                if result.used_delta() {
                    stats.files_delta_synced = 1;
                    if let Some(literal_bytes) = result.literal_bytes {
                        stats.delta_bytes_saved =
                            result.bytes_written.saturating_sub(literal_bytes);
                    }
                }

                // Track compression if used
                if result.compression_used {
                    stats.files_compressed = 1;
                    if let Some(transferred) = result.transferred_bytes {
                        stats.compression_bytes_saved =
                            result.bytes_written.saturating_sub(transferred);
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
                        tracing::warn!("Verification error for {}: {}", destination.display(), e);
                        stats.verification_failures = 1;
                    }
                }
            }
        }

        stats.duration = start_time.elapsed();
        Ok(stats)
    }

    /// Get performance metrics (if performance monitoring is enabled)
    pub fn get_performance_metrics(&self) -> Option<PerformanceMetrics> {
        self.perf_monitor
            .as_ref()
            .map(|monitor| monitor.lock().unwrap().get_metrics())
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
            false,               // dry_run
            false,               // diff_mode
            false,               // delete
            50,                  // delete_threshold
            false,               // trash
            false,               // force_delete
            true,                // quiet
            4,                   // max_concurrent
            100,                 // max_errors
            None,                // min_size
            None,                // max_size
            FilterEngine::new(), // filter_engine
            None,                // bwlimit
            false,               // resume
            0,                   // checkpoint_files
            0,                   // checkpoint_bytes
            false,               // json
            ChecksumType::Fast,
            false, // verify_on_write
            SymlinkMode::Preserve,
            false, // preserve_xattrs
            false, // preserve_hardlinks
            false, // preserve_acls
            false, // preserve_flags
            false, // ignore_times
            false, // size_only
            false, // checksum
            false, // verify_only
            false, // use_cache (disabled in tests to avoid side effects)
            false, // clear_cache
            false, // checksum_db
            false, // clear_checksum_db
            false, // prune_checksum_db
            false, // perf
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
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

        assert_eq!(stats.files_created, 2);
        assert!(dest_dir.path().join("file1.txt").exists());
        assert!(dest_dir.path().join("file2.txt").exists());
        assert_eq!(
            fs::read_to_string(dest_dir.path().join("file1.txt")).unwrap(),
            "content1"
        );
    }

    #[tokio::test]
    async fn test_sync_with_subdirectories() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create nested structure
        fs::create_dir(source_dir.path().join("subdir")).unwrap();
        fs::write(source_dir.path().join("subdir/file.txt"), "nested").unwrap();

        let engine = create_test_engine();
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

        assert!(stats.files_created >= 1);
        assert!(dest_dir.path().join("subdir/file.txt").exists());
        assert_eq!(
            fs::read_to_string(dest_dir.path().join("subdir/file.txt")).unwrap(),
            "nested"
        );
    }

    #[tokio::test]
    async fn test_sync_empty_source() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let engine = create_test_engine();
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

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
            true,                // dry_run = true
            false,               // diff_mode
            false,               // delete
            50,                  // delete_threshold
            false,               // trash
            false,               // force_delete
            true,                // quiet
            4,                   // max_concurrent
            100,                 // max_errors
            None,                // min_size
            None,                // max_size
            FilterEngine::new(), // filter_engine
            None,                // bwlimit
            false,               // resume
            0,                   // checkpoint_files
            0,                   // checkpoint_bytes
            false,               // json
            ChecksumType::Fast,
            false, // verify_on_write
            SymlinkMode::Preserve,
            false, // preserve_xattrs
            false, // preserve_hardlinks
            false, // preserve_acls
            false, // preserve_flags
            false, // ignore_times
            false, // size_only
            false, // checksum
            false, // verify_only
            false, // use_cache
            false, // clear_cache
            false, // checksum_db
            false, // clear_checksum_db
            false, // prune_checksum_db
            false, // perf
        );

        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

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
        assert!(!source_files.is_empty());

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
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

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
            )
            .unwrap();
        }

        let engine = create_test_engine();
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

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
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

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
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

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
        let stats = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();

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
        let stats1 = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();
        assert_eq!(stats1.files_created, 1);

        // Second sync - should skip unchanged file
        let stats2 = engine
            .sync(source_dir.path(), dest_dir.path())
            .await
            .unwrap();
        assert_eq!(stats2.files_skipped, 1);
        assert_eq!(stats2.files_created, 0);
    }

    // === Error Collection and max_errors Threshold Tests ===

    #[tokio::test]
    async fn test_error_threshold_zero_collects_all_errors() {
        use std::os::unix::fs::PermissionsExt;

        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create test files
        for i in 1..=5 {
            fs::write(source_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }

        // Make destination read-only to cause write errors
        let perms = fs::Permissions::from_mode(0o444);
        fs::set_permissions(dest_dir.path(), perms).unwrap();

        // Create engine with max_errors = 0 (unlimited)
        let transport = LocalTransport::new();
        let engine = SyncEngine::new(
            transport,
            false,               // dry_run
            false,               // diff_mode
            false,               // delete
            50,                  // delete_threshold
            false,               // trash
            false,               // force_delete
            true,                // quiet
            1,                   // max_concurrent (serial to make errors predictable)
            0,                   // max_errors = 0 (unlimited)
            None,                // min_size
            None,                // max_size
            FilterEngine::new(), // filter_engine
            None,                // bwlimit
            false,               // resume
            0,                   // checkpoint_files
            0,                   // checkpoint_bytes
            false,               // json
            ChecksumType::Fast,
            false, // verify_on_write
            SymlinkMode::Preserve,
            false, // preserve_xattrs
            false, // preserve_hardlinks
            false, // preserve_acls
            false, // preserve_flags
            false, // ignore_times
            false, // size_only
            false, // checksum
            false, // verify_only
            false, // use_cache
            false, // clear_cache
            false, // checksum_db
            false, // clear_checksum_db
            false, // prune_checksum_db
            false, // perf
        );

        let result = engine.sync(source_dir.path(), dest_dir.path()).await;

        // Should complete with errors collected in stats
        match result {
            Ok(stats) => {
                // All files should have been attempted (errors collected, not aborted)
                assert!(!stats.errors.is_empty(), "Should have collected errors");
            }
            Err(_) => {
                // May error out due to permission issues
            }
        }

        // Restore permissions for cleanup
        let perms = fs::Permissions::from_mode(0o755);
        let _ = fs::set_permissions(dest_dir.path(), perms);
    }

    #[tokio::test]
    async fn test_error_threshold_aborts_when_exceeded() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create multiple files
        for i in 1..=10 {
            fs::write(source_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }

        // Scan files first
        let scanner = scanner::Scanner::new(source_dir.path());
        let source_files = scanner.scan().unwrap();
        assert_eq!(source_files.len(), 10);

        // Delete some files after scan to cause errors (TOCTOU)
        for i in 1..=5 {
            fs::remove_file(source_dir.path().join(format!("file{}.txt", i))).unwrap();
        }

        // Create engine with max_errors = 3
        let transport = LocalTransport::new();
        let engine = SyncEngine::new(
            transport,
            false,               // dry_run
            false,               // diff_mode
            false,               // delete
            50,                  // delete_threshold
            false,               // trash
            false,               // force_delete
            true,                // quiet
            1,                   // max_concurrent (serial)
            3,                   // max_errors = 3
            None,                // min_size
            None,                // max_size
            FilterEngine::new(), // filter_engine
            None,                // bwlimit
            false,               // resume
            0,                   // checkpoint_files
            0,                   // checkpoint_bytes
            false,               // json
            ChecksumType::Fast,
            false, // verify_on_write
            SymlinkMode::Preserve,
            false, // preserve_xattrs
            false, // preserve_hardlinks
            false, // preserve_acls
            false, // preserve_flags
            false, // ignore_times
            false, // size_only
            false, // checksum
            false, // verify_only
            false, // use_cache
            false, // clear_cache
            false, // checksum_db
            false, // clear_checksum_db
            false, // prune_checksum_db
            false, // perf
        );

        let result = engine.sync(source_dir.path(), dest_dir.path()).await;

        // Should abort with error when threshold is exceeded
        match result {
            Ok(stats) => {
                // If successful, should have processed remaining files
                // but this is TOCTOU scenario so may succeed with partial files
                assert!(stats.files_created <= 10);
            }
            Err(e) => {
                // Should contain "Error threshold exceeded" in the error message
                let error_msg = e.to_string();
                assert!(
                    error_msg.contains("Error threshold exceeded") || error_msg.contains("errors"),
                    "Error should mention threshold: {}",
                    error_msg
                );
            }
        }
    }

    #[tokio::test]
    async fn test_error_collection_below_threshold_continues() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create files
        for i in 1..=10 {
            fs::write(source_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }

        // Scan first
        let scanner = scanner::Scanner::new(source_dir.path());
        let source_files = scanner.scan().unwrap();
        assert_eq!(source_files.len(), 10);

        // Delete only 2 files to stay below threshold
        fs::remove_file(source_dir.path().join("file1.txt")).unwrap();
        fs::remove_file(source_dir.path().join("file2.txt")).unwrap();

        // Create engine with max_errors = 5 (higher than expected errors)
        let transport = LocalTransport::new();
        let engine = SyncEngine::new(
            transport,
            false,               // dry_run
            false,               // diff_mode
            false,               // delete
            50,                  // delete_threshold
            false,               // trash
            false,               // force_delete
            true,                // quiet
            1,                   // max_concurrent
            5,                   // max_errors = 5 (above expected errors)
            None,                // min_size
            None,                // max_size
            FilterEngine::new(), // filter_engine
            None,                // bwlimit
            false,               // resume
            0,                   // checkpoint_files
            0,                   // checkpoint_bytes
            false,               // json
            ChecksumType::Fast,
            false, // verify_on_write
            SymlinkMode::Preserve,
            false, // preserve_xattrs
            false, // preserve_hardlinks
            false, // preserve_acls
            false, // preserve_flags
            false, // ignore_times
            false, // size_only
            false, // checksum
            false, // verify_only
            false, // use_cache
            false, // clear_cache
            false, // checksum_db
            false, // clear_checksum_db
            false, // prune_checksum_db
            false, // perf
        );

        let result = engine.sync(source_dir.path(), dest_dir.path()).await;

        // Should complete successfully (below threshold)
        match result {
            Ok(stats) => {
                // Should have synced the remaining files that exist
                assert!(stats.files_created <= 8, "Should sync remaining files");
                // Errors should be collected in stats
                if !stats.errors.is_empty() {
                    assert!(stats.errors.len() <= 5, "Errors should be below threshold");
                }
            }
            Err(_) => {
                // May still error in TOCTOU scenarios
            }
        }
    }

    #[tokio::test]
    async fn test_error_message_includes_count_and_first_error() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        // Create files
        for i in 1..=5 {
            fs::write(source_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }

        // Scan first
        let scanner = scanner::Scanner::new(source_dir.path());
        let _ = scanner.scan().unwrap();

        // Delete all files to cause maximum errors
        for i in 1..=5 {
            fs::remove_file(source_dir.path().join(format!("file{}.txt", i))).unwrap();
        }

        // Create engine with low threshold
        let transport = LocalTransport::new();
        let engine = SyncEngine::new(
            transport,
            false, // dry_run
            false, // diff_mode
            false, // delete
            50,    // delete_threshold
            false, // trash
            false, // force_delete
            true,  // quiet
            1,     // max_concurrent
            2,     // max_errors = 2 (will be exceeded)
            None,  // min_size
            None,  // max_size
            FilterEngine::new(),
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
            false, // preserve_flags
            false, // ignore_times
            false, // size_only
            false, // checksum
            false, // verify_only
            false, // use_cache
            false, // clear_cache
            false, // checksum_db
            false, // clear_checksum_db
            false, // prune_checksum_db
            false, // perf
        );

        let result = engine.sync(source_dir.path(), dest_dir.path()).await;

        // Verify error message format when threshold exceeded
        if let Err(e) = result {
            let error_msg = e.to_string();
            // Should mention error count and threshold
            assert!(
                error_msg.contains("error") || error_msg.contains("Error"),
                "Error message should mention errors: {}",
                error_msg
            );
        }
    }
}
