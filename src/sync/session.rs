//! v0.5 sync session orchestration.
//!
//! Local, push, and pull entry points use the shared ordered controller;
//! remote sessions carry endpoint operations over the v3 protocol.

use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError};
use crate::sync::config::SyncConfig;
use crate::sync::scanner::ScanOptions;
use crate::sync::stats::{SyncError as StatError, SyncStats, VerificationResult};
use std::path::{Path, PathBuf};
use std::time::Instant;
use sy::engine::domain::{Entry, EntryKind, SyncScope};
use sy::engine::reconcile::{EngineError, OrderedReconciler, ReconcileItem};

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
    scope: SyncScope,
}

impl SyncSession {
    pub fn new(source: EndpointPair, dest: EndpointPair, config: SyncConfig) -> Self {
        Self {
            source,
            dest,
            config,
            scan_options: ScanOptions::default(),
            scope: SyncScope::Tree,
        }
    }

    /// Bind user operands once for both synchronization and verification.
    pub async fn for_local_paths(
        source: &Path,
        destination: &Path,
        contents: bool,
        config: SyncConfig,
    ) -> Result<Self> {
        let (source, destination, scope) = super::selected::resolve(
            source.to_path_buf(),
            destination.to_path_buf(),
            contents,
            config.preserve.symlink_mode,
        )
        .await?;
        let mut session = Self::new(
            EndpointPair::Local(Box::new(LocalEndpoint::new(source))),
            EndpointPair::Local(Box::new(LocalEndpoint::new(destination))),
            config,
        );
        session.scope = scope;
        Ok(session)
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
        super::local::run(
            self.source.root(),
            self.dest.root(),
            &self.config,
            self.scan_options,
            self.scope.clone(),
        )
        .await
    }

