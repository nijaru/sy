//! Local-sync executor for the shared v3 controller pipeline (item 6).
//!
//! The third `SyncPlanExecutor` implementation after push and pull: same
//! safety-ordered controller loop (preorder directories, bounded concurrent
//! leaves, reverse-order deletes, journal-owned finalize), same planner and
//! reconciler, with the local filesystem on both sides.
//!
//! File transfers route through `endpoint::transfer::transfer_file`, which
//! keeps the native fast paths (clonefile/copy_file_range, reflink+patch,
//! sparse extent copy) and transactional staging; those strategies are the
//! entire point of local sync and must not regress behind a generic
//! streaming loop.

use crate::endpoint::existing::{self, ExistingDestinationError, FingerprintOptions};
use crate::endpoint::receipt::PublishedDestinationReceipt;
use crate::endpoint::transfer::{TransferOptions, TransferResult};
use crate::engine::domain::{Entry, EntryKind, RelativePath, Timestamp};
use crate::engine::hardlink_groups::{HardlinkGroups, HardlinkRepresentative};
use crate::engine::planner::ExecutionPolicy;
use crate::engine::scheduler::{ResourceRequest, Scheduler};
use crate::engine::work::WorkItem;
use crate::remote::acl::{read_preserved_acls, AclLocation, RemoteAclError};
use crate::remote::bsdflags::RemoteBsdFlagsError;
use crate::remote::xattr::{read_preserved_xattrs, RemoteXattrError, XattrLocation};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Working set per in-flight local transfer; mirrors the remote executors so
/// the scheduler byte budget is symmetric across directions.
pub const LOCAL_FILE_WORKING_SET: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LocalSyncError {
    #[error("hardlink bookkeeping failed: {0}")]
    HardlinkState(#[from] std::io::Error),
    #[error(transparent)]
    Existing(#[from] ExistingDestinationError),
    #[error(transparent)]
    Scheduler(#[from] crate::engine::scheduler::SchedulerError),

    #[error("local sync destination mutation failed for {0}: {1}")]
    Destination(PathBuf, std::io::Error),

    #[error("local sync source mutation failed for {0}: {1}")]
    Source(PathBuf, std::io::Error),

    #[error("symlink action is missing the scanned target for {0}")]
    MissingSymlinkTarget(PathBuf),

    #[error("destination mutation requires scanned identity for {0}")]
    MissingDestinationIdentity(PathBuf),

    #[error("regular-file transfer requires scanned Unix mode metadata for {0}")]
    MissingScannedMode(PathBuf),

    #[error("--backup location for {0} is not representable")]
    InvalidBackupPath(PathBuf),

    #[error(transparent)]
    Rooted(#[from] crate::rooted_fs::RootedFsError),

    #[error(transparent)]
    Endpoint(#[from] crate::error::SyncError),

    #[error("staged verification failed for {path}: expected {expected}, got {actual}")]
    VerificationFailed {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error(transparent)]
    Xattr(#[from] RemoteXattrError),

    #[error(transparent)]
    Acl(#[from] RemoteAclError),

    #[error(transparent)]
    BsdFlags(#[from] RemoteBsdFlagsError),
}

pub type Result<T> = std::result::Result<T, LocalSyncError>;

#[derive(Debug, Clone)]
pub enum LocalSyncAction {
    CreateDirectory {
        source: Entry,
        destination_path: RelativePath,
    },
    TransferFile {
        source: Entry,
        destination_path: RelativePath,
        destination: Option<Entry>,
        metadata: crate::endpoint::transfer::TransferMetadata,
        source_removal: bool,
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
        policy: ExecutionPolicy,
    },
}

pub type LocalTransferMetadata = crate::endpoint::transfer::TransferMetadata;

pub struct LocalSyncExecutor {
    source_root: crate::endpoint::source_root::SourceRoot,
    source_endpoint: crate::endpoint::local::LocalEndpoint,
    destination_endpoint: crate::endpoint::local::LocalEndpoint,
    scheduler: Scheduler,
    /// --backup: enabled marker, backup directory (None = beside the file),
    /// and suffix, mirroring the other executors.
    backup: bool,
    backup_dir: Option<PathBuf>,
    backup_suffix: String,
    /// --copy-links: transfers stat through source symlinks so the target's
    /// bytes and size flow through the native fast paths.
    follow_symlinks: bool,
    /// Shared --bwlimit pacing; `None` keeps the kernel fast paths.
    rate_limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    /// --remove-source-files: remove a committed or parity-verified source
    /// entry after the destination commit is acknowledged.
    remove_source_files: bool,
    /// -H/--preserve-hardlinks: group -> first committed destination path.
    /// The mutex is held across a grouped transfer so members serialize
    /// (ungrouped files stay concurrent), mirroring the remote executors.
    hardlinks: bool,
    hardlink_groups: tokio::sync::Mutex<HardlinkGroups>,
    hardlink_removals: crate::engine::hardlink_removals::HardlinkRemovalJournal,
    /// -X/--preserve-xattrs: mirror the source's extended attributes onto the
    /// destination for every entry this executor creates or updates. Symlinks
    /// are skipped (their attributes are not portable).
    xattrs: bool,
    /// -A/--preserve-acls: mirror the source's access-control list onto the
    /// destination for every entry this executor creates or updates.
    /// Symlinks are skipped, like xattrs.
    acls: bool,
    /// -F/--preserve-flags (macOS only): mirror the source's BSD file flags
    /// onto the destination for every entry this executor creates or
    /// updates. Symlinks are skipped, like xattrs.
    bsd_flags: bool,
    /// Post-write BLAKE3 verification (--verify).
    verify_on_write: bool,
    reporter: Option<Arc<crate::sync::output::SyncReporter>>,
}

impl LocalSyncExecutor {
    pub fn new(
        source_root: crate::endpoint::source_root::SourceRoot,
        destination_endpoint: crate::endpoint::local::LocalEndpoint,
        scheduler: Scheduler,
    ) -> Self {
        let source_endpoint = source_root.endpoint();
        Self {
            source_root,
            source_endpoint,
            destination_endpoint,
            scheduler,
            backup: false,
            backup_dir: None,
            backup_suffix: String::new(),
            follow_symlinks: false,
            rate_limiter: None,
            remove_source_files: false,
            verify_on_write: false,
            hardlinks: false,
            hardlink_groups: tokio::sync::Mutex::new(HardlinkGroups::default()),
            hardlink_removals: crate::engine::hardlink_removals::HardlinkRemovalJournal::default(),
            xattrs: false,
            acls: false,
            bsd_flags: false,
            reporter: None,
        }
    }

    pub fn with_backup(mut self, enabled: bool, dir: Option<PathBuf>, suffix: String) -> Self {
        self.backup = enabled;
        self.backup_dir = dir;
        self.backup_suffix = suffix;
        self
    }

    pub fn with_follow_symlinks(mut self, follow: bool) -> Self {
        self.follow_symlinks = follow;
        self
    }

    pub fn with_rate_limiter(
        mut self,
        limiter: Option<Arc<std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>>,
    ) -> Self {
        self.rate_limiter = limiter;
        self
    }

    pub fn with_remove_source_files(mut self, enabled: bool) -> Self {
        self.remove_source_files = enabled;
        self
    }

    pub fn with_verify_on_write(mut self, enabled: bool) -> Self {
        self.verify_on_write = enabled;
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

    pub fn with_reporter(
        mut self,
        reporter: Option<Arc<crate::sync::output::SyncReporter>>,
    ) -> Self {
        self.reporter = reporter;
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

    fn report_transfer(
        &self,
        op: crate::sync::output::ItemizeOp,
        path: &RelativePath,
        size: u64,
        bytes: u64,
    ) {
        if let Some(reporter) = &self.reporter {
            match op {
                crate::sync::output::ItemizeOp::Create => reporter.created(
                    crate::sync::output::ItemizeKind::File,
                    path.as_path(),
                    size,
                    bytes,
                ),
                _ => reporter.updated(path.as_path(), size, bytes, bytes < size),
            }
        }
    }

    fn source_path(&self, relative: &RelativePath) -> PathBuf {
        self.source_root.path().join(relative.as_path())
    }

    /// Read the source's extended attributes for one entry when `-X` requested
    /// them. Symlinks carry no portable mutable attributes, so they are skipped
    /// rather than having their targets resolved.
    async fn read_source_xattrs(&self, source: &Entry) -> Result<Option<Vec<(OsString, Vec<u8>)>>> {
        if !self.xattrs || source.is_symlink() {
            return Ok(None);
        }
        if source.is_file() {
            return Ok(Some(
                existing::observed_xattrs(self.source_root.clone(), source.clone()).await?,
            ));
        }
        let location = XattrLocation::Local(&self.source_root);
        let xattrs = read_preserved_xattrs(&location, source).await?;
        Ok(Some(xattrs))
    }

    /// Read the source's access-control list for one entry when `-A`
    /// requested it. Symlinks are skipped, like xattrs.
    async fn read_source_acls(&self, source: &Entry) -> Result<Option<String>> {
        if !self.acls || source.is_symlink() {
            return Ok(None);
        }
        if source.is_file() {
            return Ok(Some(
                existing::observed_acl(self.source_root.clone(), source.clone())
                    .await?
                    .unwrap_or_default(),
            ));
        }
        let location = AclLocation::Local(&self.source_root);
        let acl = read_preserved_acls(&location, source).await?;
        Ok(Some(acl.unwrap_or_default()))
    }

    /// Read the source's BSD file flags for one entry when `-F` requested
    /// them (macOS only; elsewhere `-F` is refused up front). Symlinks are
    /// skipped, like xattrs.
    async fn read_source_bsd_flags(&self, source: &Entry) -> Result<Option<u32>> {
        if !self.bsd_flags || source.is_symlink() {
            return Ok(None);
        }
        let flags = existing::observed_flags(self.source_root.clone(), source.clone()).await?;
        Ok(Some(flags))
    }

    fn destination_path(&self, relative: &RelativePath) -> PathBuf {
        crate::endpoint::Endpoint::root(&self.destination_endpoint).join(relative.as_path())
    }

    fn lower_unchanged_file_preservation(
        &self,
        source: Entry,
        destination: Entry,
        policy: ExecutionPolicy,
    ) -> Result<Option<WorkItem<LocalSyncAction>>> {
        if !source.is_file() || !(self.xattrs || self.acls || self.bsd_flags) {
            return Ok(None);
        }
        let (unix_mode, modified) =
            requested_metadata(&source, &destination, policy)?.unwrap_or((None, None));
        // Quick equality is not byte equality: keep the observed destination.
        Ok(Some(metadata_work(
            source,
            destination,
            unix_mode,
            modified,
            policy,
        )?))
    }

    /// Backup location for one destination-relative path, matching the
    /// other executors: beside the file (name + suffix) or under --backup-dir
    /// with the tree shape preserved (GNU rsync semantics). Always
    /// destination-anchored — never relative to the process CWD.
    fn backup_destination_for(&self, relative: &RelativePath) -> Result<PathBuf> {
        backup_destination_path(
            crate::endpoint::Endpoint::root(&self.destination_endpoint),
            relative,
            self.backup_dir.as_deref(),
            &self.backup_suffix,
        )
    }

    /// Preserve a to-be-replaced destination file. COPY semantics: the
    /// original stays in place so it can still serve as the reflink/delta
    /// basis for the replacement transfer's staging. The copy never follows
    /// symlinks; a backup that resolved a link target would copy the wrong
    /// tree (and could touch a target outside both roots).
    async fn backup_replacement_file(
        &self,
        relative: &RelativePath,
        identity: Option<crate::engine::domain::EntryIdentity>,
    ) -> Result<()> {
        let expected = identity.ok_or_else(|| {
            LocalSyncError::MissingDestinationIdentity(self.destination_path(relative))
        })?;
        let backup = self.backup_destination_for(relative)?;
        let rooted = self.metadata_authority().await?;
        let relative = relative.clone();
        tokio::task::spawn_blocking(move || {
            rooted.verify_root_path_blocking()?;
            rooted.backup_file_blocking(&relative, &backup, expected)?;
            rooted.verify_root_path_blocking()
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        Ok(())
    }

    /// Copy a regular deletion candidate privately, then revalidate before
    /// unlinking it. Unlike move-backed backups, copy failure cannot remove
    /// the original. Neither the final stat/unlink nor backup commit is CAS.
    async fn backup_deleted_entry(
        &self,
        relative: &RelativePath,
        expected: crate::engine::domain::EntryIdentity,
    ) -> Result<()> {
        let backup = self.backup_destination_for(relative)?;
        let rooted = self.metadata_authority().await?;
        let relative = relative.clone();
        tokio::task::spawn_blocking(move || {
            rooted.verify_root_path_blocking()?;
            rooted.backup_file_blocking(&relative, &backup, expected)?;
            rooted.verify_root_path_blocking()?;
            rooted.remove_destination_blocking(&relative, false, Some(expected))?;
            rooted.verify_root_path_blocking()
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        Ok(())
    }

    async fn execute(
        &self,
        item: WorkItem<LocalSyncAction>,
    ) -> Result<crate::engine::work::WorkResult> {
        let (action, resources) = item.into_parts();
        let permit = self.scheduler.acquire(resources).await?;

        match action {
            LocalSyncAction::CreateDirectory {
                source,
                destination_path,
            } => {
                let source_root = self.source_root.rooted();
                let expected = source.identity.ok_or_else(|| {
                    crate::rooted_fs::RootedFsError::DestinationChanged(
                        source.path.as_path().to_path_buf(),
                    )
                })?;
                let rooted = self.metadata_authority().await?;
                let relative = source.path.clone();
                let identity = tokio::task::spawn_blocking(move || {
                    source_root.verify_root_path_blocking()?;
                    source_root.read_directory_preservation_blocking(
                        &relative,
                        expected,
                        Default::default(),
                    )?;
                    source_root.verify_root_path_blocking()?;
                    rooted.verify_root_path_blocking()?;
                    let identity = rooted.create_directory_blocking(&destination_path)?;
                    rooted.verify_root_path_blocking()?;
                    Ok::<_, crate::rooted_fs::RootedFsError>(identity)
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
            LocalSyncAction::TransferFile {
                source,
                destination_path,
                destination,
                metadata,
                source_removal,
            } => {
                // -H: members after the first link to the representative.
                if self.hardlinks {
                    if let Some(identity) = source.hardlink_group {
                        let group = *identity.as_bytes();
                        return self
                            .execute_grouped_file(
                                source,
                                destination_path,
                                destination,
                                metadata,
                                group,
                                source_removal,
                            )
                            .await
                            .map(crate::engine::work::WorkResult::Transfer);
                    }
                }
                let is_update = destination.is_some();
                // BSD flags remain a post-commit phase; xattrs and ACLs are
                // captured by the transfer layer from the held byte source.
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                let (transfer, receipt) = self
                    .transfer_source_file(
                        &source,
                        &destination_path,
                        &destination,
                        &metadata,
                        crate::endpoint::io::PreservationRequest {
                            xattrs: self.xattrs && !source.is_symlink(),
                            acl: self.acls && !source.is_symlink(),
                        },
                        bsd_flags,
                    )
                    .await?;
                if source_removal {
                    self.remove_committed_source(&receipt, &source).await?;
                }
                let op = if is_update {
                    crate::sync::output::ItemizeOp::Update
                } else {
                    crate::sync::output::ItemizeOp::Create
                };
                self.report_transfer(op, &destination_path, source.size, transfer.literal_bytes);
                Ok(crate::engine::work::WorkResult::Transfer(transfer))
            }
            LocalSyncAction::ReplaceSymlink {
                source,
                destination_path,
                destination,
                modified,
            } => {
                self.check_source_identity(&source).await?;
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    LocalSyncError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
                if let Some(existing) = destination
                    .as_ref()
                    .filter(|entry| self.backup && entry.is_file())
                {
                    self.backup_replacement_file(&existing.path, existing.identity)
                        .await?;
                }
                let expected = match destination {
                    None => crate::endpoint::ExpectedDestination::Absent,
                    Some(destination) => crate::endpoint::ExpectedDestination::Unchanged(
                        destination.identity.ok_or_else(|| {
                            LocalSyncError::MissingDestinationIdentity(
                                destination.path.as_path().to_path_buf(),
                            )
                        })?,
                    ),
                };
                let publication = self
                    .replace_symlink(target, &destination_path, expected, modified)
                    .await?;
                let receipt = PublishedDestinationReceipt::for_symlink(
                    source.path.clone(),
                    destination_path.clone(),
                    source.identity,
                    publication,
                );
                self.remove_committed_source(&receipt, &source).await?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Symlink,
                    &destination_path,
                );
                Ok(crate::engine::work::WorkResult::Metadata)
            }
            LocalSyncAction::ApplyMetadata {
                source,
                mut destination,
                unix_mode,
                modified,
                policy,
            } => {
                let expected_destination = destination.identity.ok_or_else(|| {
                    LocalSyncError::MissingDestinationIdentity(
                        destination.path.as_path().to_path_buf(),
                    )
                })?;
                self.check_source_identity(&source).await?;
                let xattrs = self.read_source_xattrs(&source).await?;
                let acls = self.read_source_acls(&source).await?;
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                self.check_source_identity(&source).await?;
                let rooted = self.metadata_authority().await?;
                let relative = destination.path.clone();
                let kind = source.kind;
                let identity = tokio::task::spawn_blocking(move || {
                    rooted.verify_root_path_blocking()?;
                    let identity = rooted.apply_observed_preservation_blocking(
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
                    rooted.verify_root_path_blocking()?;
                    Ok::<_, crate::rooted_fs::RootedFsError>(identity)
                })
                .await
                .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
                self.check_source_identity(&source).await?;
                destination.identity = Some(identity);
                if let Some(mode) = unix_mode {
                    destination.unix_mode = Some(mode);
                }
                if let Some(time) = modified {
                    destination.modified = time;
                }
                // Fingerprinting has its own file/byte/CPU admission.
                drop(permit);
                if source.is_file() {
                    self.remove_unchanged_source(&source, &destination, policy)
                        .await?;
                }
                Ok(crate::engine::work::WorkResult::Metadata)
            }
        }
    }

    /// One grouped file under `-H`: transfer the representative or link
    /// subsequent members to it. The linking member revalidates its scan
    /// identity first so a source replaced after the scan fails loudly
    /// instead of linking stale bytes. The group mutex serializes members.
    async fn execute_grouped_file(
        &self,
        source: Entry,
        destination_path: RelativePath,
        destination: Option<Entry>,
        metadata: LocalTransferMetadata,
        group: [u8; 32],
        source_removal: bool,
    ) -> Result<crate::engine::work::TransferSummary> {
        let groups = self.hardlink_groups.lock().await;
        if let Some(first) = groups.get(group).await? {
            self.check_source_identity(&source).await?;
            let dest_abs = self.destination_path(&destination_path);
            first.validate_metadata(metadata.unix_mode, metadata.modified)?;
            let is_update = destination.is_some();
            if let Some(existing) = destination
                .as_ref()
                .filter(|entry| self.backup && entry.is_file())
            {
                self.backup_replacement_file(&existing.path, existing.identity)
                    .await?;
            }
            let expected = match destination.as_ref() {
                None => crate::endpoint::ExpectedDestination::Absent,
                Some(entry) => {
                    crate::endpoint::ExpectedDestination::Unchanged(entry.identity.ok_or_else(
                        || LocalSyncError::MissingDestinationIdentity(dest_abs.clone()),
                    )?)
                }
            };
            let rooted = self.metadata_authority().await?;
            let first_path = first.path.clone();
            let destination_path_for_worker = destination_path.clone();
            let publication = tokio::task::spawn_blocking(move || {
                rooted.verify_root_path_blocking()?;
                let proof = rooted.publish_hardlink_blocking(
                    &first_path,
                    &destination_path_for_worker,
                    first.publication,
                    expected,
                )?;
                crate::endpoint::local::verify_committed_root(
                    &rooted,
                    &rooted
                        .root_path()
                        .join(destination_path_for_worker.as_path()),
                )?;
                Ok::<_, LocalSyncError>(proof)
            })
            .await
            .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
            groups.advance(group, publication.identity).await?;
            let receipt = PublishedDestinationReceipt::for_hardlink(
                source.path.clone(),
                destination_path.clone(),
                source.identity,
                publication,
            );
            if source_removal {
                self.defer_grouped_source_removal(group, &receipt, &source)
                    .await?;
            }
            let op = if is_update {
                crate::sync::output::ItemizeOp::Update
            } else {
                crate::sync::output::ItemizeOp::Create
            };
            self.report_transfer(op, &destination_path, source.size, 0);
            return Ok(crate::engine::work::TransferSummary {
                file_size: source.size,
                digest: [0_u8; 32],
                literal_bytes: 0,
                reused_bytes: source.size,
            });
        }
        let is_update = destination.is_some();
        // BSD flags remain a post-commit phase; xattrs and ACLs are captured
        // from the held source that supplies the representative's bytes.
        let bsd_flags = self.read_source_bsd_flags(&source).await?;
        let (transfer, receipt) = self
            .transfer_source_file(
                &source,
                &destination_path,
                &destination,
                &metadata,
                crate::endpoint::io::PreservationRequest {
                    xattrs: self.xattrs && !source.is_symlink(),
                    acl: self.acls && !source.is_symlink(),
                },
                bsd_flags,
            )
            .await?;
        groups
            .insert(
                group,
                HardlinkRepresentative {
                    path: destination_path.clone(),
                    publication: receipt
                        .publication()
                        .map_err(std::io::Error::other)?
                        .identity,
                    unix_mode: metadata.unix_mode,
                    modified: metadata.modified,
                },
            )
            .await?;
        if source_removal {
            self.defer_grouped_source_removal(group, &receipt, &source)
                .await?;
        }
        drop(groups);
        let op = if is_update {
            crate::sync::output::ItemizeOp::Update
        } else {
            crate::sync::output::ItemizeOp::Create
        };
        self.report_transfer(op, &destination_path, source.size, transfer.literal_bytes);
        Ok(transfer)
    }

    async fn defer_grouped_source_removal(
        &self,
        group: [u8; 32],
        receipt: &PublishedDestinationReceipt,
        source: &Entry,
    ) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        receipt.validate_source_removal(source).map_err(|error| {
            LocalSyncError::Source(
                self.source_path(&source.path),
                std::io::Error::other(error.to_string()),
            )
        })?;
        self.check_source_identity(source).await?;
        self.hardlink_removals
            .append(
                group,
                source,
                receipt.publication().map_err(std::io::Error::other)?,
            )
            .await?;
        Ok(())
    }

    async fn finish_deferred_source_removals(&self) -> Result<()> {
        if self.remove_source_files && self.hardlinks {
            let groups = self.hardlink_groups.lock().await;
            self.hardlink_removals
                .replay(self.source_root.clone(), &groups, |proof| async move {
                    // Empty/skipped selected work may have no destination
                    // root. Acquire only when a recorded proof needs it.
                    let rooted = self
                        .metadata_authority()
                        .await
                        .map_err(std::io::Error::other)?;
                    tokio::task::spawn_blocking(move || {
                        rooted.verify_root_path_blocking()?;
                        proof.revalidate_blocking(&rooted)?;
                        rooted.verify_root_path_blocking()
                    })
                    .await
                    .map_err(std::io::Error::other)?
                    .map_err(std::io::Error::other)
                })
                .await?;
        }
        Ok(())
    }

    /// Revalidate a linking member's scan identity (cheap stat, no bytes).
    async fn check_source_identity(&self, source: &Entry) -> Result<()> {
        if self.follow_symlinks && source.is_file() {
            use crate::endpoint::Endpoint;
            let file = self
                .source_endpoint
                .open_native_file_following(source.path.as_path())
                .await
                .map_err(|error| {
                    LocalSyncError::Source(
                        self.source_path(&source.path),
                        std::io::Error::other(error),
                    )
                })?
                .ok_or_else(|| ExistingDestinationError::MissingObservation(source.path.clone()))?;
            let source = source.clone();
            let source_root = self.source_root.clone();
            tokio::task::spawn_blocking(move || {
                source_root.validate_blocking()?;
                if source.identity.is_none()
                    || crate::endpoint::local_identity::metadata_identity(
                        &file.metadata()?,
                        EntryKind::File,
                    ) != source.identity
                {
                    return Err(ExistingDestinationError::ObservationChanged(source.path));
                }
                source_root.validate_blocking()?;
                Ok::<(), ExistingDestinationError>(())
            })
            .await
            .map_err(|error| ExistingDestinationError::Worker(error.to_string()))??;
            return Ok(());
        }
        let rooted = self.source_root.rooted();
        let source = source.clone();
        tokio::task::spawn_blocking(move || existing::validate_path(&rooted, &source))
            .await
            .map_err(|error| ExistingDestinationError::Worker(error.to_string()))??;
        Ok(())
    }

    /// One file through the transfer layer's staging, strategy selection,
    /// verification, and atomic commit.
    async fn transfer_source_file(
        &self,
        source: &Entry,
        destination_path: &RelativePath,
        destination: &Option<Entry>,
        metadata: &crate::endpoint::transfer::TransferMetadata,
        preservation_request: crate::endpoint::io::PreservationRequest,
        final_flags: Option<u32>,
    ) -> Result<(
        crate::engine::work::TransferSummary,
        PublishedDestinationReceipt,
    )> {
        let backup = destination
            .as_ref()
            .filter(|entry| self.backup && entry.is_file());
        let expected_destination = match destination {
            Some(destination) => match destination.identity {
                Some(scanned) => {
                    let rooted = self.metadata_authority().await?;
                    let current = tokio::task::spawn_blocking(move || {
                        rooted.retired_destination_identity_blocking(scanned)
                    })
                    .await
                    .map_err(|error| {
                        crate::rooted_fs::RootedFsError::Worker(error.to_string())
                    })??;
                    crate::endpoint::io::ExpectedDestination::Unchanged(current)
                }
                None => crate::endpoint::io::ExpectedDestination::Unverified,
            },
            None => crate::endpoint::io::ExpectedDestination::Absent,
        };
        let result: TransferResult = crate::endpoint::transfer::transfer_file_with_before_stage(
            &self.source_endpoint,
            source.path.as_path(),
            &self.destination_endpoint,
            destination_path.as_path(),
            TransferOptions {
                // A type replacement (file over symlink, link over file)
                // must not treat the mismatched destination as a delta
                // basis: staging replaces it wholesale.
                update: destination.is_some(),
                verify: self.verify_on_write,
                follow_symlinks: self.follow_symlinks,
                rate_limiter: self.rate_limiter.clone(),
                identity: crate::endpoint::transfer::TransferIdentity {
                    source: crate::endpoint::transfer::SourceExpectation::Scanned(
                        source.identity.ok_or_else(|| {
                            ExistingDestinationError::MissingObservation(source.path.clone())
                        })?,
                    ),
                    destination: expected_destination,
                },
                preservation: crate::endpoint::io::Preservation::default(),
                preservation_request,
                final_flags,
                metadata: Some(*metadata),
            },
            || async {
                if let Some(existing) = backup {
                    self.backup_replacement_file(&existing.path, existing.identity)
                        .await
                        .map_err(std::io::Error::other)?;
                }
                Ok(())
            },
        )
        .await
        .map_err(|error| match error {
            crate::error::SyncError::Io(io) => {
                LocalSyncError::Destination(self.destination_path(destination_path), io)
            }
            other => LocalSyncError::Endpoint(other),
        })?;
        // Fail-fast on staged verification mismatch: the previous
        // destination is intact (the transfer layer aborted staging), and
        // the sync surfaces the failure instead of tolerating it.
        if let crate::endpoint::io::VerificationStatus::Failed { expected, actual } =
            result.verification
        {
            return Err(LocalSyncError::VerificationFailed {
                path: self.destination_path(destination_path),
                expected: expected.to_hex().to_string(),
                actual: actual.to_hex().to_string(),
            });
        }
        // Native strategies do not compute a whole-file digest on the happy
        // path; the summary carries byte accounting for the controller's
        // counters.
        Ok((
            crate::engine::work::TransferSummary {
                file_size: source.size,
                digest: [0_u8; 32],
                literal_bytes: result.bytes_written,
                reused_bytes: 0,
            },
            result.receipt,
        ))
    }

    /// Replace a destination entry with a symlink.
    async fn replace_symlink(
        &self,
        target: &Path,
        relative: &RelativePath,
        expected: crate::endpoint::ExpectedDestination,
        modified: Option<Timestamp>,
    ) -> Result<crate::rooted_fs::PublishedEntryProof> {
        let rooted = self.metadata_authority().await?;
        let target = target.to_path_buf();
        let relative = relative.clone();
        tokio::task::spawn_blocking(move || {
            rooted.verify_root_path_blocking()?;
            let proof = rooted.publish_symlink_blocking(&relative, &target, expected, modified)?;
            crate::endpoint::local::verify_committed_root(
                &rooted,
                &rooted.root_path().join(relative.as_path()),
            )?;
            Ok::<_, LocalSyncError>(proof)
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))?
    }

    async fn metadata_authority(&self) -> Result<crate::rooted_fs::RootedFs> {
        // Byte staging and executor links must share one run-owned lineage;
        // independently opening the same pathname creates distinct authority.
        let rooted = self.destination_endpoint.rooted_fs(false).await?;
        Ok(tokio::task::spawn_blocking(move || {
            rooted.verify_root_path_blocking()?;
            Ok::<_, crate::rooted_fs::RootedFsError>(rooted.as_ref().clone())
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??)
    }

    /// --remove-source-files: remove the source entry after the destination
    /// commit has produced an authorized publication receipt. The receipt
    /// is validated, and the on-disk scan identity is re-checked before
    /// unlinking — a source replaced or modified after the scan must not be
    /// removed, because the moved bytes would not be the verified ones.
    /// Only non-directories move; empty directories stay, matching rsync.
    async fn remove_committed_source(
        &self,
        receipt: &PublishedDestinationReceipt,
        source: &Entry,
    ) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        receipt.validate_source_removal(source).map_err(|error| {
            LocalSyncError::Source(
                self.source_path(&source.path),
                std::io::Error::other(error.to_string()),
            )
        })?;
        let destination = self.metadata_authority().await?;
        let rooted = self.source_root.rooted();
        let receipt = receipt.clone();
        let source = source.clone();
        // Check the operator address in the unlink worker, not just before it
        // is queued. The separate root/name checks and unlink are not CAS.
        tokio::task::spawn_blocking(move || {
            destination.verify_root_path_blocking()?;
            receipt.revalidate_destination_blocking(&destination)?;
            destination.verify_root_path_blocking()?;
            existing::remove_observed_source_blocking(&rooted, &source)?;
            Ok::<_, LocalSyncError>(())
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))?
    }

    /// Delete one destination-only entry after the delete-threshold gate. A
    /// non-empty directory is kept without an error (its surviving content
    /// is legitimate, e.g. --backup repopulation, matching rsync); only a
    /// genuinely empty directory is removed.
    async fn execute_delete(&self, action: crate::engine::delete_plan::DeleteAction) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                ..ResourceRequest::default()
            })
            .await?;
        let path = self.destination_path(&action.path);
        let expected = action
            .identity
            .ok_or_else(|| LocalSyncError::MissingDestinationIdentity(path.clone()))?;
        if self.backup && action.kind == EntryKind::File {
            self.backup_deleted_entry(&action.path, expected).await?;
        } else {
            let rooted = self.metadata_authority().await?;
            let action = action.clone();
            let result = tokio::task::spawn_blocking(move || {
                rooted.verify_root_path_blocking()?;
                rooted.remove_destination_blocking(
                    &action.path,
                    action.kind == EntryKind::Directory,
                    Some(expected),
                )?;
                rooted.verify_root_path_blocking()
            })
            .await
            .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))?;
            match result {
                Ok(()) => {}
                Err(crate::rooted_fs::RootedFsError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
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

    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                ..ResourceRequest::default()
            })
            .await?;
        let rooted = self.metadata_authority().await?;
        let source = self.source_root.rooted();
        let request = crate::rooted_fs::DirectoryPreservationRequest {
            xattrs: self.xattrs,
            acl: self.acls,
            bsd_flags: self.bsd_flags,
        };
        tokio::task::spawn_blocking(move || {
            let crate::engine::finalize_journal::DirectoryTarget::Observed(expected) =
                metadata.target
            else {
                return Err(crate::rooted_fs::RootedFsError::DestinationChanged(
                    metadata.path.as_path().to_path_buf(),
                ));
            };
            source.verify_root_path_blocking()?;
            let preservation = if metadata.preserve_source {
                source.read_directory_preservation_blocking(
                    &metadata.path,
                    metadata.source_identity,
                    request,
                )?
            } else {
                Default::default()
            };
            source.verify_root_path_blocking()?;
            rooted.verify_root_path_blocking()?;
            rooted.finalize_directory_blocking(
                &metadata.path,
                expected,
                metadata.unix_mode,
                metadata.modified,
                &preservation,
            )?;
            rooted.verify_root_path_blocking()
        })
        .await
        .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
        Ok(())
    }

    /// Re-prove content and selected preservation on the observed destination;
    /// a preflight comparison alone cannot authorize deletion later.
    async fn remove_unchanged_source(
        &self,
        source: &Entry,
        destination: &Entry,
        policy: ExecutionPolicy,
    ) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        let options = FingerprintOptions {
            permissions: policy.preserve_permissions,
            times: policy.preserve_times,
            xattrs: self.xattrs,
            acls: self.acls,
            bsd_flags: self.bsd_flags,
        };
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                active_files: 1,
                buffered_bytes: options.buffered_bytes(),
                metadata_ops: 1,
                cpu_tasks: 1,
                ..ResourceRequest::default()
            })
            .await?;
        let source_rooted = self.source_root.rooted();
        let destination_rooted = self.metadata_authority().await?;
        let source_fingerprint =
            existing::fingerprint(source_rooted.clone(), source.clone(), options).await?;
        let destination_fingerprint =
            existing::fingerprint(destination_rooted.clone(), destination.clone(), options).await?;
        let receipt = existing::receipt(
            source,
            destination,
            source_fingerprint,
            destination_fingerprint,
        )?;
        if self.hardlinks {
            if let Some(group) = source.hardlink_group {
                self.check_source_identity(source).await?;
                self.hardlink_removals
                    .append_existing(*group.as_bytes(), source, &receipt)
                    .await?;
                return Ok(());
            }
        }
        existing::remove_verified_source(
            source_rooted,
            source.clone(),
            receipt,
            Some(destination_rooted),
        )
        .await?;
        Ok(())
    }
}

impl crate::engine::controller::SyncPlanExecutor for LocalSyncExecutor {
    type Action = LocalSyncAction;
    type Error = LocalSyncError;

    fn lower(
        &self,
        op: crate::engine::domain::SyncOp,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
        if let crate::engine::domain::SyncOp::Unchanged {
            source,
            destination,
            ..
        } = op
        {
            return self.lower_unchanged_file_preservation(source, destination, policy);
        }
        lower_local_op(op, policy)
    }

    fn is_directory_action(&self, action: &LocalSyncAction) -> bool {
        matches!(action, LocalSyncAction::CreateDirectory { .. })
    }

    async fn execute(
        &self,
        item: WorkItem<LocalSyncAction>,
    ) -> std::result::Result<crate::engine::work::WorkResult, LocalSyncError> {
        LocalSyncExecutor::execute(self, item).await
    }

    async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::execute_delete(self, action).await
    }

    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::execute_finalize(self, metadata).await
    }

    async fn finish_deferred_source_removals(&self) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::finish_deferred_source_removals(self).await
    }

    async fn remove_unchanged_source(
        &self,
        source: &Entry,
        destination: &Entry,
        policy: ExecutionPolicy,
    ) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::remove_unchanged_source(self, source, destination, policy).await
    }
}

/// One owner for backup effect addresses, shared by preflight and execution.
pub fn backup_destination_path(
    destination_root: &Path,
    relative: &RelativePath,
    dir: Option<&Path>,
    suffix: &str,
) -> Result<PathBuf> {
    let mut backup_name = relative
        .as_path()
        .file_name()
        .ok_or_else(|| LocalSyncError::InvalidBackupPath(relative.as_path().to_path_buf()))?
        .to_os_string();
    backup_name.push(suffix);
    match dir {
        Some(dir) => {
            let mut backup = dir.join(relative.as_path());
            backup.set_file_name(backup_name);
            Ok(backup)
        }
        None => {
            let mut backup = destination_root.join(relative.as_path());
            backup.set_file_name(backup_name);
            Ok(backup)
        }
    }
}

pub fn action_resources(action: &LocalSyncAction) -> ResourceRequest {
    match action {
        LocalSyncAction::TransferFile { .. } => ResourceRequest {
            active_files: 1,
            buffered_bytes: LOCAL_FILE_WORKING_SET,
            metadata_ops: 0,
            cpu_tasks: 1,
            network_writes: 0,
        },
        LocalSyncAction::ApplyMetadata { .. } => ResourceRequest {
            active_files: 0,
            buffered_bytes: 4 * crate::protocol::MAX_FRAME_PAYLOAD as u64,
            metadata_ops: 1,
            cpu_tasks: 1,
            network_writes: 0,
        },
        _ => ResourceRequest {
            active_files: 0,
            buffered_bytes: 0,
            metadata_ops: 1,
            cpu_tasks: 0,
            network_writes: 0,
        },
    }
}

/// Lower one semantic planner operation to a local action. Mirrors the push
/// and pull lowering: same mode/times policy handling, same
/// directory-transition refusal (the controller replays directory
/// finalize metadata child-before-parent; a transactional directory
/// replacement is a separate design).
pub fn lower_local_op(
    op: crate::engine::domain::SyncOp,
    policy: ExecutionPolicy,
) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
    use crate::engine::domain::SyncOp;

    match op {
        SyncOp::Create {
            source,
            destination_path,
        } => lower_create(source, destination_path, policy),
        SyncOp::Update {
            source,
            destination,
        } => lower_update(source, destination, policy),
        SyncOp::Replace {
            source,
            destination,
        } => lower_replace(source, destination, policy),
        SyncOp::Metadata {
            source,
            destination,
        } => lower_metadata(source, destination, policy),
        SyncOp::Skip { .. } | SyncOp::Unchanged { .. } => Ok(None),
    }
}

pub type RequestedMetadata = Option<(Option<u32>, Option<Timestamp>)>;

fn lower_create(
    source: Entry,
    destination_path: RelativePath,
    policy: ExecutionPolicy,
) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
    match source.kind {
        EntryKind::Directory => Ok(Some(mutation_work(LocalSyncAction::CreateDirectory {
            source,
            destination_path,
        }))),
        EntryKind::File => {
            // Staged files are private at 0600, so a committed file always
            // needs an explicit final mode; the source's scanned mode is the
            // create default (matches the other executors).
            let mode = source.unix_mode.ok_or_else(|| {
                LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = LocalTransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(Some(file_work(LocalSyncAction::TransferFile {
                source,
                destination_path,
                destination: None,
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Symlink => Ok(Some(mutation_work(LocalSyncAction::ReplaceSymlink {
            modified: policy.preserve_times.then_some(source.modified),
            source,
            destination_path,
            destination: None,
        }))),
    }
}

fn lower_update(
    source: Entry,
    destination: Entry,
    policy: ExecutionPolicy,
) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
    let destination_path = destination.path.clone();
    match source.kind {
        EntryKind::File => {
            let mode = if policy.preserve_permissions {
                source.unix_mode
            } else {
                destination.unix_mode
            }
            .ok_or_else(|| {
                LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = LocalTransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(Some(file_work(LocalSyncAction::TransferFile {
                source,
                destination_path,
                destination: Some(destination),
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Directory => lower_metadata(source, destination, policy),
        EntryKind::Symlink => Ok(Some(mutation_work(LocalSyncAction::ReplaceSymlink {
            modified: policy.preserve_times.then_some(source.modified),
            destination_path: destination.path.clone(),
            source,
            destination: Some(destination),
        }))),
    }
}

fn lower_replace(
    source: Entry,
    destination: Entry,
    policy: ExecutionPolicy,
) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
    let destination_path = destination.path.clone();
    match source.kind {
        EntryKind::Directory => Err(LocalSyncError::Destination(
            source.path.as_path().to_path_buf(),
            std::io::Error::other("transactional directory replacement is not implemented"),
        )),
        EntryKind::File => {
            let mode = source.unix_mode.ok_or_else(|| {
                LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = LocalTransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(Some(file_work(LocalSyncAction::TransferFile {
                source,
                destination_path,
                destination: Some(destination),
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Symlink => Ok(Some(mutation_work(LocalSyncAction::ReplaceSymlink {
            modified: policy.preserve_times.then_some(source.modified),
            destination_path: destination.path.clone(),
            source,
            destination: Some(destination),
        }))),
    }
}

fn lower_metadata(
    source: Entry,
    destination: Entry,
    policy: ExecutionPolicy,
) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
    if source.is_directory() {
        return Ok(None);
    }
    let Some((unix_mode, modified)) = requested_metadata(&source, &destination, policy)? else {
        return Ok(None);
    };
    Ok(Some(metadata_work(
        source,
        destination,
        unix_mode,
        modified,
        policy,
    )?))
}

fn requested_metadata(
    source: &Entry,
    destination: &Entry,
    policy: ExecutionPolicy,
) -> std::result::Result<RequestedMetadata, LocalSyncError> {
    // Preservation constrains the result even when the scanned modes match:
    // ACL/xattr work can change mode bits, including native SGID retention.
    let unix_mode = if policy.preserve_permissions && !source.is_symlink() {
        Some(source.unix_mode.ok_or_else(|| {
            LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf())
        })?)
    } else {
        None
    };
    let modified = policy
        .preserve_times
        .then_some(source.modified)
        .filter(|_| destination.modified != source.modified);
    Ok((unix_mode.is_some() || modified.is_some()).then_some((unix_mode, modified)))
}

fn file_work(action: LocalSyncAction) -> WorkItem<LocalSyncAction> {
    let resources = action_resources(&action);
    WorkItem::new(action, resources)
}

fn mutation_work(action: LocalSyncAction) -> WorkItem<LocalSyncAction> {
    let resources = action_resources(&action);
    WorkItem::new(action, resources)
}

fn metadata_work(
    source: Entry,
    destination: Entry,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
    policy: ExecutionPolicy,
) -> Result<WorkItem<LocalSyncAction>> {
    if destination.identity.is_none() {
        return Err(LocalSyncError::MissingDestinationIdentity(
            destination.path.as_path().to_path_buf(),
        ));
    }
    Ok(mutation_work(LocalSyncAction::ApplyMetadata {
        source,
        destination,
        unix_mode,
        modified,
        policy,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::delete_plan::DeleteAction;
    use crate::engine::domain::{EntryIdentity, SyncOp};
    use crate::engine::scheduler::ResourceBudget;

    fn rel(path: &str) -> RelativePath {
        RelativePath::new(path).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn planned_publication_rejects_replaced_root_with_identical_leaf_alias() {
        use futures::TryStreamExt;
        let parent = tempfile::tempdir().unwrap();
        let source_root = parent.path().join("source");
        let destination_root = parent.path().join("destination");
        let foreign = parent.path().join("foreign");
        let held = parent.path().join("held");
        for root in [&source_root, &destination_root, &foreign] {
            std::fs::create_dir(root).unwrap();
        }
        std::fs::write(source_root.join("file"), b"new bytes").unwrap();
        std::fs::write(destination_root.join("file"), b"old bytes").unwrap();
        // Establish the alias before scanning, so nlink/ctime cannot expose a
        // fresh-path executor's adoption of the foreign root.
        std::fs::hard_link(destination_root.join("file"), foreign.join("file")).unwrap();
        let authority = crate::endpoint::source_root::SourceRoot::open(source_root.clone())
            .await
            .unwrap();
        let request = crate::engine::scan::ScanRequest {
            metadata: crate::engine::scan::EntryMetadataRequest {
                unix_mode: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let source = authority
            .entries(request)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        let endpoint = crate::endpoint::local::LocalEndpoint::new(destination_root.clone());
        let rooted = endpoint.rooted_fs(false).await.unwrap();
        let destination = rooted
            .entry_stream(request)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        let executor = LocalSyncExecutor::new(
            authority,
            endpoint,
            Scheduler::new(ResourceBudget::default()).unwrap(),
        )
        .with_remove_source_files(true);
        let work = lower_local_op(
            SyncOp::Update {
                source,
                destination,
            },
            ExecutionPolicy::default(),
        )
        .unwrap()
        .unwrap();
        std::fs::rename(&destination_root, &held).unwrap();
        std::fs::rename(&foreign, &destination_root).unwrap();
        assert!(executor.execute(work).await.is_err());
        for root in [&held, &destination_root] {
            assert_eq!(std::fs::read(root.join("file")).unwrap(), b"old bytes");
            assert_eq!(
                std::fs::read_dir(root).unwrap().count(),
                1,
                "no leaked private staging"
            );
        }
        assert_eq!(
            std::fs::read(source_root.join("file")).unwrap(),
            b"new bytes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replaced_publication_or_root_cannot_authorize_source_removal() {
        use futures::TryStreamExt;
        for change_root in [false, true] {
            let retained = tempfile::tempdir().unwrap();
            let source_root = tempfile::tempdir().unwrap();
            let destination_root = tempfile::tempdir().unwrap();
            std::fs::write(source_root.path().join("input"), b"published bytes").unwrap();
            let authority =
                crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                    .await
                    .unwrap();
            let source = authority
                .entries(Default::default())
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .remove(0);
            let source_endpoint = authority.endpoint();
            let destination_endpoint =
                crate::endpoint::local::LocalEndpoint::new(destination_root.path().to_path_buf());
            let result = crate::endpoint::transfer::transfer_file(
                &source_endpoint,
                source.path.as_path(),
                &destination_endpoint,
                std::path::Path::new("renamed"),
                TransferOptions {
                    update: false,
                    verify: false,
                    follow_symlinks: false,
                    rate_limiter: None,
                    identity: crate::endpoint::transfer::TransferIdentity {
                        source: crate::endpoint::transfer::SourceExpectation::Scanned(
                            source.identity.unwrap(),
                        ),
                        destination: crate::endpoint::ExpectedDestination::Absent,
                    },
                    preservation: crate::endpoint::io::Preservation::default(),
                    preservation_request: crate::endpoint::io::PreservationRequest::default(),
                    final_flags: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
            assert_eq!(
                result.verification,
                crate::endpoint::io::VerificationStatus::NotRequested
            );
            let executor = LocalSyncExecutor::new(
                authority,
                destination_endpoint,
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_remove_source_files(true);
            if change_root {
                let rooted = executor.metadata_authority().await.unwrap();
                std::fs::rename(destination_root.path(), retained.path().join("held")).unwrap();
                std::fs::create_dir(destination_root.path()).unwrap();
                // The held leaf/proof is still valid. Only the local operator
                // address has changed; that alone must prohibit source unlink.
                result
                    .receipt
                    .revalidate_destination_blocking(&rooted)
                    .unwrap();
            } else {
                std::fs::rename(
                    destination_root.path().join("renamed"),
                    destination_root.path().join("saved"),
                )
                .unwrap();
            }
            std::fs::write(
                destination_root.path().join("renamed"),
                b"foreign replacement",
            )
            .unwrap();
            assert!(executor
                .remove_committed_source(&result.receipt, &source)
                .await
                .is_err());
            assert_eq!(
                std::fs::read(source_root.path().join("input")).unwrap(),
                b"published bytes"
            );
            assert_eq!(
                std::fs::read(destination_root.path().join("renamed")).unwrap(),
                b"foreign replacement"
            );
            if change_root {
                assert_eq!(
                    std::fs::read(retained.path().join("held/renamed")).unwrap(),
                    b"published bytes"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replaced_symlink_publication_keeps_source() {
        use futures::TryStreamExt;
        let source_root = tempfile::tempdir().unwrap();
        let destination_root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("target", source_root.path().join("input")).unwrap();
        let authority =
            crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                .await
                .unwrap();
        let source = authority
            .entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .remove(0);
        let executor = LocalSyncExecutor::new(
            authority,
            crate::endpoint::local::LocalEndpoint::new(destination_root.path().to_path_buf()),
            Scheduler::new(ResourceBudget::default()).unwrap(),
        )
        .with_remove_source_files(true);
        let publication = executor
            .replace_symlink(
                Path::new("target"),
                &rel("renamed"),
                crate::endpoint::ExpectedDestination::Absent,
                None,
            )
            .await
            .unwrap();
        let receipt = PublishedDestinationReceipt::for_symlink(
            source.path.clone(),
            rel("renamed"),
            source.identity,
            publication,
        );
        std::fs::remove_file(destination_root.path().join("renamed")).unwrap();
        std::os::unix::fs::symlink("foreign", destination_root.path().join("renamed")).unwrap();
        assert!(executor
            .remove_committed_source(&receipt, &source)
            .await
            .is_err());
        assert_eq!(
            std::fs::read_link(source_root.path().join("input")).unwrap(),
            Path::new("target")
        );
        assert_eq!(
            std::fs::read_link(destination_root.path().join("renamed")).unwrap(),
            Path::new("foreign")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unchanged_removal_reuses_the_executors_retirement_authority() {
        use futures::TryStreamExt;
        use xattr::FileExt;
        for foreign_edit in [false, true] {
            let source_root = tempfile::tempdir().unwrap();
            let destination_root = tempfile::tempdir().unwrap();
            std::fs::write(source_root.path().join("a"), b"new bytes").unwrap();
            std::fs::write(source_root.path().join("b"), b"old bytes").unwrap();
            std::fs::write(destination_root.path().join("a"), b"old bytes").unwrap();
            std::fs::hard_link(
                destination_root.path().join("a"),
                destination_root.path().join("b"),
            )
            .unwrap();
            let mut request = crate::engine::scan::ScanRequest::default();
            request.metadata.unix_mode = true;
            let authority =
                crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                    .await
                    .unwrap();
            let sources: Vec<Entry> = authority.entries(request).try_collect().await.unwrap();
            let destinations: Vec<Entry> = crate::endpoint::local_entry_scan::local_entry_stream(
                destination_root.path().to_path_buf(),
                request,
            )
            .try_collect()
            .await
            .unwrap();
            let executor = LocalSyncExecutor::new(
                authority,
                crate::endpoint::local::LocalEndpoint::new(destination_root.path().to_path_buf()),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_remove_source_files(true);
            let update = lower_local_op(
                SyncOp::Update {
                    source: sources[0].clone(),
                    destination: destinations[0].clone(),
                },
                ExecutionPolicy::default(),
            )
            .unwrap()
            .unwrap();
            executor.execute(update).await.unwrap();
            if foreign_edit {
                std::fs::File::open(destination_root.path().join("b"))
                    .unwrap()
                    .set_xattr("user.sy-foreign", b"foreign")
                    .unwrap();
            }
            let result = executor
                .remove_unchanged_source(&sources[1], &destinations[1], ExecutionPolicy::default())
                .await;
            if foreign_edit {
                assert!(result.is_err());
                assert_eq!(
                    std::fs::read(source_root.path().join("b")).unwrap(),
                    b"old bytes"
                );
            } else {
                result.unwrap();
                assert!(!source_root.path().join("b").exists());
            }
            assert_eq!(
                std::fs::read(destination_root.path().join("a")).unwrap(),
                b"new bytes"
            );
            assert_eq!(
                std::fs::read(destination_root.path().join("b")).unwrap(),
                b"old bytes"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hardlink_members_preserve_group_fidelity_and_observed_entries() {
        use futures::TryStreamExt;
        use std::os::unix::fs::MetadataExt;

        // Unchanged observations, source modification, observed-destination
        // replacement, and a foreign name arriving after an Absent preflight.
        for race in 0..4 {
            let source_root = tempfile::tempdir().unwrap();
            let dest_root = tempfile::tempdir().unwrap();
            std::fs::write(source_root.path().join("a"), b"new").unwrap();
            std::fs::hard_link(source_root.path().join("a"), source_root.path().join("b")).unwrap();
            if race != 3 {
                std::fs::write(dest_root.path().join("b"), b"old").unwrap();
            }
            let mut request = crate::engine::scan::ScanRequest::default();
            request.metadata.hardlink_group = true;
            request.metadata.unix_mode = true;
            let authority =
                crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                    .await
                    .unwrap();
            let entries: Vec<Entry> = authority.entries(request).try_collect().await.unwrap();
            let destination = crate::endpoint::local_entry_scan::local_entry_stream(
                dest_root.path().to_path_buf(),
                request,
            )
            .try_next()
            .await
            .unwrap();
            let executor = LocalSyncExecutor::new(
                authority,
                crate::endpoint::local::LocalEndpoint::new(dest_root.path().to_path_buf()),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_hardlinks(true);
            let first = lower_local_op(
                SyncOp::Create {
                    destination_path: entries[0].path.clone(),
                    source: entries[0].clone(),
                },
                ExecutionPolicy::default(),
            )
            .unwrap()
            .unwrap();
            executor.execute(first).await.unwrap();
            if race == 1 {
                std::fs::write(source_root.path().join("b"), b"bad").unwrap();
            } else if race >= 2 {
                std::fs::write(dest_root.path().join("b"), b"foreign").unwrap();
            }
            let op = match destination {
                Some(destination) => SyncOp::Update {
                    source: entries[1].clone(),
                    destination,
                },
                None => SyncOp::Create {
                    source: entries[1].clone(),
                    destination_path: entries[1].path.clone(),
                },
            };
            let member = lower_local_op(op, ExecutionPolicy::default())
                .unwrap()
                .unwrap();
            let result = executor.execute(member).await;
            assert_eq!(std::fs::read(dest_root.path().join("a")).unwrap(), b"new");
            if race != 0 {
                assert!(result.is_err());
                let expected: &[u8] = if race == 1 { b"old" } else { b"foreign" };
                assert_eq!(std::fs::read(dest_root.path().join("b")).unwrap(), expected);
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
            assert!(source_root.path().join("b").exists());
            assert_eq!(std::fs::read_dir(dest_root.path()).unwrap().count(), 2);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_hardlink_source_removal_failure_retains_group() {
        use futures::TryStreamExt;

        for replace_destination in [false, true] {
            let source_root = tempfile::tempdir().unwrap();
            let dest_root = tempfile::tempdir().unwrap();
            std::fs::write(source_root.path().join("a"), b"new").unwrap();
            std::fs::hard_link(source_root.path().join("a"), source_root.path().join("b")).unwrap();
            let mut request = crate::engine::scan::ScanRequest::default();
            request.metadata.hardlink_group = true;
            request.metadata.unix_mode = true;
            let authority =
                crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                    .await
                    .unwrap();
            let entries: Vec<Entry> = authority.entries(request).try_collect().await.unwrap();
            let executor = LocalSyncExecutor::new(
                authority,
                crate::endpoint::local::LocalEndpoint::new(dest_root.path().to_path_buf()),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_hardlinks(true)
            .with_remove_source_files(true);

            for source in entries {
                let work = lower_local_op(
                    SyncOp::Create {
                        destination_path: source.path.clone(),
                        source,
                    },
                    ExecutionPolicy::default(),
                )
                .unwrap()
                .unwrap();
                executor.execute(work).await.unwrap();
            }
            let raced_root = if replace_destination {
                dest_root.path()
            } else {
                source_root.path()
            };
            std::fs::remove_file(raced_root.join("b")).unwrap();
            std::fs::write(raced_root.join("b"), b"bad").unwrap();

            assert!(executor.finish_deferred_source_removals().await.is_err());
            assert!(source_root.path().join("a").exists());
            assert!(source_root.path().join("b").exists());
            assert_eq!(std::fs::read(raced_root.join("b")).unwrap(), b"bad");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verified_existing_hardlink_removal_waits_for_all_destination_proofs() {
        use futures::TryStreamExt;
        for race in [0, 1, 2] {
            let source_root = tempfile::tempdir().unwrap();
            let destination_root = tempfile::tempdir().unwrap();
            let foreign = tempfile::tempdir().unwrap();
            let retained = tempfile::tempdir().unwrap();
            for root in [source_root.path(), destination_root.path()] {
                std::fs::write(root.join("a"), b"same").unwrap();
                std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
            }
            if race == 2 {
                for name in ["a", "b"] {
                    std::fs::hard_link(
                        destination_root.path().join(name),
                        foreign.path().join(name),
                    )
                    .unwrap();
                }
            }
            let mut request = crate::engine::scan::ScanRequest::default();
            request.metadata.hardlink_group = true;
            let authority =
                crate::endpoint::source_root::SourceRoot::open(source_root.path().to_path_buf())
                    .await
                    .unwrap();
            let sources: Vec<Entry> = authority.entries(request).try_collect().await.unwrap();
            let destinations: Vec<Entry> = crate::endpoint::local_entry_scan::local_entry_stream(
                destination_root.path().to_path_buf(),
                request,
            )
            .try_collect()
            .await
            .unwrap();
            let executor = LocalSyncExecutor::new(
                authority,
                crate::endpoint::local::LocalEndpoint::new(destination_root.path().to_path_buf()),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_hardlinks(true)
            .with_remove_source_files(true);
            for (source, destination) in sources.iter().zip(&destinations) {
                executor
                    .remove_unchanged_source(source, destination, ExecutionPolicy::default())
                    .await
                    .unwrap();
                assert!(source_root.path().join("a").exists());
                assert!(source_root.path().join("b").exists());
            }
            if race == 1 {
                std::fs::remove_file(destination_root.path().join("b")).unwrap();
                std::fs::write(destination_root.path().join("b"), b"foreign").unwrap();
            } else if race == 2 {
                std::fs::rename(destination_root.path(), retained.path().join("held")).unwrap();
                std::fs::rename(foreign.path(), destination_root.path()).unwrap();
            }
            let result = executor.finish_deferred_source_removals().await;
            assert_eq!(result.is_err(), race != 0);
            assert_eq!(source_root.path().join("a").exists(), race != 0);
            assert_eq!(source_root.path().join("b").exists(), race != 0);
            if race == 2 {
                for name in ["a", "b"] {
                    assert_eq!(
                        std::fs::read(destination_root.path().join(name)).unwrap(),
                        b"same"
                    );
                    assert_eq!(
                        std::fs::read(retained.path().join("held").join(name)).unwrap(),
                        b"same"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_lowering_and_execution_preserve_racing_entries() {
        for update in [false, true] {
            for directory in [false, true] {
                let source_root = tempfile::TempDir::new().unwrap();
                let dest_root = tempfile::TempDir::new().unwrap();
                let path = dest_root.path().join("link");
                let source =
                    Entry::symlink(rel("link"), PathBuf::from("new"), Timestamp::UNIX_EPOCH);
                let op = if update {
                    std::os::unix::fs::symlink("old", &path).unwrap();
                    let mut destination =
                        Entry::symlink(rel("link"), PathBuf::from("old"), Timestamp::UNIX_EPOCH);
                    destination.identity = crate::endpoint::local_identity::metadata_identity(
                        &std::fs::symlink_metadata(&path).unwrap(),
                        EntryKind::Symlink,
                    );
                    SyncOp::Update {
                        source,
                        destination,
                    }
                } else {
                    SyncOp::Create {
                        destination_path: source.path.clone(),
                        source,
                    }
                };
                let work = lower_local_op(op, ExecutionPolicy::default())
                    .unwrap()
                    .unwrap();
                if update {
                    std::fs::remove_file(&path).unwrap();
                }
                if directory {
                    std::fs::create_dir(&path).unwrap();
                    std::fs::write(path.join("child"), b"raced").unwrap();
                } else {
                    std::fs::write(&path, b"raced").unwrap();
                }
                let executor = LocalSyncExecutor::new(
                    crate::endpoint::source_root::SourceRoot::open(
                        source_root.path().to_path_buf(),
                    )
                    .await
                    .unwrap(),
                    crate::endpoint::local::LocalEndpoint::new(dest_root.path().to_path_buf()),
                    Scheduler::new(ResourceBudget::default()).unwrap(),
                );
                assert!(executor.execute(work).await.is_err());
                let retained = if directory { path.join("child") } else { path };
                assert_eq!(std::fs::read(retained).unwrap(), b"raced");
                assert_eq!(std::fs::read_dir(dest_root.path()).unwrap().count(), 1);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_backup_rejects_changed_destination_without_touching_backup() {
        for symlink in [false, true] {
            let source = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let path = destination.path().join("file");
            let backup = destination.path().join("file~");
            std::fs::write(&path, b"scanned").unwrap();
            std::fs::write(&backup, b"previous backup").unwrap();
            let expected = crate::endpoint::local_identity::identity_for_metadata(
                &std::fs::symlink_metadata(&path).unwrap(),
            );
            let target = source.path().join("target");
            std::fs::write(&target, b"raced contents").unwrap();
            if symlink {
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(&target, &path).unwrap();
            } else {
                std::fs::write(&path, b"raced contents").unwrap();
            }
            let executor = LocalSyncExecutor::new(
                crate::endpoint::source_root::SourceRoot::open(source.path().to_path_buf())
                    .await
                    .unwrap(),
                crate::endpoint::local::LocalEndpoint::new(destination.path().to_path_buf()),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_backup(true, None, "~".into());
            assert!(executor
                .backup_replacement_file(&rel("file"), expected)
                .await
                .is_err());
            assert_eq!(std::fs::read(&backup).unwrap(), b"previous backup");
            assert_eq!(std::fs::read(&path).unwrap(), b"raced contents");
            assert_eq!(std::fs::read(&target).unwrap(), b"raced contents");
            if symlink {
                assert_eq!(std::fs::read_link(&path).unwrap(), target);
            }
        }
    }

    #[tokio::test]
    async fn local_execute_delete_validates_identity() {
        let temp_src = tempfile::TempDir::new().unwrap();
        let temp_dst = tempfile::TempDir::new().unwrap();
        let file_path = temp_dst.path().join("file");
        let dir_path = temp_dst.path().join("dir");
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

        let scheduler = Scheduler::new(ResourceBudget::default()).unwrap();
        let executor = LocalSyncExecutor::new(
            crate::endpoint::source_root::SourceRoot::open(temp_src.path().to_path_buf())
                .await
                .unwrap(),
            crate::endpoint::local::LocalEndpoint::new(temp_dst.path().to_path_buf()),
            scheduler,
        );

        // Vanished entry succeeds idempotently.
        executor
            .execute_delete(DeleteAction {
                path: rel("missing"),
                kind: EntryKind::File,
                identity: Some(file_id),
            })
            .await
            .unwrap();

        // Mismatched file identity fails and preserves the file.
        let wrong_id = EntryIdentity::from_bytes([123; 32]);
        let err = executor
            .execute_delete(DeleteAction {
                path: rel("file"),
                kind: EntryKind::File,
                identity: Some(wrong_id),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            LocalSyncError::Rooted(crate::rooted_fs::RootedFsError::DestinationChanged(_))
        ));
        assert!(file_path.exists());

        // Type mismatch fails.
        let err = executor
            .execute_delete(DeleteAction {
                path: rel("file"),
                kind: EntryKind::Directory,
                identity: Some(file_id),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            LocalSyncError::Rooted(crate::rooted_fs::RootedFsError::DestinationChanged(_))
        ));
        assert!(file_path.exists());

        // Matching file identity deletes successfully.
        executor
            .execute_delete(DeleteAction {
                path: rel("file"),
                kind: EntryKind::File,
                identity: Some(file_id),
            })
            .await
            .unwrap();
        assert!(!file_path.exists());

        // Mismatched directory identity fails and preserves the directory.
        let err = executor
            .execute_delete(DeleteAction {
                path: rel("dir"),
                kind: EntryKind::Directory,
                identity: Some(wrong_id),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            LocalSyncError::Rooted(crate::rooted_fs::RootedFsError::DestinationChanged(_))
        ));
        assert!(dir_path.exists());

        // Matching directory identity deletes successfully.
        executor
            .execute_delete(DeleteAction {
                path: rel("dir"),
                kind: EntryKind::Directory,
                identity: Some(dir_id),
            })
            .await
            .unwrap();
        assert!(!dir_path.exists());
    }
}
