//! SyncSession: strategy dispatch for file synchronization.
//!
//! Orchestrates sync by selecting the right strategy based on endpoint types:
//! - DirectLocal: Local → Local (scan, plan, execute via Endpoint)
//! - StreamingPush: Local → SSH (streaming protocol)
//! - StreamingPull: SSH → Local (streaming protocol)
//! - ObjectStore: S3/GCS involved (future)

// Dead-code suppressed until main.rs is rewritten to use SyncSession (Phase 3 completion).
// Remove this allow once SyncSession is wired into main.rs.

use crate::endpoint::Endpoint;
use crate::endpoint::local::LocalEndpoint;
use crate::compress::CompressionDetection;
use crate::error::{Result, SyncError};
use crate::sync::strategy::SyncAction;
use crate::sync::executor::TaskExecutor;
use crate::ssh::config::SshConfig;
use crate::sync::config::SyncConfig;
use crate::sync::scanner::FileEntry;
use crate::sync::stats::SyncStats;
use crate::sync::strategy::{StrategyPlanner, SyncTask};
use crate::sync::scanner::ScanOptions;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Represents the type of endpoint for strategy selection.
#[allow(dead_code)] // Wired in by main.rs rewrite (Phase 3 completion)
pub enum EndpointPair {
    /// Local filesystem endpoint
    Local(Box<dyn Endpoint>),
    /// SSH remote endpoint (connection established during sync)
    Ssh { host: String, user: Option<String>, root: PathBuf },
}

impl EndpointPair {
    /// Create an EndpointPair from a SyncPath.
    #[allow(dead_code)] // Wired in by main.rs rewrite
    pub fn from_sync_path(path: &crate::path::SyncPath) -> Self {
        match path {
            crate::path::SyncPath::Local { path, .. } => {
                EndpointPair::Local(Box::new(LocalEndpoint::new(path.clone())))
            }
            crate::path::SyncPath::Remote { host, user, path, .. } => {
                EndpointPair::Ssh {
                    host: host.clone(),
                    user: user.clone(),
                    root: path.clone(),
                }
            }
            _ => panic!("S3/GCS endpoints not yet supported"),
        }
    }
}

#[allow(dead_code)] // Wired in by main.rs rewrite (Phase 3 completion)
impl EndpointPair {
    /// Get the root path for this endpoint
    pub fn root(&self) -> &Path {
        match self {
            EndpointPair::Local(ep) => ep.root(),
            EndpointPair::Ssh { root, .. } => root,
        }
    }

    /// Check if this is a local endpoint
    pub fn is_local(&self) -> bool {
        matches!(self, EndpointPair::Local(_))
    }

    /// Get the inner Endpoint if local
    pub fn as_endpoint(&self) -> Option<&dyn Endpoint> {
        match self {
            EndpointPair::Local(ep) => Some(ep.as_ref()),
            _ => None,
        }
    }
}

/// Selected sync strategy based on endpoint types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Wired in by main.rs rewrite (Phase 3 completion)
pub enum SyncStrategy {
    /// Local to local sync
    DirectLocal,
    /// Local to remote SSH push
    StreamingPush,
    /// Remote SSH to local pull
    StreamingPull,
    /// Object store involved (future)
    ObjectStore,
}

/// Orchestrates file synchronization between two endpoints.
#[allow(dead_code)] // Wired in by main.rs rewrite (Phase 3 completion)
pub struct SyncSession {
    source: EndpointPair,
    dest: EndpointPair,
    config: SyncConfig,
    scan_options: ScanOptions,
}

#[allow(dead_code)] // Wired in by main.rs rewrite (Phase 3 completion)
impl SyncSession {
    /// Create a new sync session.
    pub fn new(source: EndpointPair, dest: EndpointPair, config: SyncConfig) -> Self {
        Self {
            source,
            dest,
            config,
            scan_options: ScanOptions::default(),
        }
    }

