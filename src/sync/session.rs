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
use crate::error::{Result, SyncError};
use crate::ssh::config::SshConfig;
use crate::sync::config::SyncConfig;
use crate::sync::scanner::FileEntry;
use crate::sync::stats::SyncStats;
use crate::sync::strategy::{StrategyPlanner, SyncAction, SyncTask};
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

        // Load directory cache if enabled
        let mut dir_cache = if self.config.use_cache {
            let cache = crate::sync::dircache::DirectoryCache::load(dest_ep.root());
            tracing::debug!("Loaded directory cache with {} entries", cache.len());
            Some(cache)
        } else {
            None
        };

        // Check if we can use cached scan results
        let can_use_cache = if let Some(ref cache) = dir_cache {
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
        let source_entries = if can_use_cache {
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
        if let crate::sync::config::DeleteMode::Enabled { .. } = self.config.delete {
            let source_set: std::collections::HashSet<PathBuf> = source_entries
                .iter()
                .map(|e| (*e.relative_path).clone())
                .collect();

            for dest_entry in &dest_entries {
                if !source_set.contains(&*dest_entry.relative_path) {
                    // Apply filter engine to deletions too
                    if self.config.filter_engine.should_exclude(&dest_entry.relative_path, dest_entry.is_dir) {
                        continue;
                    }

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
        }

        let creates = tasks.iter().filter(|t| t.action == SyncAction::Create).count();
        let updates = tasks.iter().filter(|t| t.action == SyncAction::Update).count();
        let skips = tasks.iter().filter(|t| t.action == SyncAction::Skip).count();
        tracing::info!(
            "Plan: {} creates, {} updates, {} deletes, {} skips",
            creates, updates, delete_count, skips
        );

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

        let mut stats = SyncStats {
            files_scanned: source_count as u64,
            ..Default::default()
        };

        // Track inodes for hard link preservation
        let mut hardlink_map: std::collections::HashMap<u64, PathBuf> = std::collections::HashMap::new();

        for task in &tasks {
            match task.action {
                SyncAction::Skip => {
                    stats.files_skipped += 1;
                }
                SyncAction::Create | SyncAction::Update => {
                    let source_entry = task.source.as_ref()
                        .ok_or_else(|| SyncError::Io(std::io::Error::other("Missing source for create/update")))?;
                    let source_path = source_ep.root().join(&*source_entry.relative_path);

                    if source_entry.is_dir {
                        dest_ep.create_dir_all(&task.dest_path).await?;
                        
                        // Preserve directory permissions if enabled
                        #[cfg(unix)]
                        if self.config.preserve.permissions {
                            use std::os::unix::fs::PermissionsExt;
                            if let Ok(meta) = std::fs::metadata(&source_path) {
                                let mode = meta.permissions().mode();
                                let abs_dest = if task.dest_path.is_absolute() {
                                    task.dest_path.clone()
                                } else {
                                    dest_ep.root().join(&*task.dest_path)
                                };
                                let _ = std::fs::set_permissions(&abs_dest, std::fs::Permissions::from_mode(mode));
                            }
                        }
                        
                        stats.dirs_created += 1;
                    } else if source_entry.is_symlink {
                        // Read symlink target
                        #[cfg(unix)]
                        {
                            // Resolve absolute path for read_link
                            let abs_source = if source_path.is_absolute() {
                                source_path.clone()
                            } else {
                                source_ep.root().join(&source_path)
                            };
                            let target = std::fs::read_link(&abs_source)?;
                            // Remove existing file/symlink before creating
                            if task.action == SyncAction::Update {
                                dest_ep.remove(&task.dest_path, false).await?;
                            }
                            dest_ep.create_symlink(&target, &task.dest_path).await?;
                            if task.action == SyncAction::Create {
                                stats.symlinks_created += 1;
                            } else {
                                stats.files_updated += 1;
                            }
                        }
                    } else {
                        // Check if this is a hard link that's already been copied
                        let is_hardlink = self.config.preserve.hardlinks
                            && source_entry.nlink > 1;
                        
                        if is_hardlink {
                            if let Some(inode) = source_entry.inode {
                                if let Some(first_path) = hardlink_map.get(&inode) {
                                    // Remove existing file before creating hard link
                                    if task.action == SyncAction::Update {
                                        dest_ep.remove(&task.dest_path, false).await?;
                                    }
                                    // Create hard link to first copy
                                    dest_ep.create_hardlink(first_path, &task.dest_path).await?;
                                    if task.action == SyncAction::Create {
                                        stats.files_created += 1;
                                    } else {
                                        stats.files_updated += 1;
                                    }
                                    continue;
                                }
                                // First copy of this inode - copy normally and record
                                let data = source_ep.read_file(&source_entry.relative_path).await?;
                                let meta = source_ep.metadata(&source_entry.relative_path).await?;
                                dest_ep.write_file(&task.dest_path, &data, &meta).await?;
                                stats.bytes_transferred += data.len() as u64;
                                hardlink_map.insert(inode, task.dest_path.clone());
                            } else {
                                // No inode info - copy normally
                                let data = source_ep.read_file(&source_entry.relative_path).await?;
                                let meta = source_ep.metadata(&source_entry.relative_path).await?;
                                dest_ep.write_file(&task.dest_path, &data, &meta).await?;
                                stats.bytes_transferred += data.len() as u64;
                            }
                        } else {
                            // Regular file copy
                            let data = source_ep.read_file(&source_entry.relative_path).await?;
                            let meta = source_ep.metadata(&source_entry.relative_path).await?;
                            dest_ep.write_file(&task.dest_path, &data, &meta).await?;
                            stats.bytes_transferred += data.len() as u64;
                        }

                        // Copy xattrs if enabled
                        if self.config.preserve.xattrs {
                            #[cfg(unix)]
                            {
                                let abs_source = if source_path.is_absolute() {
                                    source_path.clone()
                                } else {
                                    source_ep.root().join(&source_path)
                                };
                                if let Ok(xattrs) = xattr::list(&abs_source) {
                                    for attr in xattrs {
                                        if let Ok(Some(val)) = xattr::get(&abs_source, &attr) {
                                            let abs_dest = dest_ep.root().join(&*task.dest_path);
                                            let _ = xattr::set(&abs_dest, &attr, &val);
                                        }
                                    }
                                }
                            }
                        }

                        if task.action == SyncAction::Create {
                            stats.files_created += 1;
                        } else {
                            stats.files_updated += 1;
                        }
                    }
                }
                SyncAction::Delete => {
                    dest_ep.remove(&task.dest_path, true).await?;
                    stats.files_deleted += 1;
                }
            }
        }

        stats.duration = start.elapsed();
        
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
        let mut stats = SyncStats::default();
        stats.files_scanned = 1;

        let (source_ep, dest_ep) = self.endpoints()?;

        // Check if dest exists
        let dest_exists = tokio::fs::metadata(dest).await.is_ok();

        // Read source file
        let source_path = crate::path::SyncPath::Local {
            path: source.to_path_buf(),
            has_trailing_slash: false,
        };
        let data = source_ep.read_file(&source_path.relative_path()).await?;
        let meta = source_ep.metadata(&source_path.relative_path()).await?;

        if !dest_exists {
            // Create new file
            let dest_path = crate::path::SyncPath::Local {
                path: dest.to_path_buf(),
                has_trailing_slash: false,
            };
            dest_ep.write_file(&dest_path.relative_path(), &data, &meta).await?;
            stats.files_created = 1;
            stats.bytes_transferred = data.len() as u64;
        } else {
            // Update existing file
            let dest_path = crate::path::SyncPath::Local {
                path: dest.to_path_buf(),
                has_trailing_slash: false,
            };
            dest_ep.write_file(&dest_path.relative_path(), &data, &meta).await?;
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
            true, // compress for SSH
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
            true, // compress for SSH
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
}
