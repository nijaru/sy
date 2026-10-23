//! v0.5 sync session orchestration.
//!
//! Local, push, and pull entry points use the shared ordered controller;
//! remote sessions carry endpoint operations over the v3 protocol.

use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::error::{Result, SyncError};
use crate::sync::config::SyncConfig;
use crate::sync::scanner::ScanOptions;
use crate::sync::stats::SyncStats;
use crate::sync::verification::{Difference, VerificationResult};
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
        operand: PathBuf,
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
                operand: path.clone(),
            }),
            crate::path::SyncPath::S3 { .. } | crate::path::SyncPath::Gcs { .. } => Err(
                SyncError::Config("S3/GCS endpoints not yet supported".to_string()),
            ),
        }
    }

    /// Operator operand until direction-specific binding captures its root FD.
    pub fn path(&self) -> &Path {
        match self {
            Self::Local(endpoint) => endpoint.root(),
            Self::Ssh { operand, .. } => operand,
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

/// PUSH retains classification and operator provenance separately from each
/// run's child observations. Failed acquisition never retries a new pathname.
enum PushSource {
    Unacquired,
    Captured(sy::rooted_fs::operand::BoundSource),
    Terminal,
}

/// Acquisition failure/cancellation is terminal: another call must never
/// recapture a replacement at the operator spelling.
enum LocalSource {
    Unacquired,
    Captured(sy::endpoint::source_root::SourceRoot),
    Terminal,
}

/// Constructed only after the LOCAL engine returned and all owned work joined.
/// The exact failed owner travels with the outcome; it is not an effect lease.
pub(super) struct DrainedSourceRace {
    source: sy::endpoint::source_root::SourceRoot,
    pub(super) cause: SyncError,
}

pub(super) enum WatchSyncOutcome {
    Complete(SyncStats),
    SourceRace(DrainedSourceRace),
}

pub struct SyncSession {
    source: EndpointPair,
    dest: EndpointPair,
    config: SyncConfig,
    scan_options: ScanOptions,
    scope: SyncScope,
    source_contents: bool,
    local_source: tokio::sync::Mutex<LocalSource>,
    push_source: tokio::sync::Mutex<PushSource>,
}

impl SyncSession {
    pub fn new(source: EndpointPair, dest: EndpointPair, config: SyncConfig) -> Self {
        Self {
            source,
            dest,
            config,
            scan_options: ScanOptions::default(),
            scope: SyncScope::Tree,
            source_contents: true,
            local_source: tokio::sync::Mutex::new(LocalSource::Unacquired),
            push_source: tokio::sync::Mutex::new(PushSource::Unacquired),
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
            EndpointPair::Local(Box::new(LocalEndpoint::new(source.path().to_path_buf()))),
            EndpointPair::Local(Box::new(LocalEndpoint::new(destination))),
            config,
        );
        session.scope = scope;
        session.local_source = tokio::sync::Mutex::new(LocalSource::Captured(source));
        Ok(session)
    }

    pub fn with_source_contents(mut self, contents: bool) -> Self {
        self.source_contents = contents;
        self
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

    async fn acquire_local_source(
        &self,
        owner: &mut LocalSource,
        retry: Option<DrainedSourceRace>,
    ) -> Result<(
        sy::endpoint::source_root::SourceRoot,
        sy::endpoint::source_root::SourceRunAttempt,
    )> {
        use sy::endpoint::local::map_source_rooted_fs_error;
        use sy::endpoint::source_root::SourceRoot;
        let retained_attempt = match (&*owner, &retry) {
            (LocalSource::Captured(original), Some(race)) if original.same_run(&race.source) => {
                Some(
                    original
                        .retry_attempt()
                        .map_err(map_source_rooted_fs_error)?,
                )
            }
            (LocalSource::Captured(original), None) => {
                // A failed race is not usable by ordinary sync. Reject it before
                // consuming the retained owner needed by the explicit retry.
                Some(original.attempt().map_err(map_source_rooted_fs_error)?)
            }
            (LocalSource::Unacquired, None) => None,
            _ => {
                return Err(super::policy::map_io(
                    sy::rooted_fs::RootedFsError::SourceCatalogueTerminal,
                ))
            }
        };
        // Set the irreversible state and arm the old owner's nonblocking
        // cutoff BEFORE any acquisition/replay await. Drop cannot join native
        // work; it denies reuse and never claims a drained retry outcome.
        let previous = std::mem::replace(owner, LocalSource::Terminal);
        let source = match (previous, retry) {
            (LocalSource::Captured(original), Some(_)) => {
                original.retry_after_source_change().await
            }
            (LocalSource::Captured(original), None) => original.begin_run().await,
            (LocalSource::Unacquired, None) => {
                SourceRoot::open(self.source.path().to_path_buf()).await
            }
            _ => {
                return Err(super::policy::map_io(
                    sy::rooted_fs::RootedFsError::SourceCatalogueTerminal,
                ))
            }
        }
        .map_err(map_source_rooted_fs_error)?;
        let attempt = source.attempt().map_err(map_source_rooted_fs_error)?;
        *owner = LocalSource::Captured(source.clone());
        if let Some(retained_attempt) = retained_attempt {
            retained_attempt.finish();
        }
        Ok((source, attempt))
    }

    #[cfg(test)]
    async fn local_source(&self) -> Result<sy::endpoint::source_root::SourceRoot> {
        let mut owner = self.local_source.lock().await;
        let (source, attempt) = self.acquire_local_source(&mut owner, None).await?;
        attempt.finish();
        Ok(source)
    }

    async fn direct_local(&self) -> Result<SyncStats> {
        match self.local_attempt(None).await? {
            WatchSyncOutcome::Complete(stats) => Ok(stats),
            WatchSyncOutcome::SourceRace(race) => Err(race.cause),
        }
    }

    pub(super) async fn sync_for_watch(
        &self,
        retry: Option<DrainedSourceRace>,
    ) -> Result<WatchSyncOutcome> {
        if self.select_strategy() != SyncStrategy::DirectLocal {
            // Reaping OpenSSH does not prove terminal remote native drain.
            return self.sync().await.map(WatchSyncOutcome::Complete);
        }
        self.local_attempt(retry).await
    }

    async fn local_attempt(&self, retry: Option<DrainedSourceRace>) -> Result<WatchSyncOutcome> {
        // Serialize acquisition, execution, drain and successor installation.
        let mut owner = self.local_source.lock().await;
        let (source, attempt) = self.acquire_local_source(&mut owner, retry).await?;
        // Preflight closes both producers; execute_until joins the controller,
        // native workers and removal owner. Composite errors remain intact.
        let result = super::local::run_acquired(
            source.clone(),
            self.dest.path(),
            &self.config,
            self.scan_options,
            self.scope.clone(),
        )
        .await;
        let outcome = match result {
            Ok(stats) => Ok(WatchSyncOutcome::Complete(stats)),
            Err(cause) => {
                let eligible = super::local::is_drained_source_race(&cause);
                let retired = source.retire_failed_run(eligible).await;
                if eligible {
                    retired.map_err(super::policy::map_io)?;
                    Ok(WatchSyncOutcome::SourceRace(DrainedSourceRace {
                        source,
                        cause,
                    }))
                } else {
                    Err(cause)
                }
            }
        };
        attempt.finish();
        outcome
    }

    /// Verify local source and destination trees through the same strict,
    /// bounded ordered merge used by synchronization. Regular files are
    /// compared with streaming BLAKE3 only after cheap kind/size checks.
    pub async fn verify(&self) -> Result<VerificationResult> {
        self.source.as_endpoint().ok_or_else(|| {
            SyncError::Config("source must be local for verification".to_string())
        })?;
        let dest_endpoint = self.dest.as_endpoint().ok_or_else(|| {
            SyncError::Config("destination must be local for verification".to_string())
        })?;

        let mut owner = self.local_source.lock().await;
        let (source_authority, attempt) = self.acquire_local_source(&mut owner, None).await?;
        source_authority
            .validate()
            .await
            .map_err(sy::endpoint::local::map_source_rooted_fs_error)?;
        let source_endpoint = source_authority.endpoint();
        let source = source_endpoint.root();
        let dest = dest_endpoint.root();
        let started = Instant::now();
        let mut result = VerificationResult::default();
        let source_request = super::policy::source_scan_request(&self.config, self.scan_options);
        let destination_request = super::policy::destination_scan_request(&self.config);
        let (source_stream, dest_stream) = match &self.scope {
            SyncScope::Tree => (
                source_authority.entries(source_request),
                crate::endpoint::local_entry_scan::local_entry_stream(
                    dest.to_path_buf(),
                    destination_request,
                ),
            ),
            SyncScope::SelectedLeaf {
                source: source_path,
                destination,
            } => (
                source_authority.selected_entries(source_path.clone(), source_request, false),
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
                        result.record_difference(
                            Difference::SourceOnly,
                            source,
                            entry.path.as_path(),
                        );
                    }
                    ReconcileItem::DestinationOnly(entry) => {
                        if self.scope != SyncScope::Tree {
                            continue;
                        }
                        result.record_difference(
                            Difference::DestinationOnly,
                            dest,
                            entry.path.as_path(),
                        );
                    }
                    ReconcileItem::Matched {
                        source: source_entry,
                        destination: dest_entry,
                    } => {
                        let relative = source_entry.path.as_path();
                        match entries_match(
                            &source_endpoint,
                            dest_endpoint,
                            &source_entry,
                            &dest_entry,
                            self.config.preserve.symlink_mode == crate::cli::SymlinkMode::Follow,
                        )
                        .await
                        {
                            Ok(true) => result.files_matched += 1,
                            Ok(false) => {
                                result.record_difference(Difference::Mismatch, source, relative)
                            }
                            Err(error) => result.record_error(source, relative, &error),
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
        attempt.finish();
        Ok(result)
    }

    async fn push_source(&self) -> Result<sy::rooted_fs::operand::BoundSource> {
        use sy::endpoint::source_root::SourceRoot;
        use sy::rooted_fs::operand::BoundSource;
        let mut owner = self.push_source.lock().await;
        match &mut *owner {
            PushSource::Unacquired => {
                // Cancellation or failure during first acquisition cannot
                // cause a later run to select a replacement SOURCE.
                *owner = PushSource::Terminal;
                let bound = if self.config.preserve.symlink_mode == crate::cli::SymlinkMode::Follow
                {
                    BoundSource::open_local_following(
                        self.source.path().to_path_buf(),
                        self.source_contents,
                    )
                    .await
                } else {
                    BoundSource::open_local(self.source.path().to_path_buf(), self.source_contents)
                        .await
                }
                .map_err(super::policy::map_io)?;
                *owner = PushSource::Captured(bound.clone());
                Ok(bound)
            }
            PushSource::Captured(bound) => {
                let source = SourceRoot::from_rooted(bound.rooted.clone())
                    .map_err(super::policy::map_io)?
                    .begin_run()
                    .await
                    .map_err(super::policy::map_io)?;
                bound.rooted = source.rooted();
                Ok(bound.clone())
            }
            PushSource::Terminal => Err(super::policy::map_io(
                sy::rooted_fs::RootedFsError::SourceCatalogueTerminal,
            )),
        }
    }

    async fn streaming_push(&self) -> Result<SyncStats> {
        let (host, user, dest_root) = match &self.dest {
            EndpointPair::Ssh {
                host,
                user,
                operand,
            } => (host, user, operand),
            _ => {
                return Err(SyncError::Config(
                    "destination must be SSH for push".to_string(),
                ))
            }
        };
        let bound = self.push_source().await?;
        let source = sy::endpoint::source_root::SourceRoot::from_rooted(bound.rooted.clone())
            .map_err(super::policy::map_io)?;
        let attempt = source.attempt().map_err(super::policy::map_io)?;
        let result = super::push::run_acquired(
            bound,
            dest_root,
            host,
            user,
            &self.config,
            self.scan_options,
        )
        .await;
        if result.is_err() {
            // No remote terminal-native-drain certificate exists. Even a data
            // race retires this owner permanently; it cannot enable watch retry.
            let _ = source.retire_failed_run(false).await;
        }
        attempt.finish();
        result
    }

    async fn streaming_pull(&self) -> Result<SyncStats> {
        let (host, user, source_root) = match &self.source {
            EndpointPair::Ssh {
                host,
                user,
                operand,
            } => (host, user, operand),
            _ => return Err(SyncError::Config("source must be SSH for pull".to_string())),
        };
        #[cfg(feature = "ssh")]
        {
            if let Some(reason) = super::pull::pull_unsupported_reason(&self.config) {
                return Err(SyncError::Config(reason.to_string()));
            }
            super::pull::run(
                source_root,
                self.dest.path(),
                host,
                user,
                &self.config,
                self.scan_options,
                self.source_contents,
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

    fn push_session(source: &Path, config: SyncConfig) -> SyncSession {
        SyncSession::new(
            EndpointPair::Local(Box::new(LocalEndpoint::new(source.to_path_buf()))),
            EndpointPair::Ssh {
                host: "must-not-connect.invalid".into(),
                user: None,
                operand: PathBuf::from("/must-not-create"),
            },
            config,
        )
    }

    fn assert_source_root_changed(error: SyncError, path: &Path) {
        let SyncError::Io(error) = error else {
            panic!("expected typed original SOURCE error, got {error:?}")
        };
        assert!(
            matches!(
                error.get_ref().and_then(|cause| cause.downcast_ref::<sy::rooted_fs::RootedFsError>()),
                Some(sy::rooted_fs::RootedFsError::RootChanged(changed)) if changed == path
            ),
            "wrong SOURCE failure: {error:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_push_rejects_foreign_operator_before_connection() {
        use futures::TryStreamExt;
        use sy::endpoint::source_root::SourceRoot;
        for case in ["root", "ancestor", "link"] {
            let fixture = TempDir::new().unwrap();
            let ancestor = fixture.path().join("ancestor");
            let root = ancestor.join("tree");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("file"), b"original").unwrap();
            let operand = if case == "link" {
                let link = fixture.path().join("selected");
                std::os::unix::fs::symlink("ancestor/tree", &link).unwrap();
                link.join(".")
            } else {
                root.clone()
            };
            let mut config = test_config();
            config.delete = DeleteMode::Enabled {
                limit: crate::sync::config::DeleteLimit::Unlimited,
                force: false,
            };
            let session = push_session(&operand, config);
            // Exercise the real session acquisition and first scan without an
            // SSH peer. Only the connection boundary is deliberately absent.
            let bound = session.push_source().await.unwrap();
            let source = SourceRoot::from_rooted(bound.rooted).unwrap();
            source
                .entries(Default::default())
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            match case {
                "root" => {
                    std::fs::rename(&root, fixture.path().join("held")).unwrap();
                    // An empty replacement must not authorize --delete.
                    std::fs::create_dir(&root).unwrap();
                }
                "ancestor" => {
                    let held = fixture.path().join("held");
                    std::fs::rename(&ancestor, &held).unwrap();
                    std::fs::create_dir(&ancestor).unwrap();
                    std::fs::rename(held.join("tree"), &root).unwrap();
                }
                "link" => {
                    let link = fixture.path().join("selected");
                    std::fs::remove_file(&link).unwrap();
                    std::os::unix::fs::symlink(&root, &link).unwrap();
                }
                _ => unreachable!(),
            }
            // This assertion also makes the regression probe safe: fresh
            // acquisition fails here rather than ever reaching OpenSSH.
            assert_source_root_changed(session.push_source().await.unwrap_err(), source.path());
            assert_source_root_changed(session.sync().await.unwrap_err(), source.path());
            if case == "root" {
                assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
                assert_eq!(
                    std::fs::read(fixture.path().join("held/file")).unwrap(),
                    b"original"
                );
            } else {
                assert_eq!(std::fs::read(root.join("file")).unwrap(), b"original");
            }
        }
    }

    #[tokio::test]
    async fn repeated_push_refreshes_children_through_original_shape_and_root() {
        use futures::TryStreamExt;
        use sy::endpoint::source_root::SourceRoot;
        let fixture = TempDir::new().unwrap();
        std::fs::write(fixture.path().join("first"), b"original").unwrap();
        let session = push_session(fixture.path(), test_config()).with_source_contents(false);
        let first = session.push_source().await.unwrap();
        assert!(matches!(&first.shape,
            sy::rooted_fs::operand::SourceShape::Directory { basename: Some(name) }
            if name.as_path() == Path::new(fixture.path().file_name().unwrap())));
        let original = SourceRoot::from_rooted(first.rooted).unwrap();
        original
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        std::fs::write(fixture.path().join("second"), b"fresh child").unwrap();
        let next = session.push_source().await.unwrap();
        assert!(matches!(&next.shape,
            sy::rooted_fs::operand::SourceShape::Directory { basename: Some(name) }
            if name.as_path() == Path::new(fixture.path().file_name().unwrap())));
        let next = SourceRoot::from_rooted(next.rooted).unwrap();
        let entries: Vec<_> = next
            .entries(Default::default())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        next.validate().await.unwrap();
        assert!(original
            .rooted()
            .open_regular_blocking(&sy::engine::domain::RelativePath::new("second").unwrap())
            .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watch_race_outcome_waits_for_native_work_and_retries_exact_failed_owner() {
        use sy::rooted_fs::{PublicationPause, PublicationPausePoint};
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"old").unwrap();
        let session = std::sync::Arc::new(local_session(&source, &dest, test_config()));
        let authority = session.local_source().await.unwrap();
        let (reached, wait) = tokio::sync::oneshot::channel();
        let (resume, receive) = std::sync::mpsc::channel();
        authority.rooted().pause_mutation_at(
            0,
            PublicationPause {
                point: PublicationPausePoint::AfterSourceValidation,
                reached,
                resume: receive,
            },
        );
        let running = session.clone();
        let mut attempt = tokio::spawn(async move { running.sync_for_watch(None).await });
        wait.await.unwrap();
        std::fs::write(source.path().join("file"), b"new source data").unwrap();
        assert!(
            futures::poll!(&mut attempt).is_pending(),
            "drained outcome cannot precede native completion"
        );
        assert!(!dest.path().join("file").exists());
        resume.send(()).unwrap();
        let WatchSyncOutcome::SourceRace(race) = attempt.await.unwrap().unwrap() else {
            panic!("expected classified joined race")
        };
        assert!(authority.same_run(&race.source));
        assert!(
            session.sync().await.is_err(),
            "ordinary sync remains fail-fast"
        );
        std::fs::write(source.path().join("arrival"), b"new child").unwrap();
        assert!(matches!(
            session.sync_for_watch(Some(race)).await.unwrap(),
            WatchSyncOutcome::Complete(_)
        ));
        assert_eq!(
            std::fs::read(dest.path().join("file")).unwrap(),
            b"new source data"
        );
        assert_eq!(
            std::fs::read(dest.path().join("arrival")).unwrap(),
            b"new child"
        );
        assert!(authority.validate().await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_refresh_preserves_source_role_for_original_ancestor_change() {
        let fixture = TempDir::new().unwrap();
        let ancestor = fixture.path().join("ancestor");
        let parent = ancestor.join("parent");
        let source = parent.join("source");
        let dest = TempDir::new().unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("file"), b"original").unwrap();
        let session = SyncSession::for_local_paths(&source, dest.path(), true, test_config())
            .await
            .unwrap();
        session.sync().await.unwrap();
        std::fs::write(source.join("arrival"), b"fresh child").unwrap();
        session.sync().await.unwrap();
        assert_eq!(
            std::fs::read(dest.path().join("arrival")).unwrap(),
            b"fresh child"
        );
        let held = fixture.path().join("held");
        std::fs::rename(&ancestor, &held).unwrap();
        std::fs::create_dir(&ancestor).unwrap();
        // ROOT and immediate parent stay genuine; only the higher original
        // operator ancestor changes. Verification must not label it DEST.
        std::fs::rename(held.join("parent"), &parent).unwrap();
        assert_source_root_changed(session.verify().await.unwrap_err(), &source);
        assert_eq!(std::fs::read(source.join("file")).unwrap(), b"original");
        assert_eq!(
            std::fs::read(dest.path().join("file")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_local_acquisition_never_recaptures_or_revives_source() {
        use sy::endpoint::source_root::SourceRoot;
        use sy::rooted_fs::{PublicationPause, PublicationPausePoint, RootedFsError};
        // Occupy the only blocking worker: acquisition/replay is genuinely
        // queued at its native boundary, without adding production test hooks.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            for case in ["initial-sync", "initial-verify", "completed", "source-race"] {
                let source = TempDir::new().unwrap();
                let dest = TempDir::new().unwrap();
                std::fs::write(source.path().join("file"), b"original").unwrap();
                let session = local_session(&source, &dest, test_config());
                let mut retry = None;
                let original: Option<SourceRoot> = match case {
                    "completed" => {
                        session.sync().await.unwrap();
                        let owner = session.local_source.lock().await;
                        let LocalSource::Captured(original) = &*owner else { panic!("missing owner") };
                        Some(original.clone())
                    }
                    "source-race" => {
                        let original = session.local_source().await.unwrap();
                        let (reached, wait) = tokio::sync::oneshot::channel();
                        let (resume, receive) = std::sync::mpsc::channel();
                        original.rooted().pause_mutation_at(0, PublicationPause {
                            point: PublicationPausePoint::AfterSourceValidation,
                            reached,
                            resume: receive,
                        });
                        let change = async {
                            wait.await.unwrap();
                            std::fs::write(source.path().join("file"), b"new DATA").unwrap();
                            resume.send(()).unwrap();
                        };
                        let (outcome, ()) = tokio::join!(session.sync_for_watch(None), change);
                        let WatchSyncOutcome::SourceRace(race) = outcome.unwrap() else {
                            panic!("expected drained SOURCE race")
                        };
                        assert!(original.same_run(&race.source));
                        retry = Some(race);
                        Some(original)
                    }
                    _ => None,
                };
                let (reached, wait) = tokio::sync::oneshot::channel();
                let (resume, receive) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    reached.send(()).unwrap();
                    receive.recv().unwrap();
                });
                wait.await.unwrap();
                let mut attempt: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>>>> =
                    match case {
                        "initial-verify" => Box::pin(async { session.verify().await.map(|_| ()) }),
                        "source-race" => Box::pin(async { session.sync_for_watch(retry).await.map(|_| ()) }),
                        _ => Box::pin(async { session.sync().await.map(|_| ()) }),
                    };
                assert!(futures::poll!(&mut attempt).is_pending());
                drop(attempt);
                // Prove cutoff while the ORIGINAL spelling is still intact,
                // before queued replay can detect our later root replacement.
                if let Some(original) = &original {
                    if case == "source-race" {
                        assert!(matches!(original.retry_attempt(),
                            Err(RootedFsError::SourceCatalogueTerminal)), "failed proof retained retry authority");
                    } else {
                        assert!(matches!(original.validate_blocking(),
                            Err(RootedFsError::SourceCatalogueTerminal)), "completed owner remained usable");
                    }
                }
                let held = source.path().with_extension("held");
                std::fs::rename(source.path(), &held).unwrap();
                std::fs::create_dir(source.path()).unwrap();
                std::fs::write(source.path().join("file"), b"replacement").unwrap();
                resume.send(()).unwrap();
                blocker.await.unwrap();
                for error in [session.sync().await.unwrap_err(), session.verify().await.unwrap_err()] {
                    assert!(matches!(error, SyncError::Io(ref error)
                        if matches!(error.get_ref().and_then(|cause| cause.downcast_ref::<RootedFsError>()),
                            Some(RootedFsError::SourceCatalogueTerminal))), "{case}: {error:?}");
                }
                if let Some(original) = original {
                    assert!(original.validate().await.is_err(), "{case}: old owner revived");
                    assert!(original.begin_run().await.is_err());
                    assert!(original.retry_after_source_change().await.is_err());
                }
                assert_eq!(std::fs::read(held.join("file")).unwrap(),
                    if case == "source-race" { b"new DATA".as_slice() } else { b"original".as_slice() });
                assert_eq!(std::fs::read(source.path().join("file")).unwrap(), b"replacement");
                assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), usize::from(case == "completed"));
                std::fs::remove_dir_all(held).unwrap();
            }
        });
    }

    #[tokio::test]
    async fn direct_local_uses_incremental_reconciler() {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        std::fs::write(source.path().join("file"), b"content").unwrap();

        // Size+mtime quick equality needs timestamps preserved on the first
        // transfer; bare copies now consistently honor the absence of -t.
        let mut config = test_config();
        config.preserve.times = true;
        let first = local_session(&source, &dest, config.clone())
            .sync()
            .await
            .unwrap();
        assert_eq!(first.files_created, 1);
        let second = local_session(&source, &dest, config).sync().await.unwrap();
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