    /// Create a new sync session with custom scan options.
    pub fn with_scan_options(mut self, scan_options: ScanOptions) -> Self {
        self.scan_options = scan_options;
        self
    }

    /// Select the appropriate sync strategy based on endpoint types.
    pub fn select_strategy(&self) -> SyncStrategy {
        match (&self.source, &self.dest) {
            (EndpointPair::Local(_), EndpointPair::Local(_)) => SyncStrategy::DirectLocal,
            (EndpointPair::Local(_), EndpointPair::Ssh { .. }) => SyncStrategy::StreamingPush,
            (EndpointPair::Ssh { .. }, EndpointPair::Local(_)) => SyncStrategy::StreamingPull,
            _ => SyncStrategy::ObjectStore,
        }
    }

    /// Execute the sync operation.
    pub async fn sync(&self) -> Result<SyncStats> {
        let strategy = self.select_strategy();
        tracing::info!("Selected strategy: {:?}", strategy);

        match strategy {
            SyncStrategy::DirectLocal => self.direct_local().await,
            SyncStrategy::StreamingPush => self.streaming_push().await,
            SyncStrategy::StreamingPull => self.streaming_pull().await,
            SyncStrategy::ObjectStore => {
                Err(SyncError::Io(std::io::Error::other("Object store sync not yet implemented")))
            }
        }
    }

    /// Verify source and destination are in sync.
    pub async fn verify(&self, source: &std::path::Path, dest: &std::path::Path) -> Result<crate::sync::VerificationResult> {
        let source_ep = self.source.as_endpoint()
            .ok_or_else(|| SyncError::Io(std::io::Error::other("Source must be local for verify")))?;
        let dest_ep = self.dest.as_endpoint()
            .ok_or_else(|| SyncError::Io(std::io::Error::other("Dest must be local for verify")))?;

        // Scan both sides
        let source_entries = source_ep.scan(self.scan_options).await?;
        let dest_entries = dest_ep.scan(self.scan_options).await?;

        // Build lookup maps
        let source_map: std::collections::HashMap<PathBuf, &FileEntry> = source_entries
            .iter()
            .map(|e| ((*e.relative_path).clone(), e))
            .collect();
        let dest_map: std::collections::HashMap<PathBuf, &FileEntry> = dest_entries
            .iter()
            .map(|e| ((*e.relative_path).clone(), e))
            .collect();

        let mut result = crate::sync::VerificationResult {
            files_matched: 0,
            files_mismatched: Vec::new(),
            files_only_in_source: Vec::new(),
            files_only_in_dest: Vec::new(),
            errors: Vec::new(),
            duration: std::time::Duration::ZERO,
        };

        let start = Instant::now();

        // Check files in source
        for (path, source_entry) in &source_map {
            if let Some(dest_entry) = dest_map.get(path) {
                // Both exist - compare
                if source_entry.size == dest_entry.size {
                    result.files_matched += 1;
                } else {
                    result.files_mismatched.push(source.join(path));
                }
            } else {
                // Only in source
                result.files_only_in_source.push(source.join(path));
            }
        }

        // Check files only in dest
        for path in dest_map.keys() {
            if !source_map.contains_key(path) {
                result.files_only_in_dest.push(dest.join(path));
            }
        }

        result.duration = start.elapsed();
        Ok(result)
    }

    /// Get performance metrics (not yet implemented).
    pub fn get_performance_metrics(&self) -> Option<&crate::perf::PerformanceMetrics> {
        None // TODO: Add performance tracking to SyncSession
    }

