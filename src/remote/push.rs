use crate::endpoint::existing::{self, ExistingDestinationError, FingerprintOptions};
use crate::endpoint::receipt::PublishedDestinationReceipt;
use crate::endpoint::Capabilities;
use crate::engine::compression::CompressionPolicy;
use crate::engine::delete_plan::DeleteAction;
use crate::engine::domain::{Entry, EntryIdentity, EntryKind, RelativePath, SyncOp, Timestamp};
use crate::engine::finalize_journal::FinalizeMetadata;
use crate::engine::hardlink_groups::{HardlinkGroups, HardlinkRepresentative};
use crate::engine::planner::ExecutionPolicy;
use crate::engine::scheduler::{ResourceRequest, Scheduler, SchedulerError};
use crate::engine::work::TransferSummary;
use crate::engine::work::WorkItem;
use crate::remote::acl::{apply_preserved_acls, read_preserved_acls, AclLocation, RemoteAclError};
use crate::remote::bsdflags::{apply_preserved_bsd_flags, BsdFlagsLocation, RemoteBsdFlagsError};
use crate::remote::runtime::{ClientRemoteHandle, RemoteSessionError};
use crate::remote::transfer::{
    TransferDestination, TransferMetadata, TransferPreservationRequest, TransferStreamPolicy,
};
use crate::remote::xattr::{
    apply_preserved_xattrs, read_preserved_xattrs, RemoteXattrError, XattrLocation,
};
use crate::transfer::delta::BasisIndexLimits;
use std::ffi::OsString;
use std::path::PathBuf;

/// Existing v2 uses 10 MiB as the point where rolling-delta setup starts to
/// repay its extra destination read and signature round trip. Keep that as the
/// initial v3 policy boundary until dedicated v3 benchmarks tune it.
pub const DEFAULT_REMOTE_DELTA_MIN_SIZE: u64 = 10 * 1024 * 1024;

