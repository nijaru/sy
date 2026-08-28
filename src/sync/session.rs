//! v0.5 sync orchestration.
//!
//! Local reconciliation is correctness-first and async: comparison policy can
//! hash endpoint streams when requested, deletion safety is computed over the
//! same filtered destination universe that can actually be deleted, and cached
//! scan results are never trusted as a correctness oracle.

use crate::endpoint::io::{hash_file_streaming, VerificationStatus};
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::transfer::{transfer_file, TransferOptions};
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError};
use crate::ssh::config::SshConfig;
use crate::sync::config::{DeleteMode, SyncConfig};
use crate::sync::executor::{BackupConfig, ExecuteConfig, TaskExecutor};
use crate::sync::scanner::{FileEntry, ScanOptions};
use crate::sync::stats::{SyncError as StatsError, SyncStats};
use crate::sync::strategy::{SyncAction, SyncTask};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Endpoint configuration owned by a sync session.
pub enum EndpointPair {
    Local(Box<dyn Endpoint>),
    Ssh {
        host: String,
        user: Option<String>,
        root: PathBuf,
    },
}

impl EndpointPair {
    pub fn from_sync_path(path: &crate::path::SyncPath) -> Result<Self> {
        match path {
            crate::path::SyncPath::Local { path, .. } => {
                Ok(Self::Local(Box::new(LocalEndpoint::new(path.clone()))))
            }
            crate::path::SyncPath::Remote {
                host, user, path, ..
            } => Ok(Self::Ssh {
                host: host.clone(),
                user: user.clone(),
                root: path.clone(),
            }),
            crate::path::SyncPath::S3 { .. } | crate::path::SyncPath::Gcs { .. } => Err(
                SyncError::Config("S3/GCS endpoints not yet supported".to_string()),
            ),
        }
    }

