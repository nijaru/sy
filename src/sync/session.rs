//! v0.5 sync session orchestration.
//!
//! Local synchronization delegates to the ordered, bounded reconciler. Remote
//! synchronization keeps the existing SSH streaming protocol until its v0.5
//! protocol migration is ready.

#[path = "reconcile.rs"]
mod reconcile;

use crate::endpoint::io::hash_file_streaming;
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError};
use crate::sync::config::SyncConfig;
use crate::sync::scanner::ScanOptions;
use crate::sync::stats::{SyncError as StatError, SyncStats, VerificationResult};
use std::path::{Path, PathBuf};
use std::time::Instant;
use sy::engine::domain::{Entry, EntryKind};
use sy::engine::reconcile::{EngineError, OrderedReconciler, ReconcileItem};
use sy::engine::scan::{EntryMetadataRequest, ScanRequest};

/// Endpoint description used for top-level strategy dispatch.
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
                "object-store sync is not implemented".to_string(),
            )),
        }
    }

    async fn direct_local(&self) -> Result<SyncStats> {
        let source = self
            .source
            .as_endpoint()
            .ok_or_else(|| SyncError::Config("source must be local for direct sync".to_string()))?;
        let dest = self.dest.as_endpoint().ok_or_else(|| {
            SyncError::Config("destination must be local for direct sync".to_string())
        })?;
        reconcile::run_local_sync(source, dest, &self.config, self.scan_options).await
    }

    /// Verify local source and destination trees through the same strict,
    /// bounded ordered merge used by synchronization. Regular files are
    /// compared with streaming BLAKE3 only after cheap kind/size checks.
    pub async fn verify(&self, source: &Path, dest: &Path) -> Result<VerificationResult> {
        let source_endpoint = self.source.as_endpoint().ok_or_else(|| {
            SyncError::Config("source must be local for verification".to_string())
        })?;
        let dest_endpoint = self.dest.as_endpoint().ok_or_else(|| {
            SyncError::Config("destination must be local for verification".to_string())
        })?;

        let started = Instant::now();
        let mut result = VerificationResult {
            files_matched: 0,
            files_mismatched: Vec::new(),
            files_only_in_source: Vec::new(),
            files_only_in_dest: Vec::new(),
            errors: Vec::new(),
            duration: std::time::Duration::ZERO,
        };
        let request = verification_scan_request(self.scan_options);
        let source_stream = crate::endpoint::local_entry_scan::local_entry_stream(
            source_endpoint.root().to_path_buf(),
            request,
        );
        let dest_stream = crate::endpoint::local_entry_scan::local_entry_stream(
            dest_endpoint.root().to_path_buf(),
            request,
        );
        let mut reconciler = OrderedReconciler::new(source_stream, dest_stream);

        while let Some(item) = reconciler.next().await.map_err(map_engine_error)? {
            match item {
                ReconcileItem::SourceOnly(entry) => {
                    result
                        .files_only_in_source
                        .push(source.join(entry.path.as_path()));
                }
                ReconcileItem::DestinationOnly(entry) => {
                    result
                        .files_only_in_dest
                        .push(dest.join(entry.path.as_path()));
                }
                ReconcileItem::Matched {
                    source: source_entry,
                    destination: dest_entry,
                } => {
                    let relative = source_entry.path.as_path();
                    match entries_match(source_endpoint, dest_endpoint, &source_entry, &dest_entry)
                        .await
                    {
                        Ok(true) => result.files_matched += 1,
                        Ok(false) => result.files_mismatched.push(source.join(relative)),
                        Err(error) => result.errors.push(StatError {
                            path: source.join(relative),
                            error: error.to_string(),
                            action: "verify".to_string(),
                        }),
                    }
                }
            }
        }

        result.duration = started.elapsed();
        Ok(result)
    }

    pub fn get_performance_metrics(&self) -> Option<&crate::perf::PerformanceMetrics> {
        None
    }

    /// Sync one local regular file through the same capability-driven transfer
    /// layer as tree sync.
    pub async fn sync_single_file(&self, source: &Path, dest: &Path) -> Result<SyncStats> {
        let started = Instant::now();
        let source_parent = source.parent().unwrap_or(Path::new("."));
        let dest_parent = dest.parent().unwrap_or(Path::new("."));
        let source_name = source.file_name().ok_or_else(|| SyncError::InvalidPath {
            path: source.to_path_buf(),
        })?;
        let dest_name = dest.file_name().ok_or_else(|| SyncError::InvalidPath {
            path: dest.to_path_buf(),
        })?;
        let source_endpoint = LocalEndpoint::new(source_parent.to_path_buf());
        let dest_endpoint = LocalEndpoint::new(dest_parent.to_path_buf());
        let existed = dest_endpoint.exists(Path::new(dest_name)).await?;

        if self.config.dry_run {
            return Ok(SyncStats {
                files_scanned: 1,
                files_created: u64::from(!existed),
                files_updated: u64::from(existed),
                duration: started.elapsed(),
                ..Default::default()
            });
        }

        let transfer = crate::endpoint::transfer::transfer_file(
            &source_endpoint,
            Path::new(source_name),
            &dest_endpoint,
            Path::new(dest_name),
            crate::endpoint::transfer::TransferOptions {
                update: existed,
                verify: self.config.verification.verify_on_write,
            },
        )
        .await?;

        Ok(SyncStats {
            files_scanned: 1,
            files_created: u64::from(!existed),
            files_updated: u64::from(existed),
            bytes_transferred: transfer.bytes_written,
            duration: started.elapsed(),
            ..Default::default()
        })
    }

    async fn streaming_push(&self) -> Result<SyncStats> {
        if let Some(reason) = super::v3_push::legacy_fallback_reason(&self.config) {
            tracing::debug!(reason, "using legacy remote push compatibility path");
            return self.streaming_push_legacy().await;
        }

        let (host, user, dest_root) = match &self.dest {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => {
                return Err(SyncError::Config(
                    "destination must be SSH for push".to_string(),
                ))
            }
        };
        super::v3_push::run(
            self.source.root(),
            dest_root,
            host,
            user,
            &self.config,
            self.scan_options,
        )
        .await
    }

    async fn streaming_push_legacy(&self) -> Result<SyncStats> {
        let (host, user, dest_root) = match &self.dest {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => {
                return Err(SyncError::Config(
                    "destination must be SSH for push".to_string(),
                ))
            }
        };
        let started = Instant::now();
        let ssh_config = resolve_ssh_config(host, user)?;
        let server_session =
            crate::transport::server::ServerSession::connect_ssh(&ssh_config, dest_root)
                .await
                .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        let (mut stdin, mut stdout) = server_session.split();
        let mut streaming = crate::streaming::StreamingSync::new(
            self.source.root().to_path_buf(),
            dest_root.clone(),
            self.config.delete.is_enabled(),
            self.config.compression_detection,
        )
        .with_filter(self.config.filter_engine.clone())
        .with_dry_run(self.config.dry_run)
        .with_scan_options(self.scan_options);
        streaming = configure_streaming(streaming, &self.config);
        let stats = streaming
            .push(&mut stdout, &mut stdin)
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        Ok(streaming_stats(stats, started.elapsed()))
    }

    async fn streaming_pull(&self) -> Result<SyncStats> {
        let (host, user, source_root) = match &self.source {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => return Err(SyncError::Config("source must be SSH for pull".to_string())),
        };
        let started = Instant::now();
        let ssh_config = resolve_ssh_config(host, user)?;
        let server_session =
            crate::transport::server::ServerSession::connect_ssh(&ssh_config, source_root)
                .await
                .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        let (mut stdin, mut stdout) = server_session.split();
        let mut streaming = crate::streaming::StreamingSync::new(
            self.dest.root().to_path_buf(),
            source_root.clone(),
            self.config.delete.is_enabled(),
            self.config.compression_detection,
        )
        .with_filter(self.config.filter_engine.clone())
        .with_dry_run(self.config.dry_run)
        .with_scan_options(self.scan_options);
        streaming = configure_streaming(streaming, &self.config);
        let stats = streaming
            .pull(&mut stdout, &mut stdin)
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?;
        Ok(streaming_stats(stats, started.elapsed()))
    }
}

