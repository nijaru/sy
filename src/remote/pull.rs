//! v3 pull execution: remote source, local destination.
//!
//! The pull inverts the push executor's role split while keeping the same
//! safety ordering. The remote is a pure SOURCE: it serves scans, content
//! hashes, and whole-file fetches, and the session router already rejects
//! mutation requests in Pull sessions. The client owns reconciliation,
//! the bounded delete journal, local transactional staging, and metadata.
//!
//! Byte strategy for this slice is whole-file fetch with per-chunk
//! compression when negotiated. Delta pull (uploading local destination
//! signatures, receiving copy ops) is a follow-up: the wire vocabulary
//! supports it, but the safety-critical ordering — complete preflight, then
//! main work, then reverse-order deletes, then finalize — must land first.

use crate::endpoint::io::{ExpectedDestination, VerificationStatus};
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::{Endpoint, FileMetadata};
use crate::engine::compression::CompressionPolicy;
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::engine::hardlink_groups::{HardlinkGroups, HardlinkRepresentative};
use crate::engine::scheduler::{ResourceRequest, Scheduler};
use crate::engine::work::WorkItem;
use crate::remote::acl::{apply_preserved_acls, read_preserved_acls, AclLocation, RemoteAclError};
use crate::remote::bsdflags::{
    apply_preserved_bsd_flags, read_preserved_bsd_flags, BsdFlagsLocation, RemoteBsdFlagsError,
};
use crate::remote::fetch::{fetch_file, FetchPolicy, FetchPreservationRequest};
use crate::remote::router::RouterSender;
use crate::remote::runtime::{ClientRemoteHandle, RemoteSessionError};
use crate::remote::xattr::{
    apply_preserved_xattrs, read_preserved_xattrs, RemoteXattrError, XattrLocation,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bounded working set per in-flight fetch; mirrors the push side so the
/// scheduler byte budget is direction-symmetric.
pub const REMOTE_FETCH_WORKING_SET: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RemotePullError {
    #[error("selected-leaf destination binding is not supported over SSH")]
    DestinationAddressUnsupported,
    #[error("hardlink bookkeeping failed: {0}")]
    HardlinkState(#[from] std::io::Error),
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
    DeleteBackup(#[from] crate::rooted_fs::RootedFsError),

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
    },
    FetchFile {
        source: Entry,
        destination: Option<Entry>,
        metadata: PullTransferMetadata,
    },
    ReplaceSymlink {
        source: Entry,
        destination: Option<Entry>,
        modified: Option<Timestamp>,
    },
    ApplyMetadata {
        source: Entry,
        expected_destination: EntryIdentity,
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
    metadata_authority: tokio::sync::OnceCell<crate::rooted_fs::RootedFs>,
    remote: ClientRemoteHandle,
    sender: RouterSender,
    scheduler: Scheduler,
    /// --backup: enabled flag, the destination-anchored backup directory
    /// (None = beside-the-file suffix backups), and the suffix.
    backup: bool,
    backup_dir: Option<std::path::PathBuf>,
    backup_suffix: String,
    reporter: Option<Arc<crate::sync::output::SyncReporter>>,
    rate_limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// -z/--compress: per-chunk zstd for fetches. The negotiated ZSTD
    /// capability is checked before the first request, mirroring the push
    /// side's transfer_file_with_policy contract.
    compression: Option<CompressionPolicy>,
    /// -H/--preserve-hardlinks: group -> first committed destination path.
    /// The mutex is held across a grouped fetch so members serialize
    /// (ungrouped files stay concurrent), mirroring the push executor.
    hardlinks: bool,
    hardlink_groups: tokio::sync::Mutex<HardlinkGroups>,
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
            remote,
            sender,
            scheduler,
            backup: false,
            backup_dir: None,
            backup_suffix: String::new(),
            reporter: None,
            rate_limiter: None,
            compression: None,
            hardlinks: false,
            hardlink_groups: tokio::sync::Mutex::new(HardlinkGroups::default()),
            xattrs: false,
            acls: false,
            bsd_flags: false,
        }
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
        self.metadata_authority
            .get_or_try_init(|| crate::rooted_fs::RootedFs::open(self.destination_root.clone()))
            .await
            .map_err(Into::into)
    }

    fn dest_path(&self, relative: &RelativePath) -> PathBuf {
        self.destination_root.join(relative.as_path())
    }

    pub async fn execute(
        &self,
        item: WorkItem<RemotePullAction>,
    ) -> Result<crate::engine::work::WorkResult> {
        let (action, resources) = item.into_parts();
        let _permit = self.scheduler.acquire(resources).await?;

        match action {
            RemotePullAction::CreateDirectory { source } => {
                let expected = source.identity.ok_or_else(|| {
                    crate::rooted_fs::RootedFsError::DestinationChanged(
                        source.path.as_path().to_path_buf(),
                    )
                })?;
                self.remote
                    .read_directory_preservation(&source.path, expected, Default::default())
                    .await?;
                let rooted = self.metadata_authority().await?.clone();
                let relative = source.path.clone();
                let identity = tokio::task::spawn_blocking(move || {
                    rooted.create_directory_blocking(&relative)
                })
                .await
                .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Directory,
                    &source.path,
                );
                Ok(crate::engine::work::WorkResult::DirectoryPrepared(identity))
            }
            RemotePullAction::FetchFile {
                source,
                destination,
                metadata,
            } => {
                // -H: members after the first link to the representative
                // instead of fetching the same bytes again.
                if self.hardlinks {
                    if let Some(identity) = source.hardlink_group {
                        let group = *identity.as_bytes();
                        return self
                            .execute_grouped_fetch(source, destination, metadata, group)
                            .await
                            .map(crate::engine::work::WorkResult::Transfer);
                    }
                }
                let is_update = destination.is_some();
                let expected_destination = destination_expectation(destination.as_ref())?;
                self.validate_file_fetch_options()?;
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                // --backup preserves the replaced destination file first, as a
                // local copy (the destination is local in a pull). A backup
                // failure aborts the fetch so the user's copy cannot be
                // silently skipped.
                self.backup_replacement(destination.as_ref()).await?;
                let summary = self
                    .fetch_into_staging(&source, expected_destination, &metadata, bsd_flags)
                    .await?;
                let op = if is_update {
                    crate::sync::output::ItemizeOp::Update
                } else {
                    crate::sync::output::ItemizeOp::Create
                };
                self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
                Ok(crate::engine::work::WorkResult::Transfer(summary))
            }
            RemotePullAction::ReplaceSymlink {
                source,
                destination,
                modified,
            } => {
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    RemotePullError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
                let expected = destination_expectation(destination.as_ref())?;
                LocalEndpoint::new(self.destination_root.clone())
                    .replace_symlink(target, source.path.as_path(), expected, modified)
                    .await?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Symlink,
                    &source.path,
                );
                Ok(crate::engine::work::WorkResult::Metadata)
            }
            RemotePullAction::ApplyMetadata {
                source,
                expected_destination,
                unix_mode,
                modified,
            } => {
                let xattrs = self.read_source_xattrs(&source).await?;
                let acls = self.read_source_acls(&source).await?;
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                let rooted = self.metadata_authority().await?.clone();
                let relative = source.path.clone();
                let kind = source.kind;
                tokio::task::spawn_blocking(move || {
                    rooted.apply_metadata_blocking(
                        &relative,
                        kind,
                        expected_destination,
                        unix_mode,
                        modified,
                    )
                })
                .await
                .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
                if let Some(xattrs) = xattrs.as_deref() {
                    self.write_destination_xattrs(&source.path, source.kind, xattrs)
                        .await?;
                }
                if let Some(acls) = acls.as_deref() {
                    self.write_destination_acls(&source.path, source.kind, acls)
                        .await?;
                }
                if let Some(flags) = bsd_flags {
                    self.write_destination_bsd_flags(&source.path, source.kind, flags)
                        .await?;
                }
                Ok(crate::engine::work::WorkResult::Metadata)
            }
        }
    }

    /// One grouped fetch under `-H`: fetch the representative or link
    /// subsequent members to it locally. Group membership is scan-time, so
    /// linking members validate their own scanned identity through a rooted
    /// fingerprint request before publication. The group mutex serializes members.
    async fn execute_grouped_fetch(
        &self,
        source: Entry,
        destination: Option<Entry>,
        metadata: PullTransferMetadata,
        group: [u8; 32],
    ) -> Result<crate::engine::work::TransferSummary> {
        let expected_destination = destination_expectation(destination.as_ref())?;
        let groups = self.hardlink_groups.lock().await;
        if let Some(first) = groups.get(group).await? {
            first.validate_metadata(metadata.unix_mode, metadata.modified)?;
            // Bind the linking member to its own scanned observation. A group
            // key alone says which inode was scanned, not that it is unchanged.
            self.remote
                .existing_fingerprint(
                    &source,
                    crate::endpoint::existing::FingerprintOptions::default(),
                )
                .await
                .map_err(RemoteSessionError::from)?;
            let is_update = destination.is_some();

            self.backup_replacement(destination.as_ref()).await?;

            let first_abs = self.dest_path(&first.path);

            let dest_abs = self.dest_path(&source.path);
            link_local_file(&first_abs, &dest_abs, expected_destination).await?;
            let op = if is_update {
                crate::sync::output::ItemizeOp::Update
            } else {
                crate::sync::output::ItemizeOp::Create
            };
            self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
            return Ok(crate::engine::work::TransferSummary {
                file_size: source.size,
                digest: [0_u8; 32],
                literal_bytes: 0,
                reused_bytes: source.size,
            });
        }
        let is_update = destination.is_some();
        self.validate_file_fetch_options()?;
        let bsd_flags = self.read_source_bsd_flags(&source).await?;
        self.backup_replacement(destination.as_ref()).await?;
        let summary = self
            .fetch_into_staging(&source, expected_destination, &metadata, bsd_flags)
            .await?;
        groups
            .insert(
                group,
                HardlinkRepresentative {
                    path: source.path.clone(),
                    unix_mode: metadata.unix_mode,
                    modified: metadata.modified,
                },
            )
            .await?;
        drop(groups);
        let op = if is_update {
            crate::sync::output::ItemizeOp::Update
        } else {
            crate::sync::output::ItemizeOp::Create
        };
        self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
        Ok(summary)
    }

    /// Fetch one source file into endpoint staging and commit atomically.
    ///
    /// The staged bytes are BLAKE3-verified and preservation is applied before
    /// commit. The server receives Ack only after the local staged writer
    /// publishes; any earlier failure sends Cancel and leaves it waiting no longer.
    async fn fetch_into_staging(
        &self,
        source: &Entry,
        expected_destination: ExpectedDestination,
        metadata: &PullTransferMetadata,
        final_flags: Option<u32>,
    ) -> Result<crate::engine::work::TransferSummary> {
        let dest = self.dest_path(&source.path);
        let staged_metadata = staged_file_metadata(source, metadata, &dest)?;
        let endpoint = LocalEndpoint::new(self.destination_root.clone())
            .with_publication_admission(self.sender.publication_admission());
        let mut staged = endpoint
            .begin_write(source.path.as_path(), expected_destination)
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
                protocol_version: self.remote.protocol_version(),
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
        match verification {
            crate::endpoint::io::FinalizationOutcome::Published {
                verification: VerificationStatus::Verified,
                ..
            } => {}
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
        }
        crate::remote::fetch::acknowledge_fetch(&self.sender, fetched.stream_id)
            .await
            .map_err(|error| RemotePullError::CommittedAckFailed {
                path: source.path.as_path().to_path_buf(),
                reason: error.to_string(),
            })?;
        Ok(fetched.summary)
    }

    async fn cancel_staged_fetch(&self, stream_id: crate::protocol::StreamId) {
        if let Err(error) = crate::remote::fetch::cancel_fetch(&self.sender, stream_id).await {
            tracing::warn!(%error, stream_id = stream_id.get(), "failed to cancel an aborted staged fetch");
        }
    }

    fn validate_file_fetch_options(&self) -> Result<()> {
        let capabilities = self.remote.capabilities();
        for (requested, supported, feature) in [
            (self.xattrs, capabilities.preserve_xattrs, "xattrs"),
            (self.acls, capabilities.preserve_acls, "ACLs"),
        ] {
            if requested
                && (!supported || self.remote.protocol_version() < crate::protocol::PROTOCOL_V3_2)
            {
                return Err(
                    crate::remote::transfer::RemoteTransferError::PreservationUnavailable {
                        feature,
                    }
                    .into(),
                );
            }
        }
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
            let backup_abs = self.backup_destination_for(&existing.path)?;
            copy_local_backup(
                &self.destination_root,
                &existing.path,
                &backup_abs,
                expected,
            )
            .await?;
        }
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

    /// Mirror an already-read attribute set onto the local destination.
    async fn write_destination_xattrs(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        xattrs: &[(OsString, Vec<u8>)],
    ) -> Result<()> {
        let location = XattrLocation::Local(self.destination_root.as_path());
        apply_preserved_xattrs(&location, path, kind, xattrs).await?;
        Ok(())
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

    /// Mirror an already-read access-control list onto the local
    /// destination.
    async fn write_destination_acls(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        acl: &str,
    ) -> Result<()> {
        let location = AclLocation::Local(self.destination_root.as_path());
        apply_preserved_acls(&location, path, kind, acl).await?;
        Ok(())
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

    /// Mirror already-read BSD file flags onto the local destination.
    async fn write_destination_bsd_flags(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        flags: u32,
    ) -> Result<()> {
        let location = BsdFlagsLocation::Local(self.destination_root.as_path());
        apply_preserved_bsd_flags(&location, path, kind, flags).await?;
        Ok(())
    }

    /// Backup location for one root-relative destination path, mirroring the
    /// local engine: beside the file (name + suffix) or under --backup-dir
    /// preserving the tree shape (GNU rsync semantics). The returned path
    /// is always absolute beneath the destination (or under the absolute
    /// --backup-dir), so callers can never write relative to the process
    /// working directory.
    fn backup_destination_for(&self, relative: &RelativePath) -> Result<PathBuf> {
        let mut backup_name = relative
            .as_path()
            .file_name()
            .ok_or_else(|| RemotePullError::InvalidBackupPath(relative.as_path().to_path_buf()))?
            .to_os_string();
        backup_name.push(&self.backup_suffix);
        match &self.backup_dir {
            Some(dir) => {
                let mut backup = dir.join(relative.as_path());
                backup.set_file_name(backup_name);
                Ok(backup)
            }
            None => {
                let mut backup = self.destination_root.join(relative.as_path());
                backup.set_file_name(backup_name);
                Ok(backup)
            }
        }
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
            let backup_abs = self.backup_destination_for(&action.path)?;
            let rooted = crate::rooted_fs::RootedFs::open(self.destination_root.clone()).await?;
            let relative = action.path.clone();
            tokio::task::spawn_blocking(move || {
                rooted.backup_file_blocking(&relative, &backup_abs, expected)
            })
            .await
            .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        }
        remove_local_entry(&path, action.kind == EntryKind::Directory, action.identity)
            .await
            .map_err(|error| RemotePullError::LocalMutation(path.clone(), error))?;
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
        if self.xattrs || self.acls {
            let mode = if policy.preserve_permissions {
                source.unix_mode
            } else {
                destination.unix_mode
            }
            .ok_or_else(|| {
                RemotePullError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let modified = Some(if policy.preserve_times {
                source.modified
            } else {
                destination.modified
            });
            return Ok(Some(WorkItem::new(
                RemotePullAction::FetchFile {
                    source,
                    destination: Some(destination),
                    metadata: PullTransferMetadata {
                        unix_mode: Some(mode),
                        modified,
                    },
                },
                ResourceRequest {
                    active_files: 1,
                    buffered_bytes: REMOTE_FETCH_WORKING_SET,
                    metadata_ops: 0,
                    cpu_tasks: 1,
                    network_writes: 1,
                },
            )));
        }
        let expected_destination = destination.identity.ok_or_else(|| {
            RemotePullError::MissingDestinationIdentity(destination.path.as_path().to_path_buf())
        })?;
        Ok(Some(WorkItem::new(
            RemotePullAction::ApplyMetadata {
                source,
                expected_destination,
                unix_mode: None,
                modified: None,
            },
            ResourceRequest {
                active_files: 0,
                buffered_bytes: 0,
                metadata_ops: 1,
                cpu_tasks: 0,
                network_writes: 0,
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
            rooted.finalize_directory_blocking(
                &metadata.path,
                expected,
                metadata.unix_mode,
                metadata.modified,
                &preservation,
            )
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        Ok(())
    }
}

/// Privately copy the scanned regular destination before replacement. The
/// rooted helper binds the no-follow source handle to the scanned identity
/// and validates it again before publishing the backup. Never move the
/// visible original: fetch/preservation may still fail before commit.
async fn copy_local_backup(
    destination_root: &Path,
    relative: &RelativePath,
    backup_abs: &Path,
    expected: EntryIdentity,
) -> crate::rooted_fs::Result<()> {
    let rooted = crate::rooted_fs::RootedFs::open(destination_root.to_path_buf()).await?;
    let relative = relative.clone();
    let backup_abs = backup_abs.to_path_buf();
    tokio::task::spawn_blocking(move || {
        rooted.backup_file_blocking(&relative, &backup_abs, expected)
    })
    .await
    .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))?
}

/// Stage a hardlink beside the destination and replace it only if the scanned
/// destination state still holds. The final stat/rename pair is not an atomic
/// compare-and-swap against arbitrary concurrent writers.
async fn link_local_file(
    first: &Path,
    dest: &Path,
    expected_destination: ExpectedDestination,
) -> Result<()> {
    #[cfg(unix)]
    {
        let expected_destination =
            crate::endpoint::local::capture_destination_expectation(dest, expected_destination)
                .await?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| RemotePullError::LocalMutation(parent.to_path_buf(), error))?;
        }
        let temp = crate::temp_file::TempFileGuard::temp_path_for(dest);
        let guard = crate::temp_file::TempFileGuard::new(&temp);
        tokio::fs::hard_link(first, &temp)
            .await
            .map_err(|error| RemotePullError::LocalMutation(temp.clone(), error))?;
        crate::endpoint::local::verify_destination_expectation(dest, expected_destination).await?;
        tokio::fs::rename(&temp, dest)
            .await
            .map_err(|error| RemotePullError::LocalMutation(dest.to_path_buf(), error))?;
        drop(guard);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (first, expected_destination);
        Err(RemotePullError::LocalMutation(
            dest.to_path_buf(),
            std::io::Error::other("hardlink preservation is not supported on this platform"),
        ))
    }
}

async fn remove_local_entry(
    path: &Path,
    is_directory: bool,
    expected_identity: Option<EntryIdentity>,
) -> std::result::Result<(), std::io::Error> {
    let meta = match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let kind = if meta.file_type().is_symlink() {
        EntryKind::Symlink
    } else if meta.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    if is_directory != (kind == EntryKind::Directory) {
        return Err(std::io::Error::other(
            "destination entry type changed since the scan",
        ));
    }
    if let Some(expected) = expected_identity {
        let current =
            crate::endpoint::local_identity::metadata_identity(&meta, kind).ok_or_else(|| {
                std::io::Error::other("destination entry changed between scan and removal")
            })?;
        if current != expected {
            return Err(std::io::Error::other(
                "destination entry changed between scan and removal",
            ));
        }
    }
    if is_directory {
        match tokio::fs::remove_dir(path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    || error.raw_os_error() == Some(66)
                    || error.raw_os_error() == Some(39) =>
            {
                // Kept non-empty directory (e.g. holds protected descendant or --backup file)
                tracing::debug!(
                    path = %path.display(),
                    "kept non-empty destination directory"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    } else {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
fn staged_file_metadata(
    source: &Entry,
    metadata: &PullTransferMetadata,
    destination: &Path,
) -> Result<FileMetadata> {
    let mode = metadata
        .unix_mode
        .ok_or_else(|| RemotePullError::MissingScannedMode(destination.to_path_buf()))?;
    // Without --times the committed file keeps its natural creation time
    // (rsync semantics: no -t means the destination mtime is "now"); with
    // --times it carries the source's scanned mtime.
    let modified = metadata
        .modified
        .map(system_time_from_timestamp)
        .unwrap_or_else(std::time::SystemTime::now);
    Ok(FileMetadata {
        size: source.size,
        modified,
        is_dir: false,
        is_symlink: false,
        mode,
    })
}

#[cfg(not(unix))]
fn staged_file_metadata(
    _source: &Entry,
    _metadata: &PullTransferMetadata,
    destination: &Path,
) -> Result<FileMetadata> {
    Err(RemotePullError::LocalMutation(
        destination.to_path_buf(),
        std::io::Error::other("staged metadata is unix-only in the v3 pull"),
    ))
}

fn system_time_from_timestamp(timestamp: Timestamp) -> std::time::SystemTime {
    if timestamp.seconds() >= 0 {
        std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::new(timestamp.seconds() as u64, timestamp.nanoseconds())
    } else {
        std::time::SystemTime::UNIX_EPOCH
            - std::time::Duration::new((-(timestamp.seconds() as i128)) as u64, 0)
    }
}

impl crate::engine::controller::SyncPlanExecutor for RemotePullExecutor {
    type Action = RemotePullAction;
    type Error = RemotePullError;

    fn lower(
        &self,
        op: crate::engine::domain::SyncOp,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<Option<WorkItem<RemotePullAction>>, RemotePullError> {
        if op.path() != &op.source().path {
            return Err(RemotePullError::DestinationAddressUnsupported);
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
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn hardlink_pull_revalidates_members_and_keeps_one_metadata_owner() {
        use crate::engine::controller::SyncPlanExecutor;
        use crate::engine::domain::SyncOp;
        use crate::engine::planner::ExecutionPolicy;
        use crate::engine::scheduler::ResourceBudget;
        use crate::protocol::Operation;
        use crate::remote::router::RouterConfig;
        use crate::remote::runtime::ClientRemoteSession;
        use futures::TryStreamExt;
        use std::os::unix::fs::MetadataExt;

        for race in [None, Some("source"), Some("metadata")] {
            let source_root = tempfile::tempdir().unwrap();
            let dest_root = tempfile::tempdir().unwrap();
            std::fs::write(source_root.path().join("a"), b"new").unwrap();
            std::fs::hard_link(source_root.path().join("a"), source_root.path().join("b")).unwrap();
            std::fs::write(dest_root.path().join("b"), b"old").unwrap();
            let mut request = crate::engine::scan::ScanRequest::default();
            request.metadata.hardlink_group = true;
            request.metadata.unix_mode = true;
            let entries: Vec<Entry> = crate::endpoint::local_entry_scan::local_entry_stream(
                source_root.path().to_path_buf(),
                request,
            )
            .try_collect()
            .await
            .unwrap();
            let destination = crate::endpoint::local_entry_scan::local_entry_stream(
                dest_root.path().to_path_buf(),
                request,
            )
            .try_next()
            .await
            .unwrap()
            .unwrap();
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (client_reader, client_writer) = tokio::io::split(client_io);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let server = tokio::spawn(crate::remote::serve::serve_transport(
                server_reader,
                server_writer,
                RouterConfig::default(),
            ));
            let client = ClientRemoteSession::connect(
                client_reader,
                client_writer,
                Operation::Pull,
                source_root.path(),
                RouterConfig::default(),
            )
            .await
            .unwrap();
            let executor = RemotePullExecutor::new(
                dest_root.path().to_path_buf(),
                client.request_handle(),
                client.sender(),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_hardlinks(true);
            let first = crate::remote::pull_lower::lower_pull_op(
                SyncOp::Create {
                    destination_path: entries[0].clone().path.clone(),
                    source: entries[0].clone(),
                },
                ExecutionPolicy::default(),
            )
            .unwrap()
            .unwrap();
            SyncPlanExecutor::execute(&executor, first).await.unwrap();
            let mode = std::fs::metadata(dest_root.path().join("a"))
                .unwrap()
                .mode();
            if race == Some("source") {
                std::fs::write(source_root.path().join("b"), b"bad").unwrap();
            }
            let work = crate::remote::pull_lower::lower_pull_op(
                SyncOp::Update {
                    source: entries[1].clone(),
                    destination,
                },
                ExecutionPolicy::default(),
            )
            .unwrap()
            .unwrap();
            let (mut action, resources) = work.into_parts();
            if race == Some("metadata") {
                let RemotePullAction::FetchFile { metadata, .. } = &mut action else {
                    panic!("expected file fetch")
                };
                metadata.unix_mode = Some(0o600);
                // Ensure a real conflict regardless of the process umask.
                if mode & 0o7777 == 0o600 {
                    metadata.unix_mode = Some(0o644);
                }
            }
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                SyncPlanExecutor::execute(&executor, WorkItem::new(action, resources)),
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::metadata(dest_root.path().join("a"))
                    .unwrap()
                    .mode(),
                mode
            );
            assert_eq!(std::fs::read(dest_root.path().join("a")).unwrap(), b"new");
            if race.is_some() {
                assert!(result.is_err());
                assert_eq!(std::fs::read(dest_root.path().join("b")).unwrap(), b"old");
            } else {
                let crate::engine::work::WorkResult::Transfer(transfer) = result.unwrap() else {
                    panic!("expected transfer receipt")
                };
                assert_eq!(transfer.reused_bytes, 3);
                assert_eq!(
                    std::fs::metadata(dest_root.path().join("a")).unwrap().ino(),
                    std::fs::metadata(dest_root.path().join("b")).unwrap().ino()
                );
            }
            assert_eq!(std::fs::read_dir(dest_root.path()).unwrap().count(), 2);
            drop(executor);
            drop(client);
            let served = tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(served.is_err(), race == Some("source"));
        }
    }

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

    #[tokio::test]
    async fn pull_remove_local_entry_validates_identity() {
        let temp = tempfile::TempDir::new().unwrap();
        let file_path = temp.path().join("file");
        let dir_path = temp.path().join("dir");
        std::fs::write(&file_path, b"delete target").unwrap();
        std::fs::create_dir(&dir_path).unwrap();

        let file_meta = std::fs::symlink_metadata(&file_path).unwrap();
        let file_id =
            crate::endpoint::local_identity::metadata_identity(&file_meta, EntryKind::File)
                .unwrap();

        let dir_meta = std::fs::symlink_metadata(&dir_path).unwrap();
        let dir_id =
            crate::endpoint::local_identity::metadata_identity(&dir_meta, EntryKind::Directory)
                .unwrap();

        // Vanished entry is idempotent.
        remove_local_entry(&temp.path().join("missing"), false, Some(file_id))
            .await
            .unwrap();

        // Mismatched file identity fails.
        let wrong_id = EntryIdentity::from_bytes([77; 32]);
        let err = remove_local_entry(&file_path, false, Some(wrong_id))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "destination entry changed between scan and removal"
        );
        assert!(file_path.exists());

        // Type mismatch fails.
        let err = remove_local_entry(&file_path, true, None)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "destination entry type changed since the scan"
        );
        assert!(file_path.exists());

        // Matching file identity succeeds.
        remove_local_entry(&file_path, false, Some(file_id))
            .await
            .unwrap();
        assert!(!file_path.exists());

        // Mismatched directory identity fails.
        let err = remove_local_entry(&dir_path, true, Some(wrong_id))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "destination entry changed between scan and removal"
        );
        assert!(dir_path.exists());

        // Matching directory identity succeeds.
        remove_local_entry(&dir_path, true, Some(dir_id))
            .await
            .unwrap();
        assert!(!dir_path.exists());
    }

    #[tokio::test]
    async fn pull_remove_local_entry_does_not_recursively_delete_non_empty_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_path = temp.path().join("dir");
        std::fs::create_dir(&dir_path).unwrap();
        std::fs::write(dir_path.join("surviving"), b"keep me").unwrap();

        let dir_meta = std::fs::symlink_metadata(&dir_path).unwrap();
        let dir_id =
            crate::endpoint::local_identity::metadata_identity(&dir_meta, EntryKind::Directory)
                .unwrap();

        // Kept without an error when non-empty.
        remove_local_entry(&dir_path, true, Some(dir_id))
            .await
            .unwrap();
        assert!(dir_path.exists());
        assert!(dir_path.join("surviving").exists());
    }
}