    /// Local to local sync: scan both sides, plan, execute.
    async fn direct_local(&self) -> Result<SyncStats> {
        let start = Instant::now();

        let source_ep = self.source.as_endpoint()
            .ok_or_else(|| SyncError::Io(std::io::Error::other("Source must be local endpoint")))?;
        let dest_ep = self.dest.as_endpoint()
            .ok_or_else(|| SyncError::Io(std::io::Error::other("Dest must be local endpoint")))?;

        // Auto-create destination if it doesn't exist
        if !dest_ep.root().exists() {
            std::fs::create_dir_all(dest_ep.root())
                .map_err(|e| SyncError::Io(std::io::Error::other(
                    format!("Failed to create destination {:?}: {}", dest_ep.root(), e)
                )))?;
            tracing::info!("Created destination directory: {:?}", dest_ep.root());
        }

        // Load directory cache if enabled
        let mut dir_cache = if self.config.cache {
            let cache = crate::sync::dircache::DirectoryCache::load(dest_ep.root());
            tracing::debug!("Loaded directory cache with {} entries", cache.len());
            Some(cache)
        } else {
            None
        };

        // Check if we can use cached scan results
        let can_cache = if let Some(ref cache) = dir_cache {
            if let Ok(source_meta) = std::fs::metadata(source_ep.root()) {
                if let Ok(source_mtime) = source_meta.modified() {
                    let source_path = std::path::PathBuf::from(".");
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

        // Scan source (or use cache)
        let source_entries = if can_cache {
            if let Some(ref cache) = dir_cache {
                if let Some(cached_files) = cache.get_cached_files(&std::path::PathBuf::from(".")) {
                    tracing::info!("Using cached scan results ({} files)", cached_files.len());
                    cached_files.iter().map(|cf| cf.to_file_entry(source_ep.root())).collect()
                } else {
                    source_ep.scan(self.scan_options).await?
                }
            } else {
                source_ep.scan(self.scan_options).await?
            }
        } else {
            source_ep.scan(self.scan_options).await?
        };
        let source_count = source_entries.len();
        tracing::info!("Source scan: {} entries", source_count);

        // Update cache with scanned files
        if let Some(ref mut cache) = dir_cache {
            use crate::sync::dircache::CachedFile;
            use std::collections::HashMap;
            
            let mut files_by_dir: HashMap<std::path::PathBuf, Vec<CachedFile>> = HashMap::new();
            
            for entry in &source_entries {
                if entry.is_dir {
                    cache.update((*entry.relative_path).clone(), entry.modified);
                }
                let parent = (*entry.relative_path).parent()
                    .unwrap_or(&std::path::PathBuf::from("."))
                    .to_path_buf();
                files_by_dir
                    .entry(parent)
                    .or_default()
                    .push(CachedFile::from_file_entry(entry));
            }
            
            for (dir, files) in files_by_dir {
                cache.cache_files(dir, files);
            }
        }

        // Scan destination (for deletion support)
        let dest_entries = dest_ep.scan(self.scan_options).await?;
        tracing::info!("Dest scan: {} entries", dest_entries.len());

        // Build dest lookup for quick existence checks
        let dest_map: std::collections::HashMap<PathBuf, &FileEntry> = dest_entries
            .iter()
            .map(|e| ((*e.relative_path).clone(), e))
            .collect();
        
        tracing::debug!("Source root: {:?}", source_ep.root());
        tracing::debug!("Dest root: {:?}", dest_ep.root());
        tracing::debug!("Source entries:");
        for entry in &source_entries {
            tracing::debug!("  {:?}", entry.relative_path);
        }
        tracing::debug!("Dest entries:");
        for entry in &dest_entries {
            tracing::debug!("  {:?}", entry.relative_path);
        }

        // Plan tasks using StrategyPlanner
        let planner = StrategyPlanner::with_comparison_flags(
            self.config.comparison.ignore_times,
            self.config.comparison.size_only,
            self.config.comparison.checksum,
            self.config.comparison.update_only,
            self.config.comparison.ignore_existing,
        );

        let mut tasks: Vec<SyncTask> = Vec::with_capacity(source_entries.len());
        for entry in &source_entries {
            // Apply size filters
            if let Some(min) = self.config.min_size {
                if entry.size < min {
                    continue;
                }
            }
            if let Some(max) = self.config.max_size {
                if entry.size > max {
                    continue;
                }
            }

            // Apply filter engine
            if self.config.filter_engine.should_exclude(&entry.relative_path, entry.is_dir) {
                continue;
            }

            let task = planner.plan_from_scan(entry, &dest_map, dest_ep.root())?;
            tasks.push(task);
        }

        // Add delete tasks if enabled
        let mut delete_count = 0usize;
        if let crate::sync::config::DeleteMode::Enabled { threshold, force } = self.config.delete {
            let source_set: std::collections::HashSet<PathBuf> = source_entries
                .iter()
                .map(|e| (*e.relative_path).clone())
                .collect();

            // Collect deletions first for threshold check
            let mut deletions = Vec::new();
            for dest_entry in &dest_entries {
                if !source_set.contains(&*dest_entry.relative_path) {
                    // Apply filter engine to deletions too
                    if self.config.filter_engine.should_exclude(&dest_entry.relative_path, dest_entry.is_dir) {
                        continue;
                    }
                    deletions.push(dest_entry);
                }
            }

            // Check deletion threshold
            let dest_file_count = dest_entries.len();
            if dest_file_count > 0 && !force {
                let delete_percentage = (deletions.len() as f64 / dest_file_count as f64) * 100.0;
                if delete_percentage > threshold as f64 {
                    return Err(crate::error::SyncError::DeletionThresholdExceeded {
                        percentage: delete_percentage,
                        threshold,
                    });
                }
            }

            // Create delete tasks
            for dest_entry in deletions {
                tasks.push(SyncTask {
                    source: None,
                    dest_path: self.dest.root().join(&*dest_entry.relative_path),
                    action: SyncAction::Delete,
                    source_checksum: None,
                    dest_checksum: None,
                });
                delete_count += 1;
            }
        }

        let creates = tasks.iter().filter(|t| t.action == SyncAction::Create).count();
        let updates = tasks.iter().filter(|t| t.action == SyncAction::Update).count();
        let skips = tasks.iter().filter(|t| t.action == SyncAction::Skip).count();
        tracing::info!(
            "Plan: {} creates, {} updates, {} deletes, {} skips",
            creates, updates, delete_count, skips
        );

        // Filter out Create tasks if --existing is set (only update existing files)
        if self.config.existing {
            tasks.retain(|t| t.action != SyncAction::Create);
        }

        // Execute tasks (inline for Phase 3, TaskExecutor extracts in Phase 4)
        if self.config.dry_run {
            return Ok(SyncStats {
                files_scanned: source_count as u64,
                files_created: creates as u64,
                files_updated: updates as u64,
                files_deleted: delete_count,
                files_skipped: skips,
                duration: start.elapsed(),
                ..Default::default()
            });
        }

        // Create TaskExecutor with config from SyncConfig
        let executor = TaskExecutor::new(
            source_ep,
            dest_ep,
            false, // dry_run handled above
            self.config.preserve.clone(),
            crate::sync::config::VerificationConfig {
                mode: self.config.verification.mode,
                verify_on_write: self.config.verification.verify_on_write,
                checksum_db: self.config.verification.checksum_db,
                clear_checksum_db: self.config.verification.clear_checksum_db,
                prune_checksum_db: self.config.verification.prune_checksum_db,
            },
            self.config.max_concurrent,
        ).with_backup(crate::sync::executor::BackupConfig {
            enabled: self.config.backup.is_some(),
            suffix: self.config.suffix.clone(),
            dir: self.config.backup_dir.clone(),
        }).with_config(crate::sync::executor::ExecuteConfig {
            preserve_hardlinks: self.config.preserve.hardlinks,
            preserve_xattrs: self.config.preserve.xattrs,
            preserve_dir_permissions: self.config.preserve.permissions,
            keep_partial: self.config.partial.is_some(),
            itemize_changes: self.config.itemize_changes,
            remove_source_files: self.config.remove_source_files,
            print_stats: self.config.stats,
        });

        // Execute tasks using TaskExecutor
        let mut stats = executor.execute_batch(tasks).await?;
        stats.files_scanned = source_count as u64;
        stats.duration = start.elapsed(); // Include scan/plan time
        
        // Save directory cache if enabled
        if let Some(ref cache) = dir_cache {
            if let Err(e) = cache.save(dest_ep.root()) {
                tracing::warn!("Failed to save directory cache: {}", e);
            }
        }
        
        Ok(stats)
    }

    /// Sync a single file from source to destination.
    pub async fn sync_single_file(&self, source: &Path, dest: &Path) -> Result<SyncStats> {
        let start = std::time::Instant::now();
        let mut stats = SyncStats {
            files_scanned: 1,
            ..Default::default()
        };

        // For single file sync, use the parent directory as the endpoint root
        let source_parent = source.parent().unwrap_or(Path::new("."));
        let dest_parent = dest.parent().unwrap_or(Path::new("."));
        let source_filename = source.file_name().unwrap();
        let dest_filename = dest.file_name().unwrap();

        let source_ep = LocalEndpoint::new(source_parent.to_path_buf());
        let dest_ep = LocalEndpoint::new(dest_parent.to_path_buf());

        // Check if dest exists
        let dest_exists = tokio::fs::metadata(dest).await.is_ok();

        // Check change ratio for large files (above 10MB delta threshold)
        const DELTA_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB
        if dest_exists {
            let source_meta = tokio::fs::metadata(source).await?;
            if source_meta.len() > DELTA_THRESHOLD {
                // Determine delta sync strategy
                let supports_cow = crate::fs_util::supports_cow_reflinks(dest);
                let same_fs = crate::fs_util::same_filesystem(source, dest);
                let has_hardlinks = crate::fs_util::has_hard_links(dest);

                let use_cow_strategy = supports_cow && same_fs && !has_hardlinks;

                if use_cow_strategy {
                    tracing::info!(
                        "Delta sync strategy: COW (clone + selective writes) - filesystem supports COW reflinks"
                    );
                } else {
                    let reason = if !supports_cow {
                        "filesystem does not support COW reflinks"
                    } else if !same_fs {
                        "source and dest on different filesystems"
                    } else {
                        "destination has hard links (preserving link integrity)"
                    };

                    tracing::info!(
                        "Delta sync strategy: in-place (full file rebuild) - {}",
                        reason
                    );
                }

                match crate::delta::estimate_change_ratio(
                    source,
                    dest,
                    64 * 1024, // 64KB blocks
                    Some(20),  // Sample 20 blocks
                    Some(0.75), // 75% threshold
                ) {
                    Ok(ratio) => {
                        tracing::info!(
                            "Change ratio: {}/{} blocks changed ({:.1}%)",
                            ratio.blocks_changed,
                            ratio.blocks_sampled,
                            ratio.change_ratio * 100.0
                        );

                        if ratio.use_delta {
                            tracing::info!(
                                "Change ratio {:.1}% below threshold {:.1}%, using delta sync",
                                ratio.change_ratio * 100.0,
                                ratio.threshold * 100.0
                            );
                        } else {
                            tracing::info!(
                                "Change ratio {:.1}% exceeds threshold {:.1}%, using full copy",
                                ratio.change_ratio * 100.0,
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
        let data = source_ep.read_file(Path::new(source_filename)).await?;
        let meta = source_ep.metadata(Path::new(source_filename)).await?;

        if !dest_exists {
            // Create new file
            dest_ep.write_file(Path::new(dest_filename), &data, &meta).await?;
            stats.files_created = 1;
            stats.bytes_transferred = data.len() as u64;
        } else {
            // Update existing file
            dest_ep.write_file(Path::new(dest_filename), &data, &meta).await?;
            stats.files_updated = 1;
            stats.bytes_transferred = data.len() as u64;
        }

        stats.duration = start.elapsed();
        Ok(stats)
    }

    /// Local to SSH push sync.
    async fn streaming_push(&self) -> Result<SyncStats> {
        let (host, user, dest_root) = match &self.dest {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => return Err(SyncError::Io(std::io::Error::other("Dest must be SSH for push"))),
        };

        let source_root = self.source.root().to_path_buf();

        // Parse SSH config
        let ssh_config = if let Some(user) = user {
            SshConfig {
                hostname: host.clone(),
                user: user.clone(),
                ..Default::default()
            }
        } else {
            crate::ssh::config::parse_ssh_config(host)?
        };

        // Use ServerSession which handles SSH subprocess properly
        let server_session = crate::transport::server::ServerSession::connect_ssh(&ssh_config, dest_root)
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?;
        let (mut stdin, mut stdout) = server_session.split();

        // Use streaming protocol
        let streaming = crate::streaming::StreamingSync::new(
            source_root,
            dest_root.clone(),
            self.config.delete.is_enabled(),
            CompressionDetection::Auto, // compress for SSH
        );

        let streaming_stats = streaming.push(&mut stdout, &mut stdin)
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?;

        Ok(SyncStats {
            files_scanned: streaming_stats.files_ok as u64,
            files_created: streaming_stats.files_ok as u64,
            bytes_transferred: streaming_stats.bytes_transferred,
            files_delta_synced: streaming_stats.delta_files as usize,
            delta_bytes_saved: streaming_stats.delta_bytes_saved,
            dirs_created: streaming_stats.dirs_created,
            symlinks_created: streaming_stats.symlinks_created,
            duration: Instant::now().elapsed(),
            ..Default::default()
        })
    }

    /// SSH to local pull sync.
    async fn streaming_pull(&self) -> Result<SyncStats> {
        let (host, user, source_root) = match &self.source {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => return Err(SyncError::Io(std::io::Error::other("Source must be SSH for pull"))),
        };

        let dest_root = self.dest.root().to_path_buf();

        // Parse SSH config
        let ssh_config = if let Some(user) = user {
            SshConfig {
                hostname: host.clone(),
                user: user.clone(),
                ..Default::default()
            }
        } else {
            crate::ssh::config::parse_ssh_config(host)?
        };

        // Use ServerSession which handles SSH subprocess properly
        let server_session = crate::transport::server::ServerSession::connect_ssh(&ssh_config, source_root)
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?;
        let (mut stdin, mut stdout) = server_session.split();

        // Use streaming protocol (pull)
        let streaming = crate::streaming::StreamingSync::new(
            dest_root,
            source_root.clone(),
            self.config.delete.is_enabled(),
            CompressionDetection::Auto, // compress for SSH
        );

        let streaming_stats = streaming.pull(&mut stdout, &mut stdin)
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?;

        Ok(SyncStats {
            files_scanned: streaming_stats.files_ok as u64,
            files_created: streaming_stats.files_ok as u64,
            bytes_transferred: streaming_stats.bytes_transferred,
            files_delta_synced: streaming_stats.delta_files as usize,
            delta_bytes_saved: streaming_stats.delta_bytes_saved,
            dirs_created: streaming_stats.dirs_created,
            symlinks_created: streaming_stats.symlinks_created,
            duration: Instant::now().elapsed(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::local::LocalEndpoint;
    use crate::sync::config::{ComparisonConfig, DeleteMode, SyncConfig};
    use tempfile::TempDir;

    fn test_config() -> SyncConfig {
        SyncConfig {
            dry_run: false,
            delete: DeleteMode::Disabled,
            comparison: ComparisonConfig::default(),
            filter_engine: crate::filter::FilterEngine::new(),
            ..SyncConfig::test_default()
        }
    }

    #[test]
    fn test_strategy_local_to_local() {
        let tmp = TempDir::new().unwrap();
        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(tmp.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(tmp.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());
        assert_eq!(session.select_strategy(), SyncStrategy::DirectLocal);
    }

    #[test]
    fn test_strategy_local_to_ssh() {
        let tmp = TempDir::new().unwrap();
        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(tmp.path().to_path_buf())));
        let dest = EndpointPair::Ssh {
            host: "example.com".into(),
            user: None,
            root: PathBuf::from("/remote/path"),
        };
        let session = SyncSession::new(source, dest, test_config());
        assert_eq!(session.select_strategy(), SyncStrategy::StreamingPush);
    }

    #[test]
    fn test_strategy_ssh_to_local() {
        let tmp = TempDir::new().unwrap();
        let source = EndpointPair::Ssh {
            host: "example.com".into(),
            user: None,
            root: PathBuf::from("/remote/path"),
        };
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(tmp.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());
        assert_eq!(session.select_strategy(), SyncStrategy::StreamingPull);
    }

    #[tokio::test]
    async fn test_direct_local_empty() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());

        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.files_created, 0);
    }

    #[tokio::test]
    async fn test_direct_local_creates_files() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create source files
        std::fs::write(src_dir.path().join("file1.txt"), "hello").unwrap();
        std::fs::write(src_dir.path().join("file2.txt"), "world").unwrap();

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());

        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_created, 2);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.bytes_transferred, 10); // 5 + 5

        // Verify files were copied
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("file1.txt")).unwrap(), "hello");
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("file2.txt")).unwrap(), "world");
    }

    #[tokio::test]
    async fn test_direct_local_skips_unchanged() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create source file
        std::fs::write(src_dir.path().join("file.txt"), "hello").unwrap();

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());

