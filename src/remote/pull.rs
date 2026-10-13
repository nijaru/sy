//! v3 pull execution: remote source, local destination.
//!
//! The pull inverts the push executor's role split while keeping the same
//! safety ordering. The remote is a pure SOURCE: it serves scans, content
//! hashes, and whole/delta fetches, and the session router rejects
//! mutation requests in Pull sessions. The client owns reconciliation,
//! the bounded delete journal, local transactional staging, and metadata.
//!
//! Eligible changed large regular files upload adaptive OLD signatures and
//! reconstruct streamed operations into private local staging. SOURCE stays
//! read-only; both directions retain BLAKE3-before-publication integrity.

use crate::endpoint::io::{ExpectedDestination, VerificationStatus};
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::engine::compression::CompressionPolicy;
use crate::engine::domain::{Entry, EntryKind, RelativePath, Timestamp};
use crate::engine::scheduler::{ResourceRequest, Scheduler};
use crate::engine::work::WorkItem;
use crate::remote::acl::{read_preserved_acls, AclLocation, RemoteAclError};
use crate::remote::bsdflags::{read_preserved_bsd_flags, BsdFlagsLocation, RemoteBsdFlagsError};
use crate::remote::fetch::{fetch_file, FetchPolicy, FetchPreservationRequest};
use crate::remote::router::RouterSender;
use crate::remote::runtime::{ClientRemoteHandle, RemoteSessionError};
use crate::remote::xattr::{read_preserved_xattrs, RemoteXattrError, XattrLocation};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bounded working set per in-flight fetch; mirrors the push side so the
/// scheduler byte budget is direction-symmetric.
pub const REMOTE_FETCH_WORKING_SET: u64 = 8 * 1024 * 1024;

mod group;

#[cfg(all(test, unix))]
#[path = "pull/group_tests.rs"]
mod group_tests;