/// Per-file reservation for the bounded producer/signature path. This is a
/// working-set budget, not the logical file size. The transfer protocol has its
/// own global router byte budget in addition to this scheduler admission.
pub const REMOTE_FILE_WORKING_SET: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemotePushAction {
    CreateDirectory {
        source: Entry,
    },
    TransferFile {
        source: Entry,
        destination: Option<Entry>,
        metadata: TransferMetadata,
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

#[derive(Debug, thiserror::Error)]
pub enum RemotePushLowerError {
    #[error("regular-file commit requires Unix mode metadata for {0}")]
    MissingFileMode(PathBuf),

    #[error("metadata preservation requires Unix mode metadata for {0}")]
    MissingPreservedMode(PathBuf),

    #[error("transactional type replacement is not implemented for directory transition at {0}")]
    TransactionalDirectoryReplace(PathBuf),

    #[error("metadata mutation requires scanned destination identity for {0}")]
    MissingDestinationIdentity(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum RemotePushError {
    #[error(transparent)]
    Directory(#[from] crate::rooted_fs::RootedFsError),
    #[error("hardlink bookkeeping failed: {0}")]
    HardlinkState(#[from] std::io::Error),
    #[error(transparent)]
    Existing(#[from] ExistingDestinationError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),

    #[error(transparent)]
    Remote(#[from] RemoteSessionError),

    #[error("symlink action is missing the scanned target for {0}")]
    MissingSymlinkTarget(PathBuf),

    #[error("scanned destination identity is required to commit a remote update: {0}")]
    MissingDestinationIdentity(PathBuf),

    #[error("failed to remove committed source {0}: {1}")]
    SourceRemoval(PathBuf, std::io::Error),

    #[error("--backup location for {0} is not representable beneath the destination root")]
    InvalidBackupPath(PathBuf),

    #[error(transparent)]
    Lower(#[from] RemotePushLowerError),

    #[error(transparent)]
    Xattr(#[from] RemoteXattrError),

    #[error(transparent)]
    Acl(#[from] RemoteAclError),

    #[error(transparent)]
    BsdFlags(#[from] RemoteBsdFlagsError),
}

pub type LowerResult<T> = std::result::Result<T, RemotePushLowerError>;
pub type Result<T> = std::result::Result<T, RemotePushError>;

/// Lower one semantic planner operation into concrete remote push work.
///
/// Byte strategy is deliberately absent here. A changed destination file stays
/// attached to `TransferFile`; the executor requests rolling signatures only if
/// the negotiated peer capabilities and workload size make delta plausible.
pub fn lower_sync_op(
    op: SyncOp,
    policy: ExecutionPolicy,
) -> LowerResult<Option<WorkItem<RemotePushAction>>> {
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

fn lower_create(
    source: Entry,
    policy: ExecutionPolicy,
) -> LowerResult<Option<WorkItem<RemotePushAction>>> {
    match source.kind {
        EntryKind::Directory => Ok(Some(mutation_work(RemotePushAction::CreateDirectory {
            source,
        }))),
        EntryKind::File => {
            // Staging stays private at 0600. A new committed file therefore
            // always needs an explicit sane final mode even when -p is absent.
            // Source mode matches sy 0.4's create behavior; -p additionally
            // controls metadata-only reconciliation of already-equal files.
            let mode = source.unix_mode.ok_or_else(|| {
                RemotePushLowerError::MissingFileMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = TransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
                xattrs: None,
                acls: None,
            };
            Ok(Some(file_work(RemotePushAction::TransferFile {
                source,
                destination: None,
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Symlink => Ok(Some(mutation_work(RemotePushAction::ReplaceSymlink {
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
) -> LowerResult<Option<WorkItem<RemotePushAction>>> {
    match source.kind {
        EntryKind::File => {
            let mode = if policy.preserve_permissions {
                source.unix_mode
            } else {
                destination.unix_mode
            }
            .ok_or_else(|| {
                RemotePushLowerError::MissingFileMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = TransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
                xattrs: None,
                acls: None,
            };
            Ok(Some(file_work(RemotePushAction::TransferFile {
                source,
                destination: Some(destination),
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Directory => lower_metadata(source, destination, policy),
        EntryKind::Symlink => Ok(Some(mutation_work(RemotePushAction::ReplaceSymlink {
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
) -> LowerResult<Option<WorkItem<RemotePushAction>>> {
    match source.kind {
        EntryKind::Directory => Err(RemotePushLowerError::TransactionalDirectoryReplace(
            source.path.as_path().to_path_buf(),
        )),
        EntryKind::File => {
            let mode = source.unix_mode.ok_or_else(|| {
                RemotePushLowerError::MissingFileMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = TransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
                xattrs: None,
                acls: None,
            };
            Ok(Some(file_work(RemotePushAction::TransferFile {
                source,
                // Keep the namespace precondition even without a regular-file basis.
                destination: Some(destination),
                metadata,
                source_removal: true,
            })))
        }
        EntryKind::Symlink => Ok(Some(mutation_work(RemotePushAction::ReplaceSymlink {
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
) -> LowerResult<Option<WorkItem<RemotePushAction>>> {
    if source.is_directory() {
        return Ok(None);
    }
    let Some((unix_mode, modified)) = requested_metadata(&source, &destination, policy)? else {
        return Ok(None);
    };
    let expected_destination = destination.identity.ok_or_else(|| {
        RemotePushLowerError::MissingDestinationIdentity(destination.path.as_path().to_path_buf())
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
) -> LowerResult<Option<(Option<u32>, Option<Timestamp>)>> {
    let unix_mode = if policy.preserve_permissions && destination.unix_mode != source.unix_mode {
        Some(source.unix_mode.ok_or_else(|| {
            RemotePushLowerError::MissingPreservedMode(source.path.as_path().to_path_buf())
        })?)
    } else {
        None
    };
    let modified = if policy.preserve_times && destination.modified != source.modified {
        Some(source.modified)
    } else {
        None
    };

    Ok((unix_mode.is_some() || modified.is_some()).then_some((unix_mode, modified)))
}

fn file_work(action: RemotePushAction) -> WorkItem<RemotePushAction> {
    WorkItem::new(
        action,
        ResourceRequest {
            active_files: 1,
            buffered_bytes: REMOTE_FILE_WORKING_SET,
            metadata_ops: 0,
            cpu_tasks: 1,
            network_writes: 1,
        },
    )
}

fn mutation_work(action: RemotePushAction) -> WorkItem<RemotePushAction> {
    WorkItem::new(
        action,
        ResourceRequest {
            active_files: 0,
            buffered_bytes: 0,
            metadata_ops: 1,
            cpu_tasks: 0,
            network_writes: 1,
        },
    )
}

fn metadata_work(
    source: Entry,
    expected_destination: EntryIdentity,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
) -> WorkItem<RemotePushAction> {
    mutation_work(RemotePushAction::ApplyMetadata {
        source,
        expected_destination,
        unix_mode,
        modified,
    })
}

/// Executes already-lowered v3 push work. The caller owns tree ordering,
/// deletion commit, and finalize replay; this type owns per-item admission and
/// transfer-strategy selection.
/// `--backup` plan for the v3 push executor. All paths stay relative to the
/// destination root: the server resolves them beneath its pinned root, so an
/// absolute backup directory is rejected at session setup, not silently
/// relocated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBackupPlan {
    pub suffix: String,
    /// Optional `--backup-dir` relative to the destination root.
    pub dir: Option<crate::engine::domain::RelativePath>,
}

impl RemoteBackupPlan {
    /// Backup location for one destination path (root-relative).
    pub(crate) fn destination(
        &self,
        path: &crate::engine::domain::RelativePath,
    ) -> Option<crate::engine::domain::RelativePath> {
        let mut backup = match &self.dir {
            Some(dir) => dir.as_path().join(path.as_path()),
            None => path.as_path().to_path_buf(),
        };
        let mut name = path.as_path().file_name()?.to_os_string();
        name.push(&self.suffix);
        backup.set_file_name(name);
        crate::engine::domain::RelativePath::new(backup).ok()
    }
}

pub struct RemotePushExecutor {
    source_root: PathBuf,
    remote: ClientRemoteHandle,
    scheduler: Scheduler,
    delta_limits: BasisIndexLimits,
    delta_min_size: u64,
    /// --remove-source-files: delete the source entry after its destination
    /// commit is acknowledged (v3 commits only after BLAKE3 verification).
    remove_source_files: bool,
    /// --backup: preserve replaced and deleted destination files via
    /// server-side copies before the mutation.
    backup: Option<RemoteBackupPlan>,
    /// -z/--compress: chunk compression policy for file transfers.
    compression: Option<CompressionPolicy>,
    /// -H/--preserve-hardlinks: exact scratch state retains each group's
    /// committed representative and inode metadata. Grouped work serializes
    /// through publication; ungrouped files stay concurrent. Skipped entries
    /// do not establish representatives.
    hardlinks: bool,
    hardlink_groups: tokio::sync::Mutex<HardlinkGroups>,
    hardlink_removals: crate::engine::hardlink_removals::HardlinkRemovalJournal,
    /// -X/--preserve-xattrs: mirror the local source's extended attributes
    /// onto the remote destination for every entry the executor creates or
    /// updates. Symlinks are skipped (their attributes are not portable).
    xattrs: bool,
    /// -A/--preserve-acls: mirror the local source's access-control list
    /// onto the remote destination for every entry the executor creates or
    /// updates. Symlinks are skipped, like xattrs.
    acls: bool,
    /// -F/--preserve-flags (macOS only): mirror the source's BSD file flags
    /// onto the remote destination for every entry the executor creates or
    /// updates. Symlinks are skipped, like xattrs.
    bsd_flags: bool,
    /// Per-operation output (`-i`/`--json`). `None` prints nothing.
    reporter: Option<std::sync::Arc<crate::sync::output::SyncReporter>>,
}

impl crate::engine::controller::SyncPlanExecutor for RemotePushExecutor {
    type Action = RemotePushAction;
    type Error = RemotePushError;

    fn lower(
        &self,
        op: crate::engine::domain::SyncOp,
        policy: crate::engine::planner::ExecutionPolicy,
    ) -> std::result::Result<Option<WorkItem<RemotePushAction>>, RemotePushError> {
        if let crate::engine::domain::SyncOp::Unchanged {
            source,
            destination,
            comparison,
        } = op
        {
            return self.lower_unchanged_file_preservation(source, destination, policy, comparison);
        }
        lower_sync_op(op, policy).map_err(RemotePushError::from)
    }

    fn is_directory_action(&self, action: &RemotePushAction) -> bool {
        matches!(action, RemotePushAction::CreateDirectory { .. })
    }

    async fn execute(
        &self,
        item: crate::engine::work::WorkItem<RemotePushAction>,
    ) -> std::result::Result<crate::engine::work::WorkResult, RemotePushError> {
        RemotePushExecutor::execute(self, item).await
    }

    async fn execute_delete(
        &self,
        action: crate::engine::delete_plan::DeleteAction,
    ) -> std::result::Result<(), RemotePushError> {
        RemotePushExecutor::execute_delete(self, action).await
    }

    async fn execute_finalize(
        &self,
        metadata: crate::engine::finalize_journal::FinalizeMetadata,
    ) -> std::result::Result<(), RemotePushError> {
        RemotePushExecutor::execute_finalize(self, metadata).await
    }

    async fn finish_deferred_source_removals(&self) -> std::result::Result<(), RemotePushError> {
        RemotePushExecutor::finish_deferred_source_removals(self).await
    }

    async fn remove_unchanged_source(
        &self,
        source: &crate::engine::domain::Entry,
        destination: &crate::engine::domain::Entry,
        policy: ExecutionPolicy,
    ) -> std::result::Result<(), RemotePushError> {
        RemotePushExecutor::remove_unchanged_source(self, source, destination, policy).await
    }
}

impl RemotePushExecutor {
    pub fn new(
        source_root: PathBuf,
        remote: ClientRemoteHandle,
        scheduler: Scheduler,
        delta_limits: BasisIndexLimits,
    ) -> Self {
        Self {
            source_root,
            remote,
            scheduler,
            delta_limits,
            delta_min_size: DEFAULT_REMOTE_DELTA_MIN_SIZE,
            remove_source_files: false,
            backup: None,
            compression: None,
            hardlinks: false,
            hardlink_groups: tokio::sync::Mutex::new(HardlinkGroups::default()),
            hardlink_removals: crate::engine::hardlink_removals::HardlinkRemovalJournal::default(),
            xattrs: false,
            acls: false,
            bsd_flags: false,
            reporter: None,
        }
    }

    pub const fn with_delta_min_size(mut self, bytes: u64) -> Self {
        self.delta_min_size = bytes;
        self
    }

    pub const fn with_remove_source_files(mut self, enabled: bool) -> Self {
        self.remove_source_files = enabled;
        self
    }

    pub const fn with_compression(mut self, policy: Option<CompressionPolicy>) -> Self {
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

    pub fn with_reporter(
        mut self,
        reporter: Option<std::sync::Arc<crate::sync::output::SyncReporter>>,
    ) -> Self {
        self.reporter = reporter;
        self
    }

    fn report(
        &self,
        op: crate::sync::output::ItemizeOp,
        kind: crate::sync::output::ItemizeKind,
        path: &crate::engine::domain::RelativePath,
    ) {
        if let Some(reporter) = &self.reporter {
            reporter.operation(op, kind, path.as_path());
        }
    }

    pub fn with_backup(mut self, plan: Option<RemoteBackupPlan>) -> Self {
        self.backup = plan;
        self
    }

    fn lower_unchanged_file_preservation(
        &self,
        source: Entry,
        destination: Entry,
        policy: ExecutionPolicy,
        comparison: crate::engine::domain::ContentComparison,
    ) -> Result<Option<WorkItem<RemotePushAction>>> {
        if !source.is_file() || !(self.xattrs || self.acls || self.bsd_flags) {
            return Ok(None);
        }
        let mode = if policy.preserve_permissions {
            source.unix_mode
        } else {
            destination.unix_mode
        }
        .ok_or_else(|| {
            RemotePushLowerError::MissingFileMode(source.path.as_path().to_path_buf())
        })?;
        let metadata = TransferMetadata {
            unix_mode: Some(mode),
            modified: Some(if policy.preserve_times {
                source.modified
            } else {
                destination.modified
            }),
            xattrs: None,
            acls: None,
        };
        Ok(Some(file_work(RemotePushAction::TransferFile {
            source,
            destination: Some(destination),
            metadata,
            source_removal: comparison == crate::engine::domain::ContentComparison::Blake3,
        })))
    }

    pub async fn execute(
        &self,
        item: WorkItem<RemotePushAction>,
    ) -> Result<crate::engine::work::WorkResult> {
        let (action, resources) = item.into_parts();
        let _permit = self.scheduler.acquire(resources).await?;

        match action {
            RemotePushAction::CreateDirectory { source } => {
                let rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone()).await?;
                let expected = source.identity.ok_or_else(|| {
                    crate::rooted_fs::RootedFsError::DestinationChanged(
                        source.path.as_path().to_path_buf(),
                    )
                })?;
                let path = source.path.clone();
                tokio::task::spawn_blocking(move || {
                    rooted.read_directory_preservation_blocking(&path, expected, Default::default())
                })
                .await
                .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??;
                let identity = self.remote.create_directory(&source.path).await?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Directory,
                    &source.path,
                );
                Ok(crate::engine::work::WorkResult::DirectoryPrepared(identity))
            }
            RemotePushAction::TransferFile {
                source,
                destination,
                metadata,
                source_removal,
            } => {
                // -H/--preserve-hardlinks: members of one scanned group
                // share a single transferred representative; the rest become
                // server-side links to it. The group mutex is held across a
                // grouped transfer so members serialize (ungrouped files
                // stay concurrent); the legacy executor was fully serial
                // here, so this is strictly more concurrent.
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
                // --backup preserves the replaced destination file first. The
                // server copies it beneath the pinned root before the staged
                // replacement commits; a backup failure aborts the transfer
                // so the user's copy cannot be silently skipped.
                if let (Some(plan), Some(existing)) = (&self.backup, &destination) {
                    if existing.is_file() {
                        let backup_path = plan.destination(&existing.path).ok_or_else(|| {
                            RemotePushError::InvalidBackupPath(
                                existing.path.as_path().to_path_buf(),
                            )
                        })?;
                        self.copy_backup(&existing.path, &backup_path, existing.identity)
                            .await?;
                    }
                }
                let destination_transfer = self.prepare_destination(destination).await?;
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                // The requester captures xattrs/ACLs from the same held file
                // descriptor that produces transfer bytes, then sends them on
                // the transfer stream into private staging.
                let stream_policy = TransferStreamPolicy {
                    preservation: TransferPreservationRequest {
                        xattrs: self.xattrs,
                        acls: self.acls,
                    },
                    compression: self.compression,
                    final_flags: bsd_flags,
                };
                let (summary, publication) = self
                    .remote
                    .transfer_file_with_stream_policy(
                        self.source_root.clone(),
                        source.clone(),
                        destination_transfer,
                        metadata,
                        stream_policy,
                    )
                    .await?;
                let receipt = PublishedDestinationReceipt::for_file(
                    source.path.clone(),
                    source.path.clone(),
                    source.identity,
                    &crate::endpoint::io::VerificationStatus::Verified,
                    publication,
                );
                if source_removal {
                    self.remove_committed_source(&receipt, &source).await?;
                }
                let op = if is_update {
                    crate::sync::output::ItemizeOp::Update
                } else {
                    crate::sync::output::ItemizeOp::Create
                };
                self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
                Ok(crate::engine::work::WorkResult::Transfer(summary))
            }
            RemotePushAction::ReplaceSymlink {
                source,
                destination,
                modified,
            } => {
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    RemotePushError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
                let expected_identity = destination
                    .map(|destination| {
                        destination.identity.ok_or_else(|| {
                            RemotePushError::MissingDestinationIdentity(
                                destination.path.as_path().to_path_buf(),
                            )
                        })
                    })
                    .transpose()?;
                self.remote
                    .replace_symlink(&source.path, target, expected_identity, modified)
                    .await?;
                let receipt = PublishedDestinationReceipt::for_symlink(
                    source.path.clone(),
                    source.path.clone(),
                    source.identity,
                );
                self.remove_committed_source(&receipt, &source).await?;
                self.report(
                    crate::sync::output::ItemizeOp::Create,
                    crate::sync::output::ItemizeKind::Symlink,
                    &source.path,
                );
                Ok(crate::engine::work::WorkResult::Metadata)
            }
            RemotePushAction::ApplyMetadata {
                source,
                expected_destination,
                unix_mode,
                modified,
            } => {
                let xattrs = self.read_source_xattrs(&source).await?;
                let acls = self.read_source_acls(&source).await?;
                let bsd_flags = self.read_source_bsd_flags(&source).await?;
                self.remote
                    .apply_metadata(
                        &source.path,
                        source.kind,
                        expected_destination,
                        unix_mode,
                        modified,
                    )
                    .await?;
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

    /// One grouped regular file under `-H`: transfer the representative or
    /// link subsequent members to it. The caller holds no locks; this method
    /// owns group serialization. A linking member revalidates its scan
    /// identity first so a source replaced after the scan fails loudly
    /// instead of linking stale bytes.
    async fn execute_grouped_file(
        &self,
        source: Entry,
        destination: Option<Entry>,
        metadata: TransferMetadata,
        group: [u8; 32],
        source_removal: bool,
    ) -> Result<TransferSummary> {
        let groups = self.hardlink_groups.lock().await;
        if let Some(first) = groups.get(group).await? {
            self.check_source_identity(&source).await?;
            first.validate_metadata(metadata.unix_mode, metadata.modified)?;
            if let (Some(plan), Some(existing)) = (&self.backup, &destination) {
                if existing.is_file() {
                    let backup_path = plan.destination(&existing.path).ok_or_else(|| {
                        RemotePushError::InvalidBackupPath(existing.path.as_path().to_path_buf())
                    })?;
                    self.copy_backup(&existing.path, &backup_path, existing.identity)
                        .await?;
                }
            }
            self.remote.hardlink(&first.path, &source.path).await?;
            let receipt = PublishedDestinationReceipt::for_hardlink(
                source.path.clone(),
                source.path.clone(),
                source.identity,
            );
            if source_removal {
                self.defer_grouped_source_removal(group, &receipt, &source)
                    .await?;
            }
            let op = if destination.is_some() {
                crate::sync::output::ItemizeOp::Update
            } else {
                crate::sync::output::ItemizeOp::Create
            };
            self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
            return Ok(TransferSummary {
                file_size: source.size,
                digest: [0_u8; 32],
                literal_bytes: 0,
                reused_bytes: source.size,
            });
        }
        let is_update = destination.is_some();
        if let (Some(plan), Some(existing)) = (&self.backup, &destination) {
            if existing.is_file() {
                let backup_path = plan.destination(&existing.path).ok_or_else(|| {
                    RemotePushError::InvalidBackupPath(existing.path.as_path().to_path_buf())
                })?;
                self.copy_backup(&existing.path, &backup_path, existing.identity)
                    .await?;
            }
        }
        let destination_transfer = self.prepare_destination(destination).await?;
        let bsd_flags = self.read_source_bsd_flags(&source).await?;
        // The requester captures xattrs/ACLs from the same held file
        // descriptor that produces transfer bytes.
        let stream_policy = TransferStreamPolicy {
            preservation: TransferPreservationRequest {
                xattrs: self.xattrs,
                acls: self.acls,
            },
            compression: self.compression,
            final_flags: bsd_flags,
        };
        let representative = HardlinkRepresentative {
            path: source.path.clone(),
            unix_mode: metadata.unix_mode,
            modified: metadata.modified,
        };
        let (summary, publication) = self
            .remote
            .transfer_file_with_stream_policy(
                self.source_root.clone(),
                source.clone(),
                destination_transfer,
                metadata,
                stream_policy,
            )
            .await?;
        let receipt = PublishedDestinationReceipt::for_file(
            source.path.clone(),
            source.path.clone(),
            source.identity,
            &crate::endpoint::io::VerificationStatus::Verified,
            publication,
        );
        groups.insert(group, representative).await?;
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
        self.report(op, crate::sync::output::ItemizeKind::File, &source.path);
        Ok(summary)
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
            RemotePushError::SourceRemoval(
                self.source_root.join(source.path.as_path()),
                std::io::Error::other(error.to_string()),
            )
        })?;
        self.hardlink_removals.append(group, source).await?;
        Ok(())
    }

    async fn finish_deferred_source_removals(&self) -> Result<()> {
        if self.remove_source_files && self.hardlinks {
            self.hardlink_removals
                .replay(self.source_root.clone())
                .await?;
        }
        Ok(())
    }

    /// Revalidate a linking member's scan identity (cheap stat, no bytes).
    async fn check_source_identity(&self, source: &Entry) -> Result<()> {
        let rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone())
            .await
            .map_err(ExistingDestinationError::from)?;
        let source = source.clone();
        tokio::task::spawn_blocking(move || existing::validate_path(&rooted, &source))
            .await
            .map_err(|error| ExistingDestinationError::Worker(error.to_string()))??;
        Ok(())
    }

    async fn copy_backup(
        &self,
        source: &RelativePath,
        destination: &RelativePath,
        identity: Option<EntryIdentity>,
    ) -> Result<()> {
        let expected = identity.ok_or_else(|| {
            RemotePushError::MissingDestinationIdentity(source.as_path().to_path_buf())
        })?;
        self.remote.copy_file(source, destination, expected).await?;
        Ok(())
    }

    pub async fn execute_delete(&self, action: DeleteAction) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                network_writes: 1,
                ..ResourceRequest::default()
            })
            .await?;
        // --backup preserves a deleted file first (rsync backs up deletions,
        // not just replacements). The server copies regular-file bytes
        // beneath the pinned root; symlinks are removed without a backup so a
        // dangling or escaped target is never resolved.
        if let Some(plan) = &self.backup {
            if action.kind == EntryKind::File {
                let backup_path = plan.destination(&action.path).ok_or_else(|| {
                    RemotePushError::InvalidBackupPath(action.path.as_path().to_path_buf())
                })?;
                self.copy_backup(&action.path, &backup_path, action.identity)
                    .await?;
            }
        }
        self.remote
            .remove(
                &action.path,
                action.kind == EntryKind::Directory,
                action.identity,
            )
            .await?;
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

    pub async fn execute_finalize(&self, metadata: FinalizeMetadata) -> Result<()> {
        let _permit = self
            .scheduler
            .acquire(ResourceRequest {
                metadata_ops: 1,
                network_writes: 1,
                ..ResourceRequest::default()
            })
            .await?;
        let crate::engine::finalize_journal::DirectoryTarget::Observed(expected) = metadata.target
        else {
            return Err(crate::rooted_fs::RootedFsError::DestinationChanged(
                metadata.path.as_path().to_path_buf(),
            )
            .into());
        };
        let rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone()).await?;
        let path = metadata.path.clone();
        let request = crate::rooted_fs::DirectoryPreservationRequest {
            xattrs: self.xattrs,
            acl: self.acls,
            bsd_flags: self.bsd_flags,
        };
        let preservation = if metadata.preserve_source {
            tokio::task::spawn_blocking(move || {
                rooted.read_directory_preservation_blocking(
                    &path,
                    metadata.source_identity,
                    request,
                )
            })
            .await
            .map_err(|error| crate::rooted_fs::RootedFsError::Worker(error.to_string()))??
        } else {
            Default::default()
        };
        self.remote
            .finalize_directory_metadata(
                &metadata.path,
                expected,
                metadata.unix_mode,
                metadata.modified,
                &preservation,
            )
            .await?;
        Ok(())
    }

    /// Re-prove destination content and preservation against scanned identities.
    pub async fn remove_unchanged_source(
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
                network_writes: 1,
            })
            .await?;
        let source_rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone())
            .await
            .map_err(ExistingDestinationError::from)?;
        let source_fingerprint =
            existing::fingerprint(source_rooted.clone(), source.clone(), options).await?;
        let destination_fingerprint = self
            .remote
            .existing_fingerprint(destination, options)
            .await
            .map_err(RemoteSessionError::from)?;
        let receipt = existing::receipt(
            source,
            destination,
            source_fingerprint,
            destination_fingerprint,
        )?;
        existing::remove_verified_source(source_rooted, source.clone(), receipt, None).await?;
        Ok(())
    }

    /// Remove the source entry under --remove-source-files after the
    /// destination commit is acknowledged and produces an authorized publication receipt.
    async fn remove_committed_source(
        &self,
        receipt: &PublishedDestinationReceipt,
        source: &Entry,
    ) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        receipt.validate_source_removal(source).map_err(|error| {
            RemotePushError::SourceRemoval(
                self.source_root.join(source.path.as_path()),
                std::io::Error::other(error.to_string()),
            )
        })?;
        if source.is_file() {
            let proof = receipt.publication().map_err(|error| {
                RemotePushError::SourceRemoval(
                    self.source_root.join(source.path.as_path()),
                    std::io::Error::other(error.to_string()),
                )
            })?;
            self.remote.revalidate_publication(proof).await?;
        }
        // The remote acknowledgement and local unlink are not a distributed
        // CAS: external namespace writers must be excluded for that guarantee.
        self.remove_source_entry_on_disk(source).await
    }

    async fn remove_source_entry_on_disk(&self, source: &Entry) -> Result<()> {
        let rooted = crate::rooted_fs::RootedFs::open(self.source_root.clone())
            .await
            .map_err(ExistingDestinationError::from)?;
        existing::remove_observed_source(rooted, source.clone()).await?;
        Ok(())
    }

    /// The scanned destination state the remote commit must observe, plus a
    /// delta index when signature collection selected one for that entry.
    ///
    /// `None` is a create: the receiver requires the path to remain absent.
    async fn prepare_destination(
        &self,
        destination: Option<Entry>,
    ) -> Result<Option<TransferDestination>> {
        let Some(destination) = destination else {
            return Ok(None);
        };
        let identity = destination.identity.ok_or_else(|| {
            RemotePushError::MissingDestinationIdentity(destination.path.as_path().to_path_buf())
        })?;
        let expectation =
            crate::protocol::WireFileBasis::new(destination.size, *identity.as_bytes());
        let delta_index = if delta_candidate(
            &destination,
            self.delta_min_size,
            self.remote.capabilities(),
        ) {
            self.remote
                .delta_basis(&destination, self.delta_limits)
                .await?
        } else {
            None
        };
        Ok(Some(TransferDestination {
            expectation,
            delta_index,
        }))
    }

    /// Read the local source's extended attributes for one entry when `-X`
    /// requested them. Symlinks carry no portable mutable attributes, so they
    /// are skipped rather than having their targets resolved.
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

    /// Mirror an already-read attribute set onto the remote destination.
    async fn write_destination_xattrs(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        xattrs: &[(OsString, Vec<u8>)],
    ) -> Result<()> {
        let location = XattrLocation::Remote(&self.remote);
        apply_preserved_xattrs(&location, path, kind, xattrs).await?;
        Ok(())
    }

    /// Read the local source's access-control list for one entry when `-A`
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

    /// Mirror an already-read access-control list onto the remote
    /// destination. Preservation ordering matches xattrs (read before the
    /// first mutation, applied after commit).
    async fn write_destination_acls(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        acl: &str,
    ) -> Result<()> {
        let location = AclLocation::Remote(&self.remote);
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

    /// Mirror already-read BSD file flags onto the remote destination.
    /// Preservation ordering matches xattrs (read before the first
    /// mutation, applied after commit).
    async fn write_destination_bsd_flags(
        &self,
        path: &RelativePath,
        kind: EntryKind,
        flags: u32,
    ) -> Result<()> {
        let location = BsdFlagsLocation::Remote(&self.remote);
        apply_preserved_bsd_flags(&location, path, kind, flags).await?;
        Ok(())
    }
}

fn delta_candidate(destination: &Entry, minimum_size: u64, capabilities: &Capabilities) -> bool {
    destination.is_file()
        && destination.size >= minimum_size
        && destination.identity.is_some()
        && capabilities.rolling_signatures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::domain::{EntryIdentity, RelativePath};
    use std::path::PathBuf;

    fn path(value: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(value)).unwrap()
    }

    fn file(value: &str, size: u64, mode: u32) -> Entry {
        let mut entry = Entry::file(path(value), size, Timestamp::UNIX_EPOCH);
        entry.unix_mode = Some(mode);
        entry.identity = Some(EntryIdentity::from_bytes([7; 32]));
        entry
    }

    fn directory(value: &str, mode: u32) -> Entry {
        let mut entry = Entry::directory(path(value), Timestamp::UNIX_EPOCH);
        entry.unix_mode = Some(mode);
        entry
    }

    fn symlink(value: &str, target: &str) -> Entry {
        Entry::symlink(path(value), PathBuf::from(target), Timestamp::UNIX_EPOCH)
    }

    #[test]
    fn new_file_gets_sane_commit_mode_without_permission_preservation() {
        let source = file("file", 1, 0o640);
        let lowered = lower_sync_op(
            SyncOp::Create {
                source: source.clone(),
            },
            ExecutionPolicy::default(),
        )
        .unwrap();
        let RemotePushAction::TransferFile { metadata, .. } = lowered.unwrap().into_action() else {
            panic!("expected file transfer");
        };
        assert_eq!(metadata.unix_mode, Some(0o640));
        assert_eq!(metadata.modified, None);
    }

    #[test]
    fn update_without_p_preserves_existing_destination_mode() {
        let source = file("file", 20, 0o755);
        let destination = file("file", 10, 0o600);
        let lowered = lower_sync_op(
            SyncOp::Update {
                source,
                destination,
            },
            ExecutionPolicy::default(),
        )
        .unwrap();
        let RemotePushAction::TransferFile { metadata, .. } = lowered.unwrap().into_action() else {
            panic!("expected file transfer");
        };
        assert_eq!(metadata.unix_mode, Some(0o600));
    }

    #[test]
    fn update_with_preservation_uses_source_mode_and_time() {
        let mut source = file("file", 20, 0o755);
        source.modified = Timestamp::new(123, 456).unwrap();
        let destination = file("file", 10, 0o600);
        let lowered = lower_sync_op(
            SyncOp::Update {
                source,
                destination,
            },
            ExecutionPolicy {
                preserve_permissions: true,
                preserve_times: true,
            },
        )
        .unwrap();
        let RemotePushAction::TransferFile { metadata, .. } = lowered.unwrap().into_action() else {
            panic!("expected file transfer");
        };
        assert_eq!(metadata.unix_mode, Some(0o755));
        assert_eq!(metadata.modified, Some(Timestamp::new(123, 456).unwrap()));
    }

    #[test]
    fn directory_lowering_only_prepares_namespace() {
        let mut source = directory("dir", 0o750);
        source.modified = Timestamp::new(42, 0).unwrap();
        let lowered = lower_sync_op(
            SyncOp::Create { source },
            ExecutionPolicy {
                preserve_permissions: true,
                preserve_times: true,
            },
        )
        .unwrap();
        assert!(matches!(
            lowered.unwrap().action(),
            RemotePushAction::CreateDirectory { .. }
        ));
    }

    #[test]
    fn directory_source_type_transitions_wait_for_subtree_staging() {
        let source = directory("node", 0o755);
        let destination = file("node", 1, 0o644);
        let error = lower_sync_op(
            SyncOp::Replace {
                source,
                destination,
            },
            ExecutionPolicy::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RemotePushLowerError::TransactionalDirectoryReplace(_)
        ));
    }

    #[test]
    fn file_over_directory_type_transition_lowers_to_transfer_file() {
        let source = file("node", 1, 0o644);
        let destination = directory("node", 0o755);
        let lowered = lower_sync_op(
            SyncOp::Replace {
                source,
                destination,
            },
            ExecutionPolicy::default(),
        )
        .unwrap();
        assert!(matches!(
            lowered.unwrap().action(),
            RemotePushAction::TransferFile { .. }
        ));
    }

    #[test]
    fn file_resource_reservation_is_bounded_independent_of_file_size() {
        let source = file("huge", 80 * 1024 * 1024 * 1024, 0o644);
        let lowered = lower_sync_op(SyncOp::Create { source }, ExecutionPolicy::default()).unwrap();
        let resources = lowered.unwrap().resources();
        assert_eq!(resources.active_files, 1);
        assert_eq!(resources.buffered_bytes, REMOTE_FILE_WORKING_SET);
        assert_eq!(resources.cpu_tasks, 1);
        assert_eq!(resources.network_writes, 1);
    }

    #[test]
    fn delta_candidate_requires_size_identity_and_negotiated_support() {
        let destination = file("file", DEFAULT_REMOTE_DELTA_MIN_SIZE, 0o644);
        let supported = Capabilities {
            rolling_signatures: true,
            ..Capabilities::default()
        };
        let unsupported = Capabilities::default();
        assert!(delta_candidate(
            &destination,
            DEFAULT_REMOTE_DELTA_MIN_SIZE,
            &supported,
        ));
        assert!(!delta_candidate(
            &destination,
            DEFAULT_REMOTE_DELTA_MIN_SIZE + 1,
            &supported,
        ));
        assert!(!delta_candidate(
            &destination,
            DEFAULT_REMOTE_DELTA_MIN_SIZE,
            &unsupported,
        ));
    }

    #[test]
    fn symlink_lowering_preserves_expected_destination_on_push_and_pull() {
        let source = symlink("link", "../target");
        for destination in [
            None,
            Some(symlink("link", "old")),
            Some(file("link", 1, 0o644)),
            Some(directory("link", 0o755)),
        ] {
            let op = match &destination {
                None => SyncOp::Create {
                    source: source.clone(),
                },
                Some(destination) if destination.is_symlink() => SyncOp::Update {
                    source: source.clone(),
                    destination: destination.clone(),
                },
                Some(destination) => SyncOp::Replace {
                    source: source.clone(),
                    destination: destination.clone(),
                },
            };
            let policy = ExecutionPolicy {
                preserve_times: true,
                ..ExecutionPolicy::default()
            };
            let push = lower_sync_op(op.clone(), policy)
                .unwrap()
                .unwrap()
                .into_action();
            let RemotePushAction::ReplaceSymlink {
                source: pushed,
                destination: push_expected,
                modified,
            } = push
            else {
                panic!("expected symlink replacement");
            };
            assert_eq!(pushed, source);
            assert_eq!(push_expected, destination);
            assert_eq!(modified, Some(source.modified));
            let pull = crate::remote::pull_lower::lower_pull_op(op, policy)
                .unwrap()
                .unwrap()
                .into_action();
            let crate::remote::pull::RemotePullAction::ReplaceSymlink {
                source: pulled,
                destination: pull_expected,
                modified,
            } = pull
            else {
                panic!("expected symlink replacement");
            };
            assert_eq!(pulled, source);
            assert_eq!(pull_expected, destination);
            assert_eq!(modified, Some(source.modified));
        }
    }
}
