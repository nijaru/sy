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
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath, Timestamp};
use crate::engine::hardlink_groups::{HardlinkGroups, HardlinkRepresentative};
use crate::engine::planner::ExecutionPolicy;
use crate::engine::scheduler::{ResourceRequest, Scheduler};
use crate::engine::work::WorkItem;
use crate::remote::acl::{apply_preserved_acls, read_preserved_acls, AclLocation, RemoteAclError};
use crate::remote::bsdflags::{apply_preserved_bsd_flags, BsdFlagsLocation, RemoteBsdFlagsError};
use crate::remote::xattr::{
    apply_preserved_xattrs, read_preserved_xattrs, RemoteXattrError, XattrLocation,
};
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

    #[error("unchanged regular-file BSD flag reconciliation requires an expected-identity finalization path and is not implemented for {0}")]
    UnchangedBsdFlags(PathBuf),

    #[error("--backup location for {0} is not representable")]
    InvalidBackupPath(PathBuf),

    #[error(transparent)]
    Rooted(#[from] crate::rooted_fs::RootedFsError),

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
    },
    TransferFile {
        source: Entry,
        destination: Option<Entry>,
        metadata: crate::endpoint::transfer::TransferMetadata,
        source_removal: bool,
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

pub type LocalTransferMetadata = crate::endpoint::transfer::TransferMetadata;

pub struct LocalSyncExecutor {
    source_root: PathBuf,
    destination_root: PathBuf,
    source_endpoint: crate::endpoint::local::LocalEndpoint,
    destination_endpoint: crate::endpoint::local::LocalEndpoint,
    metadata_authority: tokio::sync::OnceCell<crate::rooted_fs::RootedFs>,
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
    pub fn new(source_root: PathBuf, destination_root: PathBuf, scheduler: Scheduler) -> Self {
        let source_endpoint = crate::endpoint::local::LocalEndpoint::new(source_root.clone());
        let destination_endpoint =
            crate::endpoint::local::LocalEndpoint::new(destination_root.clone());
        Self {
            source_root,
            destination_root,
            source_endpoint,
            destination_endpoint,
            metadata_authority: tokio::sync::OnceCell::new(),
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

    fn source_path(&self, relative: &RelativePath) -> PathBuf {
        self.source_root.join(relative.as_path())
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
        let location = XattrLocation::Local(self.source_root.as_path());
        let xattrs = read_preserved_xattrs(&location, &source.path, source.kind).await?;
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
        let location = AclLocation::Local(self.source_root.as_path());
        let acl = read_preserved_acls(&location, &source.path, source.kind).await?;
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

    fn destination_path(&self, relative: &RelativePath) -> PathBuf {
        self.destination_root.join(relative.as_path())
    }

    fn lower_unchanged_file_preservation(
        &self,
        source: Entry,
        destination: Entry,
        policy: ExecutionPolicy,
        comparison: crate::engine::domain::ContentComparison,
    ) -> Result<Option<WorkItem<LocalSyncAction>>> {
        if !source.is_file() || !(self.xattrs || self.acls || self.bsd_flags) {
            return Ok(None);
        }
        if self.bsd_flags {
            return Err(LocalSyncError::UnchangedBsdFlags(
                source.path.as_path().to_path_buf(),
            ));
        }
        let mode = if policy.preserve_permissions {
            source.unix_mode
        } else {
            destination.unix_mode
        }
        .ok_or_else(|| LocalSyncError::MissingScannedMode(source.path.as_path().to_path_buf()))?;
        let modified = Some(if policy.preserve_times {
            source.modified
        } else {
            destination.modified
        });
        Ok(Some(file_work(LocalSyncAction::TransferFile {
            source,
            destination: Some(destination),
            metadata: LocalTransferMetadata {
                unix_mode: Some(mode),
                modified,
            },
            source_removal: comparison == crate::engine::domain::ContentComparison::Blake3,
        })))
    }

    /// Backup location for one destination-relative path, matching the
    /// other executors: beside the file (name + suffix) or under --backup-dir
    /// with the tree shape preserved (GNU rsync semantics). Always
    /// destination-anchored — never relative to the process CWD.
    fn backup_destination_for(&self, relative: &RelativePath) -> Result<PathBuf> {
        let file_name = relative
            .as_path()
            .file_name()
            .ok_or_else(|| LocalSyncError::InvalidBackupPath(relative.as_path().to_path_buf()))?
            .to_string_lossy()
            .into_owned();
        let backup_name = format!("{file_name}{}", self.backup_suffix);
        match &self.backup_dir {
            Some(dir) => {
                let mut backup = dir.join(relative.as_path());
                backup.set_file_name(backup_name);
                Ok(backup)
            }
            None => {
                let mut backup = self.destination_path(relative);
                backup.set_file_name(backup_name);
                Ok(backup)
            }
        }
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
        let rooted = crate::rooted_fs::RootedFs::open(self.destination_root.clone()).await?;
        let relative = relative.clone();
        tokio::task::spawn_blocking(move || {
            rooted.backup_file_blocking(&relative, &backup, expected)
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
        let rooted = crate::rooted_fs::RootedFs::open(self.destination_root.clone()).await?;
        let relative = relative.clone();
        tokio::task::spawn_blocking(move || {
            rooted.backup_file_blocking(&relative, &backup, expected)?;
            rooted.remove_blocking(&relative, false, Some(expected))
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
        let _permit = self.scheduler.acquire(resources).await?;

        match action {
            LocalSyncAction::CreateDirectory { source } => {
                let source_root =
                    crate::rooted_fs::RootedFs::open(self.source_root.clone()).await?;
                let expected = source.identity.ok_or_else(|| {
                    crate::rooted_fs::RootedFsError::DestinationChanged(
                        source.path.as_path().to_path_buf(),
                    )
                })?;
                let rooted = self.metadata_authority().await?.clone();
                let relative = source.path.clone();
                let identity = tokio::task::spawn_blocking(move || {
                    source_root.read_directory_preservation_blocking(
                        &relative,
                        expected,
                        Default::default(),
                    )?;
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
            LocalSyncAction::TransferFile {
                source,
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
                let pending_finalization = bsd_flags.is_some();
                let (transfer, mut receipt) = self
                    .transfer_source_file(
                        &source,
                        &destination,
                        &metadata,
                        crate::endpoint::io::PreservationRequest {
                            xattrs: self.xattrs && !source.is_symlink(),
                            acl: self.acls && !source.is_symlink(),
                        },
                        pending_finalization,
                    )
                    .await?;
                // Only rename-incompatible flags remain post-commit
                // finalization; xattrs/ACLs ride into staging and a failure
                // there aborts the replacement.
                if let Some(flags) = bsd_flags {
                    self.write_destination_bsd_flags(&source.path, source.kind, flags)
                        .await?;
                    receipt.mark_finalized();
                }
                if source_removal {
                    self.remove_committed_source(&receipt, &source).await?;
                }
                let op = if is_update {
                    crate::sync::output::ItemizeOp::Update
                } else {
                    crate::sync::output::ItemizeOp::Create
                };
                self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
                Ok(crate::engine::work::WorkResult::Transfer(transfer))
            }
            LocalSyncAction::ReplaceSymlink {
                source,
                destination,
                modified,
            } => {
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    LocalSyncError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
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
                self.replace_symlink(target, &source.path, expected, modified)
                    .await?;
                let receipt =
                    PublishedDestinationReceipt::for_symlink(source.path.clone(), source.identity);
                self.remove_committed_source(&receipt, &source).await?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Symlink,
                    &source.path,
                );
                Ok(crate::engine::work::WorkResult::Metadata)
            }
            LocalSyncAction::ApplyMetadata {
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

    /// One grouped file under `-H`: transfer the representative or link
    /// subsequent members to it. The linking member revalidates its scan
    /// identity first so a source replaced after the scan fails loudly
    /// instead of linking stale bytes. The group mutex serializes members.
    async fn execute_grouped_file(
        &self,
        source: Entry,
        destination: Option<Entry>,
        metadata: LocalTransferMetadata,
        group: [u8; 32],
        source_removal: bool,
    ) -> Result<crate::engine::work::TransferSummary> {
        let groups = self.hardlink_groups.lock().await;
        if let Some(first) = groups.get(group).await? {
            self.check_source_identity(&source).await?;
            let dest_abs = self.destination_path(&source.path);
            first.validate_metadata(metadata.unix_mode, metadata.modified)?;
            let is_update = destination.is_some();
            if let Some(existing) = destination
                .as_ref()
                .filter(|entry| self.backup && entry.is_file())
            {
                self.backup_replacement_file(&existing.path, existing.identity)
                    .await?;
            }
            let first_abs = self.destination_path(&first.path);
            link_local_file(&first_abs, &dest_abs)
                .await
                .map_err(|error| LocalSyncError::Destination(dest_abs.clone(), error))?;
            let receipt =
                PublishedDestinationReceipt::for_hardlink(source.path.clone(), source.identity);
            if source_removal {
                self.remove_committed_source(&receipt, &source).await?;
            }
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
        // BSD flags remain a post-commit phase; xattrs and ACLs are captured
        // from the held source that supplies the representative's bytes.
        let bsd_flags = self.read_source_bsd_flags(&source).await?;
        let pending_finalization = bsd_flags.is_some();
        let (transfer, mut receipt) = self
            .transfer_source_file(
                &source,
                &destination,
                &metadata,
                crate::endpoint::io::PreservationRequest {
                    xattrs: self.xattrs && !source.is_symlink(),
                    acl: self.acls && !source.is_symlink(),
                },
                pending_finalization,
            )
            .await?;
        // Only rename-incompatible flags remain post-commit finalization.
        if let Some(flags) = bsd_flags {
            self.write_destination_bsd_flags(&source.path, source.kind, flags)
                .await?;
            receipt.mark_finalized();
        }
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
        if source_removal {
            self.remove_committed_source(&receipt, &source).await?;
        }
        drop(groups);
        let op = if is_update {
            crate::sync::output::ItemizeOp::Update
        } else {
            crate::sync::output::ItemizeOp::Create
        };
        self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
        Ok(transfer)
    }

    /// Revalidate a linking member's scan identity (cheap stat, no bytes).
    async fn check_source_identity(&self, source: &Entry) -> Result<()> {
        let rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone()).await?;
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
        destination: &Option<Entry>,
        metadata: &crate::endpoint::transfer::TransferMetadata,
        preservation_request: crate::endpoint::io::PreservationRequest,
        pending_finalization: bool,
    ) -> Result<(
        crate::engine::work::TransferSummary,
        PublishedDestinationReceipt,
    )> {
        let backup = destination
            .as_ref()
            .filter(|entry| self.backup && entry.is_file());
        let result: TransferResult = crate::endpoint::transfer::transfer_file_with_before_stage(
            &self.source_endpoint,
            source.path.as_path(),
            &self.destination_endpoint,
            source.path.as_path(),
            TransferOptions {
                // A type replacement (file over symlink, link over file)
                // must not treat the mismatched destination as a delta
                // basis: staging replaces it wholesale.
                update: destination.is_some(),
                verify: self.verify_on_write,
                follow_symlinks: self.follow_symlinks,
                rate_limiter: self.rate_limiter.clone(),
                identity: crate::endpoint::transfer::TransferIdentity {
                    source: source
                        .identity
                        .map(crate::endpoint::transfer::SourceExpectation::Scanned)
                        .unwrap_or(crate::endpoint::transfer::SourceExpectation::Unverified),
                    destination: match destination {
                        Some(destination) => destination
                            .identity
                            .map(crate::endpoint::io::ExpectedDestination::Unchanged)
                            .unwrap_or(crate::endpoint::io::ExpectedDestination::Unverified),
                        None => crate::endpoint::io::ExpectedDestination::Absent,
                    },
                },
                preservation: crate::endpoint::io::Preservation::default(),
                preservation_request,
                pending_finalization,
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
                LocalSyncError::Destination(self.destination_path(&source.path), io)
            }
            other => LocalSyncError::Destination(
                self.destination_path(&source.path),
                std::io::Error::other(other.to_string()),
            ),
        })?;
        // Fail-fast on staged verification mismatch: the previous
        // destination is intact (the transfer layer aborted staging), and
        // the sync surfaces the failure instead of tolerating it.
        if let crate::endpoint::io::VerificationStatus::Failed { expected, actual } =
            result.verification
        {
            return Err(LocalSyncError::VerificationFailed {
                path: self.destination_path(&source.path),
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
    ) -> Result<()> {
        use crate::endpoint::Endpoint;
        self.destination_endpoint
            .replace_symlink(target, relative.as_path(), expected, modified)
            .await
            .map_err(|error| match error {
                crate::error::SyncError::Io(io) => {
                    LocalSyncError::Destination(self.destination_path(relative), io)
                }
                other => LocalSyncError::Destination(
                    self.destination_path(relative),
                    std::io::Error::other(other.to_string()),
                ),
            })
    }

    async fn metadata_authority(&self) -> Result<&crate::rooted_fs::RootedFs> {
        self.metadata_authority
            .get_or_try_init(|| crate::rooted_fs::RootedFs::open(self.destination_root.clone()))
            .await
            .map_err(Into::into)
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
        self.remove_source_entry_on_disk(source).await
    }

    /// Revalidate and unlink through a held root; never follow a raced ancestor.
    async fn remove_source_entry_on_disk(&self, source: &Entry) -> Result<()> {
        let rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone())
            .await
            .map_err(ExistingDestinationError::from)?;
        existing::remove_observed_source(rooted, source.clone()).await?;
        Ok(())
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
            let rooted = crate::rooted_fs::RootedFs::open(self.destination_root.clone()).await?;
            let action = action.clone();
            let result = tokio::task::spawn_blocking(move || {
                match rooted.path_identity_blocking(&action.path)? {
                    None => return Ok(()),
                    Some(observation) if observation != (action.kind, expected) => {
                        return Err(crate::rooted_fs::RootedFsError::DestinationChanged(
                            action.path.as_path().to_path_buf(),
                        ));
                    }
                    Some(_) => {}
                }
                rooted.remove_blocking(
                    &action.path,
                    action.kind == EntryKind::Directory,
                    Some(expected),
                )
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
        let rooted = self.metadata_authority().await?.clone();
        let source = crate::rooted_fs::RootedFs::open(self.source_root.clone()).await?;
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
            let preservation = if metadata.preserve_source {
                source.read_directory_preservation_blocking(
                    &metadata.path,
                    metadata.source_identity,
                    request,
                )?
            } else {
                Default::default()
            };
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
        let source_rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone())
            .await
            .map_err(ExistingDestinationError::from)?;
        let destination_rooted = crate::rooted_fs::RootedFs::open(self.destination_root.clone())
            .await
            .map_err(ExistingDestinationError::from)?;
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
            comparison,
        } = op
        {
            return self.lower_unchanged_file_preservation(source, destination, policy, comparison);
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

    async fn remove_unchanged_source(
        &self,
        source: &Entry,
        destination: &Entry,
        policy: ExecutionPolicy,
    ) -> std::result::Result<(), LocalSyncError> {
        LocalSyncExecutor::remove_unchanged_source(self, source, destination, policy).await
    }
}

/// Atomically link `dest` to the existing `first` inode (`-H`): stage the
/// link under a temporary name, then rename over the destination.
async fn link_local_file(first: &Path, dest: &Path) -> std::result::Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let temp = crate::temp_file::TempFileGuard::temp_path_for(dest);
        let guard = crate::temp_file::TempFileGuard::new(&temp);
        tokio::fs::hard_link(first, &temp).await?;
        tokio::fs::rename(&temp, dest).await?;
        guard.defuse();
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (first, dest);
        Err(std::io::Error::other(
            "hardlink preservation is not supported on this platform",
        ))
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
        SyncOp::Create { source } => lower_create(source, policy),
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
    policy: ExecutionPolicy,
) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
    match source.kind {
        EntryKind::Directory => Ok(Some(mutation_work(LocalSyncAction::CreateDirectory {
            source,
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
                modified: Some(source.modified),
            };
            Ok(Some(file_work(LocalSyncAction::TransferFile {
                source,
                destination: None,
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Symlink => Ok(Some(mutation_work(LocalSyncAction::ReplaceSymlink {
            modified: policy.preserve_times.then_some(source.modified),
            source,
            destination: None,
        }))),
    }
}

fn lower_update(
    source: Entry,
    destination: Entry,
    policy: ExecutionPolicy,
) -> std::result::Result<Option<WorkItem<LocalSyncAction>>, LocalSyncError> {
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
                modified: Some(source.modified),
            };
            Ok(Some(file_work(LocalSyncAction::TransferFile {
                source,
                destination: Some(destination),
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Directory => lower_metadata(source, destination, policy),
        EntryKind::Symlink => Ok(Some(mutation_work(LocalSyncAction::ReplaceSymlink {
            modified: policy.preserve_times.then_some(source.modified),
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
                modified: Some(source.modified),
            };
            Ok(Some(file_work(LocalSyncAction::TransferFile {
                source,
                destination: Some(destination),
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Symlink => Ok(Some(mutation_work(LocalSyncAction::ReplaceSymlink {
            modified: policy.preserve_times.then_some(source.modified),
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
    let expected_destination = destination.identity.ok_or_else(|| {
        LocalSyncError::MissingDestinationIdentity(destination.path.as_path().to_path_buf())
    })?;
    Ok(Some(metadata_work(
        source,
        expected_destination,
        unix_mode,
        modified,
    )))
}

fn requested_metadata(
    source: &Entry,
    destination: &Entry,
    policy: ExecutionPolicy,
) -> std::result::Result<RequestedMetadata, LocalSyncError> {
    let unix_mode = if policy.preserve_permissions && destination.unix_mode != source.unix_mode {
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
    expected_destination: EntryIdentity,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
) -> WorkItem<LocalSyncAction> {
    mutation_work(LocalSyncAction::ApplyMetadata {
        source,
        expected_destination,
        unix_mode,
        modified,
    })
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
    async fn hardlink_members_preserve_group_fidelity_and_observed_entries() {
        use futures::TryStreamExt;
        use std::os::unix::fs::MetadataExt;

        for race in [false, true] {
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
            let executor = LocalSyncExecutor::new(
                source_root.path().to_path_buf(),
                dest_root.path().to_path_buf(),
                Scheduler::new(ResourceBudget::default()).unwrap(),
            )
            .with_hardlinks(true);
            let first = lower_local_op(
                SyncOp::Create {
                    source: entries[0].clone(),
                },
                ExecutionPolicy::default(),
            )
            .unwrap()
            .unwrap();
            executor.execute(first).await.unwrap();
            if race {
                std::fs::write(source_root.path().join("b"), b"bad").unwrap();
            }
            let member = lower_local_op(
                SyncOp::Update {
                    source: entries[1].clone(),
                    destination,
                },
                ExecutionPolicy::default(),
            )
            .unwrap()
            .unwrap();
            let result = executor.execute(member).await;
            assert_eq!(std::fs::read(dest_root.path().join("a")).unwrap(), b"new");
            if race {
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
            assert!(source_root.path().join("b").exists());
            assert_eq!(std::fs::read_dir(dest_root.path()).unwrap().count(), 2);
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
                    SyncOp::Create { source }
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
                    source_root.path().to_path_buf(),
                    dest_root.path().to_path_buf(),
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
                source.path().to_path_buf(),
                destination.path().to_path_buf(),
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
            temp_src.path().to_path_buf(),
            temp_dst.path().to_path_buf(),
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