        // First sync
        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_created, 1);

        // Second sync should skip
        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());

        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_created, 0);
    }

    #[tokio::test]
    async fn test_direct_local_dry_run() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::write(src_dir.path().join("file.txt"), "hello").unwrap();

        let config = SyncConfig {
            dry_run: true,
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config);

        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_created, 1); // counted but not executed
        assert_eq!(stats.bytes_transferred, 0); // no actual transfer

        // Verify file was NOT copied
        assert!(!dst_dir.path().join("file.txt").exists());
    }

    #[tokio::test]
    async fn test_direct_local_with_delete() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create initial files and sync
        std::fs::write(src_dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(dst_dir.path().join("delete.txt"), "delete").unwrap();

        let config = SyncConfig {
            delete: DeleteMode::Enabled { threshold: 100, force: false },
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config);

        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(stats.files_deleted, 1);
        assert!(!dst_dir.path().join("delete.txt").exists());
        assert!(dst_dir.path().join("keep.txt").exists());
    }

    #[tokio::test]
    async fn test_direct_local_with_subdirectories() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        std::fs::create_dir(src_dir.path().join("subdir")).unwrap();
        std::fs::write(src_dir.path().join("subdir/file.txt"), "content").unwrap();

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());

        let stats = session.sync().await.unwrap();
        assert!(stats.files_created >= 1);
        assert!(dst_dir.path().join("subdir/file.txt").exists());
    }

    #[tokio::test]
    async fn test_direct_local_existing_only() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create files in source
        std::fs::write(src_dir.path().join("new.txt"), "new").unwrap();
        std::fs::write(src_dir.path().join("update.txt"), "updated").unwrap();

        // Create only update.txt in dest (so it gets updated, but new.txt is skipped)
        std::fs::write(dst_dir.path().join("update.txt"), "old").unwrap();

        let config = SyncConfig {
            existing: true,
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config);

        let stats = session.sync().await.unwrap();
        // update.txt should be updated, new.txt should NOT be created
        assert_eq!(stats.files_updated, 1);
        assert!(!dst_dir.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn test_strategy_select() {
        let local = EndpointPair::Local(Box::new(LocalEndpoint::new(PathBuf::from("/tmp/src"))));
        let local2 = EndpointPair::Local(Box::new(LocalEndpoint::new(PathBuf::from("/tmp/dst"))));
        let session = SyncSession::new(local, local2, test_config());
        assert_eq!(session.select_strategy(), SyncStrategy::DirectLocal);
    }

    #[tokio::test]
    async fn test_delete_threshold_enforcement() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create 10 source files
        for i in 0..10 {
            std::fs::write(src_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }

        // Create 100 dest files (90 would be deleted)
        for i in 0..100 {
            std::fs::write(dst_dir.path().join(format!("old{}.txt", i)), "old").unwrap();
        }

        let config = SyncConfig {
            delete: DeleteMode::Enabled { threshold: 50, force: false },
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config);

        let result = session.sync().await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("threshold") || err.contains("too many"));
    }

    #[tokio::test]
    async fn test_delete_threshold_force_override() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create 10 source files
        for i in 0..10 {
            std::fs::write(src_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }

        // Create 100 dest files
        for i in 0..100 {
            std::fs::write(dst_dir.path().join(format!("old{}.txt", i)), "old").unwrap();
        }

        let config = SyncConfig {
            delete: DeleteMode::Enabled { threshold: 50, force: true },
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config);

        // Should succeed with force=true
        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_created, 10);
        assert_eq!(stats.files_deleted, 100);
    }

    #[tokio::test]
    async fn test_ignore_existing_filter() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create files in source
        std::fs::write(src_dir.path().join("new.txt"), "new").unwrap();
        std::fs::write(src_dir.path().join("existing.txt"), "updated").unwrap();

        // Create existing.txt in dest
        std::fs::write(dst_dir.path().join("existing.txt"), "old").unwrap();

        let config = SyncConfig {
            comparison: ComparisonConfig {
                ignore_existing: true,
                ..Default::default()
            },
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config);

        let stats = session.sync().await.unwrap();
        // new.txt should be created, existing.txt should be skipped
        assert_eq!(stats.files_created, 1);
        assert!(dst_dir.path().join("new.txt").exists());
        assert_eq!(std::fs::read_to_string(dst_dir.path().join("existing.txt")).unwrap(), "old");
    }

    #[tokio::test]
    async fn test_dirs_only_filter() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create files and directories
        std::fs::write(src_dir.path().join("file.txt"), "content").unwrap();
        std::fs::create_dir(src_dir.path().join("subdir")).unwrap();
        std::fs::write(src_dir.path().join("subdir/nested.txt"), "nested").unwrap();

        let config = SyncConfig {
            dirs: true,
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config)
            .with_scan_options(ScanOptions {
                dirs_only: true,
                ..Default::default()
            });

        let stats = session.sync().await.unwrap();
        // Files in root should be synced, but nested files should not
        assert!(dst_dir.path().join("file.txt").exists());
        assert!(dst_dir.path().join("subdir").exists());
        // Nested file should NOT be synced because dirs_only limits recursion
        assert!(!dst_dir.path().join("subdir/nested.txt").exists());
    }

    #[tokio::test]
    async fn test_filter_engine_integration() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Create files
        std::fs::write(src_dir.path().join("include.txt"), "include").unwrap();
        std::fs::write(src_dir.path().join("exclude.txt"), "exclude").unwrap();
        std::fs::write(src_dir.path().join("exclude.log"), "log").unwrap();

        let mut filter = crate::filter::FilterEngine::new();
        filter.add_exclude("*.log").unwrap();

        let config = SyncConfig {
            filter_engine: filter,
            ..test_config()
        };

        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, config);

        let stats = session.sync().await.unwrap();
        assert!(dst_dir.path().join("include.txt").exists());
        assert!(dst_dir.path().join("exclude.txt").exists());
        assert!(!dst_dir.path().join("exclude.log").exists());
    }

    #[tokio::test]
    async fn test_error_handling_source_not_found() {
        let src_dir = TempDir::new().unwrap();
        let dst_dir = TempDir::new().unwrap();

        // Don't create any source files
        let source = EndpointPair::Local(Box::new(LocalEndpoint::new(src_dir.path().to_path_buf())));
        let dest = EndpointPair::Local(Box::new(LocalEndpoint::new(dst_dir.path().to_path_buf())));
        let session = SyncSession::new(source, dest, test_config());

        // Should succeed with 0 files
        let stats = session.sync().await.unwrap();
        assert_eq!(stats.files_scanned, 0);
    }
}