async fn entries_match(
    source_endpoint: &dyn Endpoint,
    dest_endpoint: &dyn Endpoint,
    source: &Entry,
    dest: &Entry,
) -> Result<bool> {
    if source.kind != dest.kind {
        return Ok(false);
    }
    if source.kind == EntryKind::Directory {
        return Ok(true);
    }
    if source.kind == EntryKind::Symlink {
        return Ok(source.symlink_target == dest.symlink_target);
    }
    if source.size != dest.size {
        return Ok(false);
    }
    let source_hash = hash_file_streaming(source_endpoint, source.path.as_path()).await?;
    let dest_hash = hash_file_streaming(dest_endpoint, dest.path.as_path()).await?;
    Ok(source_hash == dest_hash)
}

fn verification_scan_request(options: ScanOptions) -> ScanRequest {
    ScanRequest {
        respect_gitignore: options.respect_gitignore,
        include_git_dir: options.include_git_dir,
        max_depth: options.dirs_only.then_some(1),
        metadata: EntryMetadataRequest {
            unix_mode: false,
            symlink_target: true,
            identity: false,
            hardlink_group: false,
        },
    }
}

fn map_engine_error(error: EngineError) -> SyncError {
    SyncError::Io(std::io::Error::other(error))
}

fn resolve_ssh_config(host: &str, user: &Option<String>) -> Result<crate::ssh::config::SshConfig> {
    if let Some(user) = user {
        Ok(crate::ssh::config::SshConfig {
            hostname: host.to_string(),
            user: user.clone(),
            ..Default::default()
        })
    } else {
        crate::ssh::config::parse_ssh_config(host)
    }
}