    /// Verify local source and destination trees through the same strict,
    /// bounded ordered merge used by synchronization. Regular files are
    /// compared with streaming BLAKE3 only after cheap kind/size checks.
    pub async fn verify(&self) -> Result<VerificationResult> {
        let source_endpoint = self.source.as_endpoint().ok_or_else(|| {
            SyncError::Config("source must be local for verification".to_string())
        })?;
        let dest_endpoint = self.dest.as_endpoint().ok_or_else(|| {
            SyncError::Config("destination must be local for verification".to_string())
        })?;

        let source = source_endpoint.root();
        let dest = dest_endpoint.root();
        let started = Instant::now();
        let mut result = VerificationResult {
            files_matched: 0,
            files_mismatched: Vec::new(),
            files_only_in_source: Vec::new(),
            files_only_in_dest: Vec::new(),
            errors: Vec::new(),
            duration: std::time::Duration::ZERO,
        };
        let source_request = super::policy::source_scan_request(&self.config, self.scan_options);
        let destination_request = super::policy::destination_scan_request(&self.config);
        let (source_stream, dest_stream) = match &self.scope {
            SyncScope::Tree => (
                crate::endpoint::local_entry_scan::local_entry_stream(
                    source.to_path_buf(),
                    source_request,
                ),
                crate::endpoint::local_entry_scan::local_entry_stream(
                    dest.to_path_buf(),
                    destination_request,
                ),
            ),
            SyncScope::SelectedLeaf {
                source: source_path,
                destination,
            } => (
                crate::endpoint::local_entry_scan::selected_leaf_stream(
                    source.to_path_buf(),
                    source_path.clone(),
                    source_request,
                    false,
                ),
                crate::endpoint::local_entry_scan::selected_leaf_stream(
                    dest.to_path_buf(),
                    destination.clone(),
                    destination_request,
                    true,
                ),
            ),
        };
        let source_stream =
            super::policy::filtered_source_stream(source_stream, self.config.filter_engine.clone());
        let min_size = self.config.min_size;
        let max_size = self.config.max_size;
        let skip_symlinks = self.config.preserve.symlink_mode == crate::cli::SymlinkMode::Skip;
        let source_stream = source_stream.filter_map(move |entry| {
            let selected = entry.as_ref().map_or(true, |entry| {
                super::policy::entry_in_size_scope(entry, min_size, max_size)
                    && super::policy::entry_selected_by_symlink_mode(entry, skip_symlinks)
            });
            futures::future::ready(selected.then_some(entry))
        });
        let mut reconciler =
            OrderedReconciler::with_scope(source_stream, dest_stream, self.scope.clone());

        let scanned = async {
            while let Some(item) = reconciler.next().await? {
                match item {
                    ReconcileItem::SourceOnly { source: entry, .. } => {
                        result
                            .files_only_in_source
                            .push(source.join(entry.path.as_path()));
                    }
                    ReconcileItem::DestinationOnly(entry) => {
                        if self.scope != SyncScope::Tree {
                            continue;
                        }
                        result
                            .files_only_in_dest
                            .push(dest.join(entry.path.as_path()));
                    }
                    ReconcileItem::Matched {
                        source: source_entry,
                        destination: dest_entry,
                    } => {
                        let relative = source_entry.path.as_path();
                        match entries_match(
                            source_endpoint,
                            dest_endpoint,
                            &source_entry,
                            &dest_entry,
                            self.config.preserve.symlink_mode == crate::cli::SymlinkMode::Follow,
                        )
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

            Ok::<(), EngineError>(())
        }
        .await;
        let drained = reconciler.close().await;
        match (scanned, drained) {
            (Err(operation), Err(drain)) => {
                return Err(SyncError::Io(std::io::Error::other(format!(
                    "verification failed ({operation}) and scan draining failed ({drain})"
                ))))
            }
            (Err(error), _) | (_, Err(error)) => return Err(map_engine_error(error)),
            (Ok(()), Ok(())) => {}
        }
        result.duration = started.elapsed();
        Ok(result)
    }

    async fn streaming_push(&self) -> Result<SyncStats> {
        let (host, user, dest_root) = match &self.dest {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => {
                return Err(SyncError::Config(
                    "destination must be SSH for push".to_string(),
                ))
            }
        };
        super::push::run(
            self.source.root(),
            dest_root,
            host,
            user,
            &self.config,
            self.scan_options,
        )
        .await
    }

    async fn streaming_pull(&self) -> Result<SyncStats> {
        let (host, user, source_root) = match &self.source {
            EndpointPair::Ssh { host, user, root } => (host, user, root),
            _ => return Err(SyncError::Config("source must be SSH for pull".to_string())),
        };
        #[cfg(feature = "ssh")]
        {
            if let Some(reason) = super::pull::pull_unsupported_reason(&self.config) {
                return Err(SyncError::Config(reason.to_string()));
            }
            super::pull::run(
                source_root,
                self.dest.root(),
                host,
                user,
                &self.config,
                self.scan_options,
            )
            .await
        }
        #[cfg(not(feature = "ssh"))]
        {
            let _ = (host, user, source_root);
            Err(SyncError::Config("SSH support is disabled".to_string()))
        }
    }
}

async fn entries_match(
    source_endpoint: &dyn Endpoint,
    dest_endpoint: &dyn Endpoint,
    source: &Entry,
    dest: &Entry,
    follow: bool,
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
    let source_hash = super::policy::observed_hash(source_endpoint, source, follow).await?;
    let dest_hash = super::policy::observed_hash(dest_endpoint, dest, false).await?;
    Ok(source_hash == dest_hash)
}

fn map_engine_error(error: EngineError) -> SyncError {
    SyncError::Io(std::io::Error::other(error))
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
        let result = session.verify().await.unwrap();
        assert_eq!(result.files_matched, 1);

        std::fs::write(dest.path().join("file"), b"diff").unwrap();
        let result = session.verify().await.unwrap();
        assert_eq!(result.files_mismatched.len(), 1);
    }
}