#[derive(Debug, thiserror::Error)]
pub enum RemotePullError {
    #[error("scanned destination address does not match transfer target {0}")]
    DestinationAddressMismatch(RelativePath),
    #[error("distinct directory addresses require subtree finalization support at {0}")]
    DirectoryAddressUnsupported(RelativePath),
    #[error(transparent)]
    Group(#[from] group::PullGroupError),
    #[error("pull group at {path} may be published, but source completion is uncertain: {source}")]
    SourceCompletionUncertain {
        path: PathBuf,
        #[source]
        source: Box<RemotePullError>,
    },
    #[error(transparent)]
    Remote(#[from] RemoteSessionError),

    #[error(transparent)]
    Transfer(#[from] crate::remote::transfer::RemoteTransferError),

    #[error(transparent)]
    Scheduler(#[from] crate::engine::scheduler::SchedulerError),

    #[error("symlink action is missing the scanned target for {0}")]
    MissingSymlinkTarget(PathBuf),

    #[error("--backup location for {0} is not representable beneath the destination root")]
    InvalidBackupPath(PathBuf),

    #[error("local destination mutation failed for {0}: {1}")]
    LocalMutation(PathBuf, std::io::Error),

    #[error(transparent)]
    Rooted(#[from] crate::rooted_fs::RootedFsError),

    #[error(transparent)]
    Existing(#[from] crate::endpoint::existing::ExistingDestinationError),

    #[error("regular-file pull requires Unix mode metadata for {0}")]
    MissingScannedMode(PathBuf),

    #[error("staged pull verification failed for {path}: expected {expected}, got {actual}")]
    StagedVerificationFailed {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("pull destination lacks a scan-time identity for {0}")]
    MissingDestinationIdentity(PathBuf),

    #[error("destination {path} committed, but the fetch acknowledgement failed: {reason}")]
    CommittedAckFailed { path: PathBuf, reason: String },

    #[error(transparent)]
    Endpoint(#[from] crate::error::SyncError),

    #[error("transactional type replacement is not implemented for directory transition at {0}")]
    TransactionalDirectoryReplace(PathBuf),

    #[error(transparent)]
    Xattr(#[from] RemoteXattrError),

    #[error(transparent)]
    Acl(#[from] RemoteAclError),

    #[error(transparent)]
    BsdFlags(#[from] RemoteBsdFlagsError),
}

pub type Result<T> = std::result::Result<T, RemotePullError>;

fn rooted_destination_expectation(
    expected: ExpectedDestination,
) -> crate::endpoint::ExpectedDestination {
    match expected {
        ExpectedDestination::Absent => crate::endpoint::ExpectedDestination::Absent,
        ExpectedDestination::Unchanged(identity) => {
            crate::endpoint::ExpectedDestination::Unchanged(identity)
        }
        ExpectedDestination::SnapshotAtOpen | ExpectedDestination::Unverified => {
            crate::endpoint::ExpectedDestination::SnapshotAtOpen
        }
    }
}

fn destination_expectation(destination: Option<&Entry>) -> Result<ExpectedDestination> {
    match destination {
        None => Ok(ExpectedDestination::Absent),
        Some(entry) => entry
            .identity
            .map(ExpectedDestination::Unchanged)
            .ok_or_else(|| {
                RemotePullError::MissingDestinationIdentity(entry.path.as_path().to_path_buf())
            }),
    }
}

#[derive(Debug, Clone)]
pub enum RemotePullAction {
    CreateDirectory {
        source: Entry,
        destination_path: RelativePath,
    },
    FetchFile {
        source: Entry,
        destination_path: RelativePath,
        destination: Option<Entry>,
        metadata: PullTransferMetadata,
    },
    ReplaceSymlink {
        source: Entry,
        destination_path: RelativePath,
        destination: Option<Entry>,
        modified: Option<Timestamp>,
    },
    ApplyMetadata {
        source: Entry,
        destination: Entry,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PullTransferMetadata {
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
}

/// Executes lowered v3 pull work against the local destination. The caller
/// (the shared controller loop) owns tree ordering, deletion commit, and
/// finalize replay; this type owns per-item admission, fetch streaming,
/// staging, and commit.
pub struct RemotePullExecutor {
    destination_root: PathBuf,
    /// The sole acquired execution authority, shared by scans and all effects.
    metadata_authority: tokio::sync::OnceCell<crate::rooted_fs::RootedFs>,
    /// Initial classification/acquisition seed, never used to adopt another root.
    destination_binding: tokio::sync::OnceCell<crate::rooted_fs::operand::BoundDestination>,
    remote: ClientRemoteHandle,
    sender: RouterSender,
    run_admission: Arc<crate::endpoint::publication::PublicationAdmission>,
    scheduler: Scheduler,
    /// --backup: enabled flag, the destination-anchored backup directory
    /// (None = beside-the-file suffix backups), and the suffix.
    backup: bool,
    backup_dir: Option<std::path::PathBuf>,
    backup_suffix: String,
    backup_root: Option<crate::rooted_fs::backup::BackupRoot>,
    reporter: Option<Arc<crate::sync::output::SyncReporter>>,
    rate_limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// -z/--compress: per-chunk zstd for fetches. The negotiated ZSTD
    /// capability is checked before the first request, mirroring the push
    /// side's transfer_file_with_policy contract.
    compression: Option<CompressionPolicy>,
    /// Sealed catalogue groups execute once through the shared controller.
    hardlinks: bool,
    /// -X/--preserve-xattrs: mirror the remote source's extended attributes
    /// onto the local destination for every entry this executor creates or
    /// updates. Symlinks are skipped (their attributes are not portable).
    xattrs: bool,
    /// -A/--preserve-acls: mirror the remote source's access-control list
    /// onto the local destination for every entry this executor creates or
    /// updates. Symlinks are skipped, like xattrs.
    acls: bool,
    /// -F/--preserve-flags (macOS only): mirror the remote source's BSD
    /// file flags onto the local destination for every entry this executor
    /// creates or updates. Symlinks are skipped, like xattrs.
    bsd_flags: bool,
}

impl RemotePullExecutor {
    pub fn new(
        destination_root: PathBuf,
        remote: ClientRemoteHandle,
        sender: RouterSender,
        scheduler: Scheduler,
    ) -> Self {
        Self {
            destination_root,
            metadata_authority: tokio::sync::OnceCell::new(),
            destination_binding: tokio::sync::OnceCell::new(),
            remote,
            sender,
            run_admission: Arc::new(crate::endpoint::publication::PublicationAdmission::default()),
            scheduler,
            backup: false,
            backup_dir: None,
            backup_suffix: String::new(),
            backup_root: None,
            reporter: None,
            rate_limiter: None,
            compression: None,
            hardlinks: false,
            xattrs: false,
            acls: false,
            bsd_flags: false,
        }
    }

    pub fn with_destination_binding(
        mut self,
        binding: crate::rooted_fs::operand::BoundDestination,
    ) -> Self {
        self.destination_root = binding.root.path().to_path_buf();
        self.destination_binding = tokio::sync::OnceCell::new_with(Some(binding));
        self
    }

    async fn binding(&self) -> Result<&crate::rooted_fs::operand::BoundDestination> {
        let binding = self
            .destination_binding
            .get_or_try_init(|| async {
                crate::rooted_fs::operand::BoundDestination::open(
                    self.destination_root.clone(),
                    crate::rooted_fs::operand::SourceShape::Directory { basename: None },
                )
                .await
            })
            .await?;
        Ok(binding)
    }

    /// Initial acquisition is authorized only after full preflight chose Create.
    pub async fn acquire_destination_for_create(&self, create: &RelativePath) -> Result<()> {
        if self.metadata_authority.get().is_some() {
            self.metadata_authority().await?;
            return Ok(());
        }
        let binding = self.binding().await?;
        if binding.name.as_ref().is_some_and(|name| name != create) {
            return Err(RemotePullError::DestinationAddressMismatch(create.clone()));
        }
        if matches!(
            binding.root,
            crate::rooted_fs::operand::DestinationRoot::Present(_)
        ) {
            self.metadata_authority().await?;
            return Ok(());
        }
        let mut root = binding.root.clone();
        root.bind_mutations(self.sender.publication_admission(), false);
        root.bind_run_mutations(Arc::clone(&self.run_admission));
        let rooted = root.acquire_local().await?;
        let held = rooted.clone();
        tokio::task::spawn_blocking(move || held.verify_root_path_blocking())
            .await
            .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        self.metadata_authority.set(rooted).map_err(|_| {
            crate::rooted_fs::RootedFsError::RootChanged(self.destination_root.clone())
        })?;
        Ok(())
    }

    pub fn with_backup_root(mut self, root: Option<crate::rooted_fs::backup::BackupRoot>) -> Self {
        self.backup_root = root;
        self
    }

    pub fn with_backup(mut self, backup_dir: Option<std::path::PathBuf>) -> Self {
        self.backup_dir = backup_dir;
        self
    }

    /// The --backup suffix (default `~`); only applied when --backup is on.
    pub fn with_backup_suffix(mut self, suffix: String) -> Self {
        self.backup_suffix = suffix;
        self
    }

    /// Enable --backup explicitly (the suffix alone is not a marker: an
    /// empty --suffix value must not silently disable backup).
    pub fn with_backup_enabled(mut self, enabled: bool) -> Self {
        self.backup = enabled;
        self
    }

    pub fn with_reporter(
        mut self,
        reporter: Option<Arc<crate::sync::output::SyncReporter>>,
    ) -> Self {
        self.reporter = reporter;
        self
    }

    pub fn with_compression(mut self, policy: Option<CompressionPolicy>) -> Self {
        self.compression = policy;
        self
    }

    pub const fn with_hardlinks(mut self, enabled: bool) -> Self {
        self.hardlinks = enabled;
        self
    }

    pub const fn with_xattrs(mut self, enabled: bool) -> Self {
        self.xattrs = enabled;
        self
    }

    pub const fn with_acls(mut self, enabled: bool) -> Self {
        self.acls = enabled;
        self
    }

    pub const fn with_bsd_flags(mut self, enabled: bool) -> Self {
        self.bsd_flags = enabled;
        self
    }

    pub fn with_rate_limiter(
        mut self,
        limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    ) -> Self {
        self.rate_limiter = limiter;
        self
    }

    fn report(
        &self,
        op: crate::sync::output::ItemizeOp,
        kind: crate::sync::output::ItemizeKind,
        path: &RelativePath,
    ) {
        if let Some(reporter) = &self.reporter {
            reporter.operation(op, kind, path.as_path());
        }
    }

    async fn metadata_authority(&self) -> Result<&crate::rooted_fs::RootedFs> {
        let authority = self
            .metadata_authority
            .get_or_try_init(|| async {
                let binding = self.binding().await?;
                let crate::rooted_fs::operand::DestinationRoot::Present(rooted) = &binding.root
                else {
                    return Err(RemotePullError::Remote(RemoteSessionError::PendingRoot));
                };
                let mut rooted = rooted.clone();
                rooted.bind_session_mutations(self.sender.publication_admission(), false);
                rooted.bind_run_mutations(Arc::clone(&self.run_admission));
                Ok::<_, RemotePullError>(rooted)
            })
            .await?;
        let rooted = authority.clone();
        tokio::task::spawn_blocking(move || rooted.verify_root_path_blocking())
            .await
            .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        Ok(authority)
    }

    /// Prepare the destination's namespace observation and scan together,
    /// using the same authority as execution. All fallible discovery precedes
    /// producer admission. A pending root is empty and unqualified.
    pub async fn destination_entries(
        &mut self,
        request: crate::engine::scan::ScanRequest,
    ) -> Result<(
        Arc<dyn crate::engine::namespace::NamespaceQualification>,
        crate::engine::reconcile::EntryStream,
    )> {
        #[cfg(unix)]
        {
            let mut binding = self.binding().await?.clone();
            let original = match self.metadata_authority.get() {
                Some(rooted) => crate::rooted_fs::operand::DestinationRoot::Present(rooted.clone()),
                None => binding.root.clone(),
            };
            let (bound, namespace) = original.with_namespace_qualification().await?;
            binding.root = bound;
            self.destination_binding = tokio::sync::OnceCell::new_with(Some(binding));
            self.metadata_authority = tokio::sync::OnceCell::new();
            if matches!(
                self.binding().await?.root,
                crate::rooted_fs::operand::DestinationRoot::Pending(_)
            ) {
                return Ok((
                    Arc::new(namespace),
                    crate::engine::reconcile::EntryStream::new(futures::stream::empty()),
                ));
            }
            let rooted = self.metadata_authority().await?;
            let entries = match &self.binding().await?.name {
                Some(name) => rooted.selected_entry_stream(name.clone(), request),
                None => rooted.entry_stream(request),
            };
            Ok((Arc::new(namespace), entries))
        }
        #[cfg(not(unix))]
        {
            let _ = request;
            Err(crate::rooted_fs::RootedFsError::UnsupportedPlatform.into())
        }
    }

    pub async fn destination_content_hash(&self, entry: Entry) -> Result<[u8; 32]> {
        let fingerprint = crate::endpoint::existing::fingerprint(
            self.metadata_authority().await?.clone(),
            entry,
            Default::default(),
        )
        .await?;
        self.metadata_authority().await?;
        Ok(fingerprint.content)
    }

    fn dest_path(&self, relative: &RelativePath) -> PathBuf {
        self.destination_root.join(relative.as_path())
    }

    pub async fn execute(
        &self,
        item: WorkItem<RemotePullAction>,
    ) -> Result<crate::engine::work::WorkResult> {
        let (action, resources) = item.into_parts();
        if self.hardlinks
            && matches!(
                &action,
                RemotePullAction::FetchFile { source, .. }
                    | RemotePullAction::ApplyMetadata { source, .. }
                    if source.hardlink_group.is_some()
            )
        {
            return Err(crate::engine::hardlink_runtime::GroupError::Invalid(
                "hardlink members require sealed whole-group execution",
            )
            .into());
        }
        if let RemotePullAction::FetchFile {
            destination_path,
            destination: Some(observed),
            ..
        }
        | RemotePullAction::ReplaceSymlink {
            destination_path,
            destination: Some(observed),
            ..
        } = &action
        {
            if destination_path != &observed.path {
                return Err(RemotePullError::DestinationAddressMismatch(
                    destination_path.clone(),
                ));
            }
        }
        let _permit = self.scheduler.acquire(resources).await?;

        match action {
            RemotePullAction::CreateDirectory {
                source,
                destination_path,
            } => {
                let expected = source.identity.ok_or_else(|| {
                    crate::rooted_fs::RootedFsError::DestinationChanged(
                        source.path.as_path().to_path_buf(),
                    )
                })?;
                self.remote
                    .read_directory_preservation(&source.path, expected, Default::default())
                    .await?;
                let rooted = self.metadata_authority().await?.clone();
                let relative = destination_path.clone();
                let identity = tokio::task::spawn_blocking(move || {
                    rooted.verify_root_path_blocking()?;
                    let identity = rooted.create_directory_blocking(&relative)?;
                    crate::endpoint::local::verify_committed_root(
                        &rooted,
                        &rooted.root_path().join(relative.as_path()),
                    )?;
                    Ok::<_, RemotePullError>(identity)
                })
                .await
                .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Directory,
                    &destination_path,
                );
                Ok(crate::engine::work::WorkResult::DirectoryPrepared(identity))
            }
            RemotePullAction::FetchFile {
                source,
                destination_path,
                destination,
                metadata,
            } => {
                let is_update = destination.is_some();
                let expected_destination = destination_expectation(destination.as_ref())?;
                self.validate_file_fetch_options()?;
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                // --backup preserves the replaced destination file first, as a
                // local copy (the destination is local in a pull). A backup
                // failure aborts the fetch so the user's copy cannot be
                // silently skipped.
                self.backup_replacement(destination.as_ref()).await?;
                let (summary, _) = self
                    .fetch_into_staging(
                        &source,
                        &destination_path,
                        expected_destination,
                        destination.as_ref(),
                        &metadata,
                        bsd_flags,
                    )
                    .await?;
                let op = if is_update {
                    crate::sync::output::ItemizeOp::Update
                } else {
                    crate::sync::output::ItemizeOp::Create
                };
                self.report(
                    op,
                    crate::sync::output::ItemizeKind::File,
                    &destination_path,
                );
                Ok(crate::engine::work::WorkResult::Transfer(summary))
            }
            RemotePullAction::ReplaceSymlink {
                source,
                destination_path,
                destination,
                modified,
            } => {
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    RemotePullError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
                self.backup_replacement(destination.as_ref()).await?;
                let expected = destination_expectation(destination.as_ref())?;
                let rooted = self.metadata_authority().await?.clone();
                let relative = destination_path.clone();
                let target = target.to_path_buf();
                tokio::task::spawn_blocking(move || {
                    rooted.verify_root_path_blocking()?;
                    rooted.replace_symlink_blocking(
                        &relative,
                        &target,
                        rooted_destination_expectation(expected),
                        modified,
                    )?;
                    crate::endpoint::local::verify_committed_root(
                        &rooted,
                        &rooted.root_path().join(relative.as_path()),
                    )?;
                    Ok::<_, RemotePullError>(())
                })
                .await
                .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Symlink,
                    &destination_path,
                );
                Ok(crate::engine::work::WorkResult::Metadata)
            }
            RemotePullAction::ApplyMetadata {
                source,
                destination,
                unix_mode,
                modified,
            } => {
                let expected_destination = destination.identity.ok_or_else(|| {
                    RemotePullError::MissingDestinationIdentity(
                        destination.path.as_path().to_path_buf(),
                    )
                })?;
                self.remote.validate_observation(&source).await?;
                let xattrs = self.read_source_xattrs(&source).await?;
                let acls = self.read_source_acls(&source).await?;
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                self.remote.validate_observation(&source).await?;
                let rooted = self.metadata_authority().await?.clone();
                let relative = destination.path;
                let kind = destination.kind;
                tokio::task::spawn_blocking(move || {
                    rooted.verify_root_path_blocking()?;
                    rooted.apply_observed_preservation_blocking(
                        &relative,
                        kind,
                        expected_destination,
                        unix_mode,
                        modified,
                        &crate::rooted_fs::MetadataPreservation {
                            xattrs: xattrs.as_deref(),
                            acl: acls.as_deref(),
                            bsd_flags,
                        },
                    )?;
                    crate::endpoint::local::verify_committed_root(
                        &rooted,
                        &rooted.root_path().join(relative.as_path()),
                    )?;
                    Ok::<_, RemotePullError>(())
                })
                .await
                .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
                // A failure here is post-mutation, not rollback. It cannot
                // become a successful completion of stale preservation work.
                self.remote.validate_observation(&source).await?;
                Ok(crate::engine::work::WorkResult::Metadata)
            }
        }
    }

    /// Fetch one source file into endpoint staging and commit atomically.
    ///
    /// The staged bytes are BLAKE3-verified and preservation is applied before
    /// commit. The server receives Ack only after the local staged writer
    /// publishes; any earlier failure sends Cancel and leaves it waiting no longer.
    async fn fetch_into_staging(
        &self,
        source: &Entry,
        destination_path: &RelativePath,
        expected_destination: ExpectedDestination,
        destination: Option<&Entry>,
        metadata: &PullTransferMetadata,
        final_flags: Option<u32>,
    ) -> Result<(
        crate::engine::work::TransferSummary,
        crate::rooted_fs::PublishedFileProof,
    )> {
        let dest = self.dest_path(destination_path);
        let staged_metadata = staged_file_metadata(metadata, &dest)?;
        if let Some(old) = destination.filter(|old| {
            crate::remote::delta_pull::candidate(
                old,
                crate::remote::push::DEFAULT_REMOTE_DELTA_MIN_SIZE,
                self.remote.capabilities(),
            )
        }) {
            return Ok(crate::remote::delta_pull::fetch_delta(
                &self.sender,
                source,
                self.remote.peer_platform(),
                crate::remote::delta_pull::PullTarget {
                    endpoint: LocalEndpoint::new(self.destination_root.clone())
                        .with_rooted_authority(self.metadata_authority().await?.clone())
                        .with_publication_admission(self.sender.publication_admission()),
                    path: destination_path.clone(),
                    old: old.clone(),
                    unix_mode: staged_metadata
                        .unix_mode
                        .ok_or_else(|| RemotePullError::MissingScannedMode(dest.clone()))?,
                    modified: metadata.modified,
                    final_flags,
                },
                FetchPolicy {
                    compression: self.compression,
                    rate_limiter: self.rate_limiter.as_ref(),
                    preservation: FetchPreservationRequest {
                        xattrs: self.xattrs,
                        acls: self.acls,
                    },
                    capabilities: self.remote.capabilities(),
                },
            )
            .await?);
        }
        let endpoint = LocalEndpoint::new(self.destination_root.clone())
            .with_rooted_authority(self.metadata_authority().await?.clone())
            .with_publication_admission(self.sender.publication_admission());
        let mut staged = endpoint
            .begin_write(destination_path.as_path(), expected_destination)
            .await?;
        let fetched = fetch_file(
            &self.sender,
            source,
            self.remote.peer_platform(),
            staged.as_mut(),
            FetchPolicy {
                compression: self.compression,
                rate_limiter: self.rate_limiter.as_ref(),
                preservation: FetchPreservationRequest {
                    xattrs: self.xattrs,
                    acls: self.acls,
                },
                capabilities: self.remote.capabilities(),
            },
        )
        .await?;
        // The common endpoint finalizer independently hashes the local staged
        // bytes against the verified fetch digest before metadata and commit.
        let verification = crate::endpoint::io::finalize_staged_writer(
            staged,
            &staged_metadata,
            &fetched.preservation,
            Some(blake3::Hash::from_bytes(fetched.summary.digest)),
            None,
            final_flags,
        )
        .await;
        let verification = match verification {
            Ok(verification) => verification,
            Err(error) => {
                self.cancel_staged_fetch(fetched.stream_id).await;
                return Err(error.into());
            }
        };
        let publication = match verification {
            crate::endpoint::io::FinalizationOutcome::Published {
                verification: VerificationStatus::Verified,
                proof,
            } => proof,
            crate::endpoint::io::FinalizationOutcome::VerificationFailed { expected, actual } => {
                self.cancel_staged_fetch(fetched.stream_id).await;
                return Err(RemotePullError::StagedVerificationFailed {
                    path: dest,
                    expected: expected.to_hex().to_string(),
                    actual: actual.to_hex().to_string(),
                });
            }
            crate::endpoint::io::FinalizationOutcome::Published { .. } => {
                self.cancel_staged_fetch(fetched.stream_id).await;
                return Err(RemotePullError::Endpoint(crate::error::SyncError::Config(
                    "pull finalized without verifying staged bytes".into(),
                )));
            }
        };
        crate::remote::fetch::acknowledge_fetch(&self.sender, fetched.stream_id)
            .await
            .map_err(|error| RemotePullError::CommittedAckFailed {
                path: destination_path.as_path().to_path_buf(),
                reason: error.to_string(),
            })?;
        Ok((fetched.summary, publication))
    }

    async fn cancel_staged_fetch(&self, stream_id: crate::protocol::StreamId) {
        if let Err(error) = crate::remote::fetch::cancel_fetch(&self.sender, stream_id).await {
            tracing::warn!(%error, stream_id = stream_id.get(), "failed to cancel an aborted staged fetch");
        }
    }

    fn validate_file_fetch_options(&self) -> Result<()> {
        let capabilities = self.remote.capabilities();
        super::preservation::PreservationRequest {
            xattrs: self.xattrs,
            acls: self.acls,
            flags: self.bsd_flags,
            ..Default::default()
        }
        .validate(capabilities)?;
        if self.compression.is_some() && !capabilities.zstd {
            return Err(RemotePullError::Remote(RemoteSessionError::PeerLacksZstd));
        }
        Ok(())
    }

    /// Preserve only scanned regular files, without changing the visible
    /// destination or its identity before the replacement transaction.
    async fn backup_replacement(&self, destination: Option<&Entry>) -> Result<()> {
        if let Some(existing) = destination.filter(|entry| entry.is_file() && self.backup_enabled())
        {
            let expected = existing.identity.ok_or_else(|| {
                RemotePullError::MissingDestinationIdentity(existing.path.as_path().to_path_buf())
            })?;
            self.backup_observed_file(&existing.path, expected).await?;
        }
        Ok(())
    }

    async fn backup_observed_file(
        &self,
        relative: &RelativePath,
        expected: crate::engine::domain::EntryIdentity,
    ) -> Result<()> {
        let backup_abs = self.backup_destination_for(relative)?;
        let rooted = self.metadata_authority().await?.clone();
        let relative = relative.clone();
        let backup_root = self.backup_root.clone();
        tokio::task::spawn_blocking(move || {
            rooted.verify_root_path_blocking()?;
            rooted.backup_file_with_root_blocking(
                &relative,
                &backup_abs,
                expected,
                backup_root.as_ref(),
            )?;
            crate::endpoint::local::verify_committed_root(&rooted, &backup_abs)?;
            Ok::<_, RemotePullError>(())
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        Ok(())
    }

    /// --backup is on when a suffix policy exists. The suffix defaults to
    /// `~`; an empty string never disables --backup (config sets a marker
    /// only when --backup was passed).
    fn backup_enabled(&self) -> bool {
        self.backup
    }

    /// Read the remote source's extended attributes for one entry when `-X`
    /// requested them. Symlinks cannot be read without resolving their target,
    /// so they are skipped.
    async fn read_source_xattrs(&self, source: &Entry) -> Result<Option<Vec<(OsString, Vec<u8>)>>> {
        if !self.xattrs || source.is_symlink() {
            return Ok(None);
        }
        let location = XattrLocation::Remote(&self.remote);
        let xattrs = read_preserved_xattrs(&location, source).await?;
        Ok(Some(xattrs))
    }

    /// Read the remote source's access-control list for one entry when `-A`
    /// requested it. Symlinks are skipped, like xattrs.
    async fn read_source_acls(&self, source: &Entry) -> Result<Option<String>> {
        if !self.acls || source.is_symlink() {
            return Ok(None);
        }
        let location = AclLocation::Remote(&self.remote);
        let acl = read_preserved_acls(&location, source).await?;
        Ok(Some(acl.unwrap_or_default()))
    }

    /// Read the remote source's BSD file flags for one entry when `-F`
    /// requested them (macOS only; elsewhere `-F` is refused up front).
    /// Symlinks are skipped, like xattrs.
    async fn read_source_bsd_flags(&self, source: &Entry) -> Result<Option<u32>> {
        if !self.bsd_flags || source.is_symlink() {
            return Ok(None);
        }
        let location = BsdFlagsLocation::Remote(&self.remote);
        let flags = read_preserved_bsd_flags(&location, source).await?;
        Ok(Some(flags))
    }

    /// Backup location for one root-relative destination path, mirroring the
    /// local engine: beside the file (name + suffix) or under --backup-dir
    /// preserving the tree shape (GNU rsync semantics). The returned path
    /// is always absolute beneath the destination (or under the absolute
    /// --backup-dir), so callers can never write relative to the process
    /// working directory.
    fn backup_destination_for(&self, relative: &RelativePath) -> Result<PathBuf> {
        super::local_executor::backup_destination_path(
            &self.destination_root,
            relative,
            self.backup_dir.as_deref(),
            &self.backup_suffix,
        )
        .map_err(|_| RemotePullError::InvalidBackupPath(relative.as_path().to_path_buf()))
    }

    /// Delete one destination-only entry after the delete-threshold gate.
    /// Local deletes follow the same protection rules as the push side:
    /// `--backup` copies regular files before removal, symlinks remove
    /// without a backup so a target is never resolved.
    pub async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                network_writes: 0,
                ..ResourceRequest::default()
            })
            .await?;
        let path = self.dest_path(&action.path);
        if self.backup_enabled() && action.kind == EntryKind::File {
            let expected = action
                .identity
                .ok_or_else(|| RemotePullError::MissingDestinationIdentity(path.clone()))?;
            self.backup_observed_file(&action.path, expected).await?;
        }
        let rooted = self.metadata_authority().await?.clone();
        let relative = action.path.clone();
        tokio::task::spawn_blocking(move || {
            rooted.verify_root_path_blocking()?;
            rooted
                .remove_destination_blocking(
                    &relative,
                    action.kind == EntryKind::Directory,
                    action.identity,
                )?
                .verify_local_root(&rooted, &relative)?;
            Ok::<_, RemotePullError>(())
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        self.report(
            crate::sync::output::ItemizeOp::Delete,
            if action.kind == EntryKind::Directory {
                crate::sync::output::ItemizeKind::Directory
            } else {
                crate::sync::output::ItemizeKind::File
            },
            &action.path,
        );
        Ok(())
    }

    fn lower_unchanged_file_preservation(
        &self,
        source: Entry,
        destination: Entry,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> Result<Option<WorkItem<RemotePullAction>>> {
        if !source.is_file() || !(self.xattrs || self.acls || self.bsd_flags) {
            return Ok(None);
        }
        if destination.identity.is_none() {
            return Err(RemotePullError::MissingDestinationIdentity(
                destination.path.as_path().to_path_buf(),
            ));
        }
        let (unix_mode, modified) =
            crate::remote::pull_lower::requested_metadata(&source, &destination, policy)?
                .unwrap_or((None, None));
        Ok(Some(WorkItem::new(
            RemotePullAction::ApplyMetadata {
                source,
                destination,
                unix_mode,
                modified,
            },
            ResourceRequest {
                active_files: 0,
                buffered_bytes: 4 * crate::protocol::MAX_FRAME_PAYLOAD as u64,
                metadata_ops: 1,
                cpu_tasks: 1,
                network_writes: 1,
            },
        )))
    }

    /// Replay reverse-order finalize metadata (directory modes/mtimes,
    /// child-before-parent).
    pub async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                network_writes: 0,
                ..ResourceRequest::default()
            })
            .await?;
        let rooted = self.metadata_authority().await?.clone();
        let crate::engine::finalize_journal::DirectoryTarget::Observed(expected) = metadata.target
        else {
            return Err(crate::rooted_fs::RootedFsError::DestinationChanged(
                metadata.path.as_path().to_path_buf(),
            )
            .into());
        };
        let request = crate::rooted_fs::DirectoryPreservationRequest {
            xattrs: self.xattrs,
            acl: self.acls,
            bsd_flags: self.bsd_flags,
        };
        let preservation = if metadata.preserve_source {
            self.remote
                .read_directory_preservation(&metadata.path, metadata.source_identity, request)
                .await?
        } else {
            Default::default()
        };
        tokio::task::spawn_blocking(move || {
            rooted.verify_root_path_blocking()?;
            rooted.finalize_directory_blocking(
                &metadata.path,
                expected,
                metadata.unix_mode,
                metadata.modified,
                &preservation,
            )?;
            crate::endpoint::local::verify_committed_root(
                &rooted,
                &rooted.root_path().join(metadata.path.as_path()),
            )?;
            Ok::<_, RemotePullError>(())
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        Ok(())
    }
}

#[cfg(unix)]
fn staged_file_metadata(
    metadata: &PullTransferMetadata,
    destination: &Path,
) -> Result<crate::endpoint::io::StagedMetadata> {
    let mode = metadata
        .unix_mode
        .ok_or_else(|| RemotePullError::MissingScannedMode(destination.to_path_buf()))?;
    // Without --times leave the staged file's native write time untouched;
    // do not replace it with a clock sample taken before fetching bytes.
    let modified = metadata
        .modified
        .map(|value| {
            crate::endpoint::transfer::timestamp_to_system_time(value).ok_or_else(|| {
                RemotePullError::LocalMutation(
                    destination.to_path_buf(),
                    std::io::Error::other("requested timestamp outside supported range"),
                )
            })
        })
        .transpose()?;
    Ok(crate::endpoint::io::StagedMetadata {
        modified,
        unix_mode: Some(mode),
    })
}

#[cfg(not(unix))]
fn staged_file_metadata(
    _metadata: &PullTransferMetadata,
    destination: &Path,
) -> Result<crate::endpoint::io::StagedMetadata> {
    Err(RemotePullError::LocalMutation(
        destination.to_path_buf(),
        std::io::Error::other("staged metadata is unix-only in the v3 pull"),
    ))
}

impl crate::engine::controller::SyncPlanExecutor for RemotePullExecutor {
    async fn prepare_destination_for_create(&self, create: &RelativePath) -> Result<()> {
        self.acquire_destination_for_create(create).await
    }

    fn close_mutation_admission(&self) {
        self.run_admission.close();
    }

    fn cancel_mutations(&self) {
        self.close_mutation_admission();
        self.sender
            .fail(Arc::new(crate::remote::router::RouterError::ShuttingDown));
    }

    type Action = RemotePullAction;
    type Error = RemotePullError;

    fn lower(
        &self,
        op: crate::engine::domain::SyncOp,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<Option<WorkItem<RemotePullAction>>, RemotePullError> {
        if op.source().is_directory() && op.path() != &op.source().path {
            return Err(RemotePullError::DirectoryAddressUnsupported(
                op.path().clone(),
            ));
        }
        if let crate::engine::domain::SyncOp::Unchanged {
            source,
            destination,
            ..
        } = op
        {
            return self.lower_unchanged_file_preservation(source, destination, policy);
        }
        crate::remote::pull_lower::lower_pull_op(op, policy)
    }

    fn supports_whole_hardlink_groups(&self) -> bool {
        self.hardlinks
    }

    async fn execute_hardlink_group(
        &self,
        group: &mut crate::engine::hardlink_runtime::HardlinkGroupPlan<'_>,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> Result<Option<()>> {
        self.execute_whole_group(group, policy).await?;
        Ok(Some(()))
    }

    fn is_directory_action(&self, action: &RemotePullAction) -> bool {
        matches!(action, RemotePullAction::CreateDirectory { .. })
    }

    async fn execute(
        &self,
        item: WorkItem<RemotePullAction>,
    ) -> std::result::Result<crate::engine::work::WorkResult, RemotePullError> {
        RemotePullExecutor::execute(self, item).await
    }

    async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> std::result::Result<(), RemotePullError> {
        RemotePullExecutor::execute_delete(self, action).await
    }

    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> std::result::Result<(), RemotePullError> {
        RemotePullExecutor::execute_finalize(self, metadata).await
    }

    async fn finish_deferred_source_removals(&self) -> std::result::Result<(), RemotePullError> {
        // A no-op plan still has to complete against the original local
        // operator address. Do not acquire/create an unneeded pending root.
        if self.metadata_authority.get().is_some() {
            self.metadata_authority().await?;
        }
        Ok(())
    }

    async fn close_source_removals(&self) -> std::result::Result<u64, RemotePullError> {
        // This protocol direction does not admit server SOURCE removal.
        Ok(0)
    }

    /// Pulls have no local source to remove; parity skips are just skips.
    async fn remove_unchanged_source(
        &self,
        _source: &Entry,
        _destination: &Entry,
        _policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<(), RemotePullError> {
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "pull_mutation_tests.rs"]
mod mutation_tests;

#[cfg(all(test, unix))]
#[path = "pull_root_tests.rs"]
mod root_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn pull_symlink_executor_uses_rooted_empty_directory_exchange() {
        use crate::engine::domain::SyncOp;
        use crate::engine::planner::ExecutionPolicy;
        use crate::engine::scheduler::ResourceBudget;
        use crate::protocol::Operation;
        use crate::remote::router::RouterConfig;
        use crate::remote::runtime::{ClientRemoteSession, ServerRemoteSession};
        use std::os::unix::fs::MetadataExt;

        let source_root = tempfile::TempDir::new().unwrap();
        let dest_root = tempfile::TempDir::new().unwrap();
        let dest = dest_root.path().join("link");
        std::fs::create_dir(&dest).unwrap();
        let mut destination =
            Entry::directory(RelativePath::new("link").unwrap(), Timestamp::UNIX_EPOCH);
        destination.identity = crate::endpoint::local_identity::metadata_identity(
            &std::fs::symlink_metadata(&dest).unwrap(),
            EntryKind::Directory,
        );
        let modified = Timestamp::new(1_600_000_000, 123_456_789).unwrap();
        let source = Entry::symlink(
            RelativePath::new("link").unwrap(),
            PathBuf::from("../target"),
            modified,
        );
        let work = crate::remote::pull_lower::lower_pull_op(
            SyncOp::Replace {
                source,
                destination,
            },
            ExecutionPolicy {
                preserve_times: true,
                ..ExecutionPolicy::default()
            },
        )
        .unwrap()
        .unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let (client, server) = tokio::join!(
            ClientRemoteSession::connect(
                client_reader,
                client_writer,
                Operation::Pull,
                source_root.path(),
                RouterConfig::default()
            ),
            ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default()),
        );
        let client = client.unwrap();
        let _server = server.unwrap();
        let executor = RemotePullExecutor::new(
            dest_root.path().to_path_buf(),
            client.request_handle(),
            client.sender(),
            Scheduler::new(ResourceBudget::default()).unwrap(),
        );
        executor.execute(work).await.unwrap();
        assert_eq!(std::fs::read_link(&dest).unwrap(), Path::new("../target"));
        let metadata = std::fs::symlink_metadata(&dest).unwrap();
        assert_eq!(metadata.mtime(), modified.seconds());
        assert_eq!(metadata.mtime_nsec(), i64::from(modified.nanoseconds()));
        assert_eq!(std::fs::read_dir(dest_root.path()).unwrap().count(), 1);
    }
}