fn configure_streaming(
    mut streaming: crate::streaming::StreamingSync,
    config: &SyncConfig,
) -> crate::streaming::StreamingSync {
    let comparison = &config.comparison;
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
    if config.verification.verify_on_write {
        streaming = streaming.with_verify(true);
    }
    if let Some(limit) = config.bwlimit {
        streaming = streaming.with_bwlimit(limit);
    }
    if let Some(limit) = config.delete.limit() {
        streaming = streaming.with_max_delete(crate::sync::config::format_delete_limit(limit));
    }
    if config.delete.is_forced() {
        streaming = streaming.with_force_delete(true);
    }
    streaming
}

fn streaming_stats(stats: crate::streaming::SyncStats, duration: std::time::Duration) -> SyncStats {
    SyncStats {
        files_scanned: stats.files_scanned,
        files_created: stats.files_ok,
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

    fn local_session(source: &TempDir, dest: &TempDir, config: SyncConfig) -> SyncSession {
        SyncSession::new(
            EndpointPair::Local(Box::new(LocalEndpoint::new(source.path().to_path_buf()))),
            EndpointPair::Local(Box::new(LocalEndpoint::new(dest.path().to_path_buf()))),
            config,
        )
    }

    #[tokio::test]
    async fn direct_local_uses_incremental_reconciler() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"content").unwrap();

        let first = local_session(&source, &dest, test_config())
            .sync()
            .await
            .unwrap();
        assert_eq!(first.files_created, 1);
        let second = local_session(&source, &dest, test_config())
            .sync()
            .await
            .unwrap();
        assert_eq!(second.files_skipped, 1);
    }

    #[tokio::test]
    async fn checksum_detects_same_size_content_change() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"aaaa").unwrap();
        std::fs::write(dest.path().join("file"), b"bbbb").unwrap();
        let mut config = test_config();
        config.comparison.checksum = true;

        let stats = local_session(&source, &dest, config).sync().await.unwrap();
        assert_eq!(stats.files_updated, 1);
        assert_eq!(std::fs::read(dest.path().join("file")).unwrap(), b"aaaa");
    }

    #[tokio::test]
    async fn verify_compares_content() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"same").unwrap();
        std::fs::write(dest.path().join("file"), b"same").unwrap();
        let session = local_session(&source, &dest, test_config());
        let result = session.verify(source.path(), dest.path()).await.unwrap();
        assert_eq!(result.files_matched, 1);

        std::fs::write(dest.path().join("file"), b"diff").unwrap();
        let result = session.verify(source.path(), dest.path()).await.unwrap();
        assert_eq!(result.files_mismatched.len(), 1);
    }
}