    pub fn root(&self) -> &Path {
        match self {
            Self::Local(endpoint) => endpoint.root(),
            Self::Ssh { root, .. } => root,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    pub fn as_endpoint(&self) -> Option<&dyn Endpoint> {
        match self {
            Self::Local(endpoint) => Some(endpoint.as_ref()),
            Self::Ssh { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStrategy {
    DirectLocal,
    StreamingPush,
    StreamingPull,
    ObjectStore,
}

pub struct SyncSession {
    source: EndpointPair,
    dest: EndpointPair,
    config: SyncConfig,
    scan_options: ScanOptions,
}

impl SyncSession {
    pub fn new(source: EndpointPair, dest: EndpointPair, config: SyncConfig) -> Self {
        Self {
            source,
            dest,
            config,
            scan_options: ScanOptions::default(),
        }
    }

    pub fn with_scan_options(mut self, scan_options: ScanOptions) -> Self {
        self.scan_options = scan_options;
        self
    }

    pub fn select_strategy(&self) -> SyncStrategy {
        match (&self.source, &self.dest) {
            (EndpointPair::Local(_), EndpointPair::Local(_)) => SyncStrategy::DirectLocal,
            (EndpointPair::Local(_), EndpointPair::Ssh { .. }) => SyncStrategy::StreamingPush,
            (EndpointPair::Ssh { .. }, EndpointPair::Local(_)) => SyncStrategy::StreamingPull,
            _ => SyncStrategy::ObjectStore,
        }
    }

    pub async fn sync(&self) -> Result<SyncStats> {
        let strategy = self.select_strategy();
        tracing::info!(?strategy, "selected sync strategy");

        match strategy {
            SyncStrategy::DirectLocal => self.direct_local().await,
            SyncStrategy::StreamingPush => self.streaming_push().await,
            SyncStrategy::StreamingPull => self.streaming_pull().await,
            SyncStrategy::ObjectStore => Err(SyncError::Config(
                "object store sync not yet implemented".to_string(),
            )),
        }
    }

    /// Verify two local endpoint trees by type and content.
    ///
    /// Regular files are BLAKE3-hashed through the endpoint streaming API.
    /// Directories compare by type; symlinks compare link targets.
    pub async fn verify(
        &self,
        source: &Path,
        dest: &Path,
    ) -> Result<crate::sync::VerificationResult> {
        let started = Instant::now();
        let source_ep = self.source.as_endpoint().ok_or_else(|| {
            SyncError::Config("source must be local for tree verification".to_string())
        })?;
        let dest_ep = self.dest.as_endpoint().ok_or_else(|| {
            SyncError::Config("destination must be local for tree verification".to_string())
        })?;

        let source_entries = source_ep.scan(self.scan_options).await?;
        let dest_entries = dest_ep.scan(self.scan_options).await?;
        let source_map = entries_by_path(&source_entries);
        let dest_map = entries_by_path(&dest_entries);

        let mut result = crate::sync::VerificationResult {
            files_matched: 0,
            files_mismatched: Vec::new(),
            files_only_in_source: Vec::new(),
            files_only_in_dest: Vec::new(),
            errors: Vec::new(),
            duration: Duration::ZERO,
        };

        for (path, source_entry) in &source_map {
            let Some(dest_entry) = dest_map.get(path) else {
                result.files_only_in_source.push(source.join(path));
                continue;
            };

            let matched = if entry_kind(source_entry) != entry_kind(dest_entry) {
                false
            } else if source_entry.is_dir {
                true
            } else if source_entry.is_symlink {
                source_entry.symlink_target == dest_entry.symlink_target
            } else if source_entry.size != dest_entry.size {
                false
            } else {
                match (
                    hash_file_streaming(source_ep, path).await,
                    hash_file_streaming(dest_ep, path).await,
                ) {
                    (Ok(source_hash), Ok(dest_hash)) => source_hash == dest_hash,
                    (source_result, dest_result) => {
                        if let Err(error) = source_result {
                            result.errors.push(StatsError {
                                path: source.join(path),
                                error: error.to_string(),
                                action: "verify-source".to_string(),
                            });
                        }
                        if let Err(error) = dest_result {
                            result.errors.push(StatsError {
                                path: dest.join(path),
                                error: error.to_string(),
                                action: "verify-destination".to_string(),
                            });
                        }
                        false
                    }
                }
            };

            if matched {
                result.files_matched += 1;
            } else {
                result.files_mismatched.push(source.join(path));
            }
        }

        for path in dest_map.keys() {
            if !source_map.contains_key(path) {
                result.files_only_in_dest.push(dest.join(path));
            }
        }

        result.duration = started.elapsed();
        Ok(result)
    }

    pub fn get_performance_metrics(&self) -> Option<&crate::perf::PerformanceMetrics> {
        None
    }

    async fn direct_local(&self) -> Result<SyncStats> {
        let started = Instant::now();
        let source = self
            .source
            .as_endpoint()
            .ok_or_else(|| SyncError::Config("source must be local for direct sync".to_string()))?;
        let dest = self.dest.as_endpoint().ok_or_else(|| {
            SyncError::Config("destination must be local for direct sync".to_string())
        })?;

        if !dest.root().exists() && !self.config.dry_run {
            std::fs::create_dir_all(dest.root())?;
        }

        // v0.4 keyed source-cache reuse off the root directory mtime, which does
        // not change for ordinary file-content modifications. Until the cache has
        // a content-safe invalidation model, it is write-only diagnostic state and
        // never substitutes for a source scan.
        if self.config.cache {
            tracing::debug!(
                "directory cache reuse disabled in v0.5 until invalidation is content-safe"
            );
        }
        if self.config.clear_cache && !self.config.dry_run {
            let _ = crate::sync::dircache::DirectoryCache::delete(dest.root());
        }

        let source_entries = source.scan(self.scan_options).await?;
        let dest_entries = dest.scan(self.scan_options).await?;
        let source_count = source_entries.len();
        tracing::info!(
            source_entries = source_count,
            dest_entries = dest_entries.len(),
            "completed local scan"
        );

        let dest_map = entries_by_path(&dest_entries);
        let mut tasks = Vec::with_capacity(source_entries.len());

        for source_entry in &source_entries {
            if !self.source_entry_selected(source_entry) {
                continue;
            }

            let dest_entry = dest_map.get(source_entry.relative_path.as_path()).copied();
            let task = self
                .plan_local_entry(source, dest, source_entry, dest_entry)
                .await?;
            tasks.push(task);
        }

        let delete_count = self
            .append_delete_tasks(&source_entries, &dest_entries, &mut tasks)
            .await?;

        if self.config.existing {
            tasks.retain(|task| task.action != SyncAction::Create);
        }

        let creates = tasks
            .iter()
            .filter(|task| task.action == SyncAction::Create)
            .count();
        let updates = tasks
            .iter()
            .filter(|task| task.action == SyncAction::Update)
            .count();
        let skips = tasks
            .iter()
            .filter(|task| task.action == SyncAction::Skip)
            .count();

        tracing::info!(
            creates,
            updates,
            deletes = delete_count,
            skips,
            "local plan"
        );

        if self.config.dry_run {
            return Ok(SyncStats {
                files_scanned: source_count as u64,
                files_created: creates as u64,
                files_updated: updates as u64,
                files_deleted: delete_count,
                files_skipped: skips,
                duration: started.elapsed(),
                ..Default::default()
            });
        }

        let executor = TaskExecutor::new(
            source,
            dest,
            false,
            self.config.preserve.clone(),
            self.config.verification.clone(),
            self.config.max_concurrent,
        )
        .with_backup(BackupConfig {
            enabled: self.config.backup.is_some(),
            suffix: self.config.suffix.clone(),
            dir: self.config.backup_dir.clone(),
        })
        .with_config(ExecuteConfig {
            preserve_hardlinks: self.config.preserve.hardlinks,
            preserve_xattrs: self.config.preserve.xattrs,
            preserve_dir_permissions: self.config.preserve.permissions,
            keep_partial: self.config.partial.is_some(),
            itemize_changes: self.config.itemize_changes,
            remove_source_files: self.config.remove_source_files,
            print_stats: self.config.stats,
        });

        let mut stats = executor.execute_batch(tasks).await?;
        stats.files_scanned = source_count as u64;
        stats.duration = started.elapsed();
        Ok(stats)
    }

    fn source_entry_selected(&self, entry: &FileEntry) -> bool {
        if !entry.is_dir {
            if let Some(min) = self.config.min_size {
                if entry.size < min {
                    return false;
                }
            }
            if let Some(max) = self.config.max_size {
                if entry.size > max {
                    return false;
                }
            }
        }

        !self
            .config
            .filter_engine
            .should_exclude(&entry.relative_path, entry.is_dir)
    }

    async fn plan_local_entry(
        &self,
        source_endpoint: &dyn Endpoint,
        dest_endpoint: &dyn Endpoint,
        source: &FileEntry,
        dest: Option<&FileEntry>,
    ) -> Result<SyncTask> {
        let dest_path = (*source.relative_path).clone();
        let Some(dest) = dest else {
            return Ok(task_for(source, dest_path, SyncAction::Create));
        };

        if self.config.comparison.ignore_existing {
            return Ok(task_for(source, dest_path, SyncAction::Skip));
        }
        if self.config.comparison.update_only && dest.modified > source.modified {
            return Ok(task_for(source, dest_path, SyncAction::Skip));
        }

        // Refuse destructive type transitions until path replacement is a
        // first-class transaction. This is safer than deleting a destination
        // before the replacement is known to be valid.
        if entry_kind(source) != entry_kind(dest) {
            return Err(SyncError::Config(format!(
                "refusing non-transactional type replacement at {}",
                source.relative_path.display()
            )));
        }

        let action = if source.is_dir {
            SyncAction::Skip
        } else if source.is_symlink {
            if source.symlink_target == dest.symlink_target {
                SyncAction::Skip
            } else {
                SyncAction::Update
            }
        } else if self.config.comparison.checksum {
            if source.size != dest.size {
                SyncAction::Update
            } else {
                let source_hash =
                    hash_file_streaming(source_endpoint, &source.relative_path).await?;
                let dest_hash = hash_file_streaming(dest_endpoint, &dest.relative_path).await?;
                if source_hash == dest_hash {
                    SyncAction::Skip
                } else {
                    SyncAction::Update
                }
            }
        } else if self.config.comparison.size_only {
            if source.size == dest.size {
                SyncAction::Skip
            } else {
                SyncAction::Update
            }
        } else if self.config.comparison.ignore_times {
            SyncAction::Update
        } else if source.size == dest.size && source.modified == dest.modified {
            SyncAction::Skip
        } else {
            SyncAction::Update
        };

        Ok(task_for(source, dest_path, action))
    }

    async fn append_delete_tasks(
        &self,
        source_entries: &[FileEntry],
        dest_entries: &[FileEntry],
        tasks: &mut Vec<SyncTask>,
    ) -> Result<usize> {
        let DeleteMode::Enabled { threshold, force } = self.config.delete else {
            return Ok(0);
        };

        // All scanned source paths protect their destination counterpart from
        // deletion, including paths skipped by size filters. Filtered destination
        // paths are separately protected below.
        let source_paths: HashSet<&Path> = source_entries
            .iter()
            .map(|entry| entry.relative_path.as_path())
            .collect();

        let eligible_dest: Vec<&FileEntry> = dest_entries
            .iter()
            .filter(|entry| {
                !self
                    .config
                    .filter_engine
                    .should_exclude(&entry.relative_path, entry.is_dir)
            })
            .collect();

        let deletions: Vec<&FileEntry> = eligible_dest
            .iter()
            .copied()
            .filter(|entry| !source_paths.contains(entry.relative_path.as_path()))
            .collect();

        if !force && !eligible_dest.is_empty() {
            let percentage = deletions.len() as f64 / eligible_dest.len() as f64 * 100.0;
            if percentage > threshold as f64 {
                return Err(SyncError::DeletionThresholdExceeded {
                    percentage,
                    threshold,
                });
            }
        }

        for entry in &deletions {
            tasks.push(SyncTask {
                source: None,
                // Keep endpoint operations relative; absolute paths make endpoint
                // containment and future remote backends harder to reason about.
                dest_path: (*entry.relative_path).clone(),
                action: SyncAction::Delete,
                source_checksum: None,
                dest_checksum: None,
            });
        }

        Ok(deletions.len())
    }

    pub async fn sync_single_file(&self, source: &Path, dest: &Path) -> Result<SyncStats> {
        let started = Instant::now();
        let source_parent = source.parent().unwrap_or(Path::new("."));
        let dest_parent = dest.parent().unwrap_or(Path::new("."));
        let source_name = source
            .file_name()
            .ok_or_else(|| SyncError::Config("source has no filename".to_string()))?;
        let dest_name = dest
            .file_name()
            .ok_or_else(|| SyncError::Config("destination has no filename".to_string()))?;

        let source_endpoint = LocalEndpoint::new(source_parent.to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest_parent.to_path_buf());
        let update = dest_endpoint.exists(Path::new(dest_name)).await?;
        let transfer = transfer_file(
            &source_endpoint,
            Path::new(source_name),
            &dest_endpoint,
            Path::new(dest_name),
            TransferOptions {
                update,
                verify: self.config.verification.verify_on_write,
            },
        )
        .await?;

        let mut stats = SyncStats {
            files_scanned: 1,
            bytes_transferred: transfer.bytes_written,
            duration: started.elapsed(),
            ..Default::default()
        };

        if matches!(transfer.verification, VerificationStatus::Failed { .. }) {
            stats.verification_failures = 1;
        } else if update {
            stats.files_updated = 1;
        } else {
            stats.files_created = 1;
        }
        Ok(stats)
    }

    async fn streaming_push(&self) -> Result<SyncStats> {
        let started = Instant::now();
        let (host, user, dest_root) = match &self.dest {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => {
                return Err(SyncError::Config(
                    "destination must be SSH for push".to_string(),
                ))
            }
        };
        let source_root = self.source.root().to_path_buf();
        let ssh_config = resolve_ssh_config(host, user)?;
        let server_session =
            crate::transport::server::ServerSession::connect_ssh(&ssh_config, dest_root)
                .await
                .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        let (mut stdin, mut stdout) = server_session.split();

        let streaming = self.configure_streaming(crate::streaming::StreamingSync::new(
            source_root,
            dest_root.clone(),
            self.config.delete.is_enabled(),
            self.config.compression_detection,
        ));

        let stats = streaming
            .push(&mut stdout, &mut stdin)
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        Ok(streaming_stats(stats, started.elapsed()))
    }

    async fn streaming_pull(&self) -> Result<SyncStats> {
        let started = Instant::now();
        let (host, user, source_root) = match &self.source {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => return Err(SyncError::Config("source must be SSH for pull".to_string())),
        };
        let dest_root = self.dest.root().to_path_buf();
        let ssh_config = resolve_ssh_config(host, user)?;
        let server_session =
            crate::transport::server::ServerSession::connect_ssh(&ssh_config, source_root)
                .await
                .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        let (mut stdin, mut stdout) = server_session.split();

        let streaming = self.configure_streaming(crate::streaming::StreamingSync::new(
            dest_root,
            source_root.clone(),
            self.config.delete.is_enabled(),
            self.config.compression_detection,
        ));

        let stats = streaming
            .pull(&mut stdout, &mut stdin)
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        Ok(streaming_stats(stats, started.elapsed()))
    }

    fn configure_streaming(
        &self,
        mut streaming: crate::streaming::StreamingSync,
    ) -> crate::streaming::StreamingSync {
        streaming = streaming
            .with_filter(self.config.filter_engine.clone())
            .with_scan_options(self.scan_options);

        let comparison = &self.config.comparison;
        let mut flags = 0_u8;
        if comparison.checksum {
            flags |= 0x01;
        }
        if comparison.update_only {
            flags |= 0x02;
        }
        if comparison.ignore_existing {
            flags |= 0x04;
        }
        if comparison.ignore_times {
            flags |= 0x08;
        }
        if comparison.size_only {
            flags |= 0x10;
        }
        if flags != 0 {
            streaming = streaming.with_comparison_flags(flags);
        }
        if self.config.verification.verify_on_write {
            streaming = streaming.with_verify(true);
        }
        if let Some(limit) = self.config.bwlimit {
            streaming = streaming.with_bwlimit(limit);
        }
        if let Some(ref max_delete) = self.config.max_delete {
            streaming = streaming.with_max_delete(max_delete.clone());
        }
        if self.config.delete.is_forced() {
            streaming = streaming.with_force_delete(true);
        }
        if self.config.dry_run {
            streaming = streaming.with_dry_run(true);
        }
        streaming
    }
}

fn entries_by_path(entries: &[FileEntry]) -> HashMap<PathBuf, &FileEntry> {
    entries
        .iter()
        .map(|entry| ((*entry.relative_path).clone(), entry))
        .collect()
}

fn entry_kind(entry: &FileEntry) -> u8 {
    if entry.is_symlink {
        2
    } else if entry.is_dir {
        1
    } else {
        0
    }
}

fn task_for(source: &FileEntry, dest_path: PathBuf, action: SyncAction) -> SyncTask {
    SyncTask {
        source: Some(Arc::new(source.clone())),
        dest_path,
        action,
        source_checksum: None,
        dest_checksum: None,
    }
}

fn resolve_ssh_config(host: &str, user: &Option<String>) -> Result<SshConfig> {
    if let Some(user) = user {
        Ok(SshConfig {
            hostname: host.to_string(),
            user: user.clone(),
            ..Default::default()
        })
    } else {
        crate::ssh::config::parse_ssh_config(host)
    }
}

fn streaming_stats(stats: crate::streaming::SyncStats, duration: Duration) -> SyncStats {
    SyncStats {
        files_scanned: stats.files_scanned as u64,
        files_created: stats.files_ok as u64,
        bytes_transferred: stats.bytes_transferred,
        files_delta_synced: stats.delta_files as usize,
        delta_bytes_saved: stats.delta_bytes_saved,
        dirs_created: stats.dirs_created,
        symlinks_created: stats.symlinks_created,
        duration,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::config::{ComparisonConfig, DeleteMode};
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

    fn local_pair(source: &TempDir, dest: &TempDir, config: SyncConfig) -> SyncSession {
        SyncSession::new(
            EndpointPair::Local(Box::new(LocalEndpoint::new(source.path().to_path_buf()))),
            EndpointPair::Local(Box::new(LocalEndpoint::new(dest.path().to_path_buf()))),
            config,
        )
    }

    #[test]
    fn selects_local_strategy() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        assert_eq!(
            local_pair(&source, &dest, test_config()).select_strategy(),
            SyncStrategy::DirectLocal
        );
    }

    #[tokio::test]
    async fn direct_local_creates_and_skips() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"content").unwrap();

        let stats = local_pair(&source, &dest, test_config())
            .sync()
            .await
            .unwrap();
        assert_eq!(stats.files_created, 1);
        assert_eq!(std::fs::read(dest.path().join("file")).unwrap(), b"content");

        let stats = local_pair(&source, &dest, test_config())
            .sync()
            .await
            .unwrap();
        assert_eq!(stats.files_skipped, 1);
    }

    #[tokio::test]
    async fn checksum_detects_same_size_content_change() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"aaaa").unwrap();
        std::fs::write(dest.path().join("file"), b"bbbb").unwrap();

        let config = SyncConfig {
            comparison: ComparisonConfig {
                checksum: true,
                ..Default::default()
            },
            ..test_config()
        };
        let stats = local_pair(&source, &dest, config).sync().await.unwrap();
        assert_eq!(stats.files_updated, 1);
        assert_eq!(std::fs::read(dest.path().join("file")).unwrap(), b"aaaa");
    }

    #[tokio::test]
    async fn verify_detects_same_size_content_change() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"aaaa").unwrap();
        std::fs::write(dest.path().join("file"), b"bbbb").unwrap();

        let session = local_pair(&source, &dest, test_config());
        let verification = session.verify(source.path(), dest.path()).await.unwrap();
        assert_eq!(verification.files_matched, 0);
        assert_eq!(verification.files_mismatched.len(), 1);
    }

    #[tokio::test]
    async fn delete_threshold_ignores_filtered_destination_entries() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(dest.path().join("delete.txt"), b"x").unwrap();
        std::fs::write(dest.path().join("protected.log"), b"x").unwrap();

        let mut filter = crate::filter::FilterEngine::new();
        filter.add_exclude("*.log").unwrap();
        let config = SyncConfig {
            delete: DeleteMode::Enabled {
                threshold: 50,
                force: false,
            },
            filter_engine: filter,
            ..test_config()
        };

        let error = local_pair(&source, &dest, config).sync().await.unwrap_err();
        assert!(matches!(error, SyncError::DeletionThresholdExceeded { .. }));
    }

    #[tokio::test]
    async fn cache_never_hides_content_change() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"one").unwrap();

        let config = SyncConfig {
            cache: true,
            ..test_config()
        };
        local_pair(&source, &dest, config.clone())
            .sync()
            .await
            .unwrap();

        std::fs::write(source.path().join("file"), b"two").unwrap();
        let stats = local_pair(&source, &dest, config).sync().await.unwrap();
        assert_eq!(stats.files_updated, 1);
        assert_eq!(std::fs::read(dest.path().join("file")).unwrap(), b"two");
    }
}
