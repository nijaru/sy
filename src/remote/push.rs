use crate::engine::delete_plan::DeleteAction;
use crate::engine::domain::{Entry, EntryKind, SyncOp, Timestamp};
use crate::engine::finalize_journal::FinalizeMetadata;
use crate::engine::scheduler::{ResourceRequest, Scheduler, SchedulerError};
use crate::engine::work::WorkItem;
use crate::protocol::CapabilitySet;
use crate::remote::runtime::{ClientRemoteHandle, RemoteSessionError};
use crate::remote::transfer::{RemoteDeltaBasis, TransferMetadata, TransferSummary};
use crate::transfer::delta::BasisIndexLimits;
use std::path::PathBuf;

/// Existing v2 uses 10 MiB as the point where rolling-delta setup starts to
/// repay its extra destination read and signature round trip. Keep that as the
/// initial v3 policy boundary until dedicated v3 benchmarks tune it.
pub const DEFAULT_REMOTE_DELTA_MIN_SIZE: u64 = 10 * 1024 * 1024;

/// Per-file reservation for the bounded producer/signature path. This is a
/// working-set budget, not the logical file size. The transfer protocol has its
/// own global router byte budget in addition to this scheduler admission.
pub const REMOTE_FILE_WORKING_SET: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemotePushPolicy {
    pub preserve_permissions: bool,
    pub preserve_times: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemotePushAction {
    CreateDirectory {
        source: Entry,
    },
    TransferFile {
        source: Entry,
        destination: Option<Entry>,
        metadata: TransferMetadata,
    },
    ReplaceSymlink {
        source: Entry,
        modified: Option<Timestamp>,
    },
    ApplyMetadata {
        source: Entry,
        unix_mode: Option<u32>,
        modified: Option<Timestamp>,
    },
}

/// Main-phase work may execute concurrently after a complete preflight.
/// Finalize work is intentionally separate so directory mode/mtime is applied
/// only after descendants have been created or updated.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoweredPush {
    pub main: Option<WorkItem<RemotePushAction>>,
    pub finalize: Option<WorkItem<RemotePushAction>>,
}

#[derive(Debug, thiserror::Error)]
pub enum RemotePushLowerError {
    #[error("regular-file commit requires Unix mode metadata for {0}")]
    MissingFileMode(PathBuf),

    #[error("metadata preservation requires Unix mode metadata for {0}")]
    MissingPreservedMode(PathBuf),

    #[error("transactional type replacement is not implemented for directory transition at {0}")]
    TransactionalDirectoryReplace(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum RemotePushError {
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),

    #[error(transparent)]
    Remote(#[from] RemoteSessionError),

    #[error("symlink action is missing the scanned target for {0}")]
    MissingSymlinkTarget(PathBuf),

    #[error("source entry {0} changed between scan and removal; not removing")]
    SourceChangedBeforeRemoval(PathBuf),

    #[error("failed to remove committed source {0}: {1}")]
    SourceRemoval(PathBuf, std::io::Error),

    #[error("--backup location for {0} is not representable beneath the destination root")]
    InvalidBackupPath(PathBuf),
}

pub type LowerResult<T> = std::result::Result<T, RemotePushLowerError>;
pub type Result<T> = std::result::Result<T, RemotePushError>;

/// Lower one semantic planner operation into concrete remote push work.
///
/// Byte strategy is deliberately absent here. A changed destination file stays
/// attached to `TransferFile`; the executor requests rolling signatures only if
/// the negotiated peer capabilities and workload size make delta plausible.
pub fn lower_sync_op(op: SyncOp, policy: RemotePushPolicy) -> LowerResult<LoweredPush> {
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
        SyncOp::Skip { .. } => Ok(LoweredPush::default()),
    }
}

fn lower_create(source: Entry, policy: RemotePushPolicy) -> LowerResult<LoweredPush> {
    match source.kind {
        EntryKind::Directory => {
            let finalize = requested_metadata(&source, None, policy, true)?
                .map(|(unix_mode, modified)| metadata_work(source.clone(), unix_mode, modified));
            Ok(LoweredPush {
                main: Some(mutation_work(RemotePushAction::CreateDirectory { source })),
                finalize,
            })
        }
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
            };
            Ok(LoweredPush {
                main: Some(file_work(RemotePushAction::TransferFile {
                    source,
                    destination: None,
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Symlink => Ok(LoweredPush {
            main: Some(mutation_work(RemotePushAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
    }
}

fn lower_update(
    source: Entry,
    destination: Entry,
    policy: RemotePushPolicy,
) -> LowerResult<LoweredPush> {
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
            };
            Ok(LoweredPush {
                main: Some(file_work(RemotePushAction::TransferFile {
                    source,
                    destination: Some(destination),
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Directory => lower_metadata(source, destination, policy),
        EntryKind::Symlink => Ok(LoweredPush {
            main: Some(mutation_work(RemotePushAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
    }
}

fn lower_replace(
    source: Entry,
    destination: Entry,
    policy: RemotePushPolicy,
) -> LowerResult<LoweredPush> {
    if source.is_directory() || destination.is_directory() {
        return Err(RemotePushLowerError::TransactionalDirectoryReplace(
            source.path.as_path().to_path_buf(),
        ));
    }

    match source.kind {
        EntryKind::File => {
            let mode = source.unix_mode.ok_or_else(|| {
                RemotePushLowerError::MissingFileMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = TransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(LoweredPush {
                main: Some(file_work(RemotePushAction::TransferFile {
                    source,
                    // A type replacement cannot reuse the old non-file leaf as
                    // a rolling basis. Same-directory staged rename still makes
                    // file-over-symlink replacement atomic.
                    destination: None,
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Symlink => Ok(LoweredPush {
            main: Some(mutation_work(RemotePushAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
        EntryKind::Directory => unreachable!("directory transitions returned above"),
    }
}

fn lower_metadata(
    source: Entry,
    destination: Entry,
    policy: RemotePushPolicy,
) -> LowerResult<LoweredPush> {
    let Some((unix_mode, modified)) =
        requested_metadata(&source, Some(&destination), policy, false)?
    else {
        return Ok(LoweredPush::default());
    };
    let work = metadata_work(source.clone(), unix_mode, modified);
    if source.is_directory() {
        Ok(LoweredPush {
            main: None,
            finalize: Some(work),
        })
    } else {
        Ok(LoweredPush {
            main: Some(work),
            finalize: None,
        })
    }
}

fn requested_metadata(
    source: &Entry,
    destination: Option<&Entry>,
    policy: RemotePushPolicy,
    include_requested_even_if_unknown_destination: bool,
) -> LowerResult<Option<(Option<u32>, Option<Timestamp>)>> {
    let unix_mode = if policy.preserve_permissions
        && (include_requested_even_if_unknown_destination
            || destination.is_some_and(|entry| entry.unix_mode != source.unix_mode))
    {
        Some(source.unix_mode.ok_or_else(|| {
            RemotePushLowerError::MissingPreservedMode(source.path.as_path().to_path_buf())
        })?)
    } else {
        None
    };
    let modified = if policy.preserve_times
        && (include_requested_even_if_unknown_destination
            || destination.is_some_and(|entry| entry.modified != source.modified))
    {
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
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
) -> WorkItem<RemotePushAction> {
    mutation_work(RemotePushAction::ApplyMetadata {
        source,
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
    fn destination(
        &self,
        path: &crate::engine::domain::RelativePath,
    ) -> Option<crate::engine::domain::RelativePath> {
        let mut components: Vec<std::ffi::OsString> = Vec::new();
        if let Some(dir) = &self.dir {
            components.extend(dir.as_path().components().filter_map(|c| match c {
                std::path::Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            }));
        } else {
            // Beside the file: parent components, then the suffixed name.
            components.extend(
                path.as_path()
                    .parent()?
                    .components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(name) => Some(name.to_os_string()),
                        _ => None,
                    }),
            );
        }
        let name = path.as_path().file_name()?.to_string_lossy().into_owned();
        components.push(format!("{name}{}", self.suffix).into());
        crate::engine::domain::RelativePath::new(
            components
                .iter()
                .map(|c| c.as_os_str())
                .collect::<std::path::PathBuf>(),
        )
        .ok()
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

    pub fn with_backup(mut self, plan: Option<RemoteBackupPlan>) -> Self {
        self.backup = plan;
        self
    }

    pub async fn execute(
        &self,
        item: WorkItem<RemotePushAction>,
    ) -> Result<Option<TransferSummary>> {
        let (action, resources) = item.into_parts();
        let _permit = self.scheduler.acquire(resources).await?;

        match action {
            RemotePushAction::CreateDirectory { source } => {
                self.remote.create_directory(&source.path).await?;
                Ok(None)
            }
            RemotePushAction::TransferFile {
                source,
                destination,
                metadata,
            } => {
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
                        self.remote.copy_file(&existing.path, &backup_path).await?;
                    }
                }
                let delta_basis = self.prepare_delta_basis(destination).await?;
                let summary = self
                    .remote
                    .transfer_file_with_metadata(
                        self.source_root.clone(),
                        source.clone(),
                        delta_basis,
                        metadata,
                    )
                    .await?;
                self.remove_committed_source(&source).await?;
                Ok(Some(summary))
            }
            RemotePushAction::ReplaceSymlink { source, modified } => {
                let target = source.symlink_target.as_deref().ok_or_else(|| {
                    RemotePushError::MissingSymlinkTarget(source.path.as_path().to_path_buf())
                })?;
                self.remote.replace_symlink(&source.path, target).await?;
                if let Some(modified) = modified {
                    self.remote
                        .apply_metadata(&source.path, EntryKind::Symlink, None, Some(modified))
                        .await?;
                }
                self.remove_committed_source(&source).await?;
                Ok(None)
            }
            RemotePushAction::ApplyMetadata {
                source,
                unix_mode,
                modified,
            } => {
                self.remote
                    .apply_metadata(&source.path, source.kind, unix_mode, modified)
                    .await?;
                Ok(None)
            }
        }
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
            if !action.is_directory {
                let backup_path = plan.destination(&action.path).ok_or_else(|| {
                    RemotePushError::InvalidBackupPath(action.path.as_path().to_path_buf())
                })?;
                let copied = self.remote.copy_file(&action.path, &backup_path).await;
                match copied {
                    Ok(()) => {}
                    // The entry may be a symlink (or otherwise not a regular
                    // file): remove proceeds without a backup.
                    Err(error) => {
                        tracing::debug!(
                            path = %action.path.as_path().display(),
                            error = %error,
                            "delete backup copy unavailable; removing without backup"
                        );
                    }
                }
            }
        }
        self.remote
            .remove(&action.path, action.is_directory)
            .await?;
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
        self.remote
            .apply_metadata(
                &metadata.path,
                metadata.kind,
                metadata.unix_mode,
                metadata.modified,
            )
            .await?;
        Ok(())
    }

    /// --remove-source-files: remove an entry the planner verified as already
    /// in sync with the destination. Only callable for non-directories whose
    /// scan identity still matches; a changed source is refused.
    pub async fn remove_verified_parity_source(&self, source: &Entry) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        self.remove_committed_source(source).await
    }

    /// Remove the source entry under --remove-source-files after the
    /// destination commit is acknowledged. The scan identity is re-checked
    /// first: a source replaced or modified after the scan must not be
    /// removed, because the moved bytes would not be the verified ones.
    #[cfg(unix)]
    async fn remove_committed_source(&self, source: &Entry) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        let path = self.source_root.join(source.path.as_path());
        let expected = source
            .identity
            .ok_or_else(|| RemotePushError::SourceChangedBeforeRemoval(path.clone()))?;
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|_| RemotePushError::SourceChangedBeforeRemoval(path.clone()))?;
        let kind = if metadata.file_type().is_symlink() {
            EntryKind::Symlink
        } else if metadata.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let current = crate::endpoint::local_identity::metadata_identity(&metadata, kind)
            .ok_or_else(|| RemotePushError::SourceChangedBeforeRemoval(path.clone()))?;
        if current != expected {
            return Err(RemotePushError::SourceChangedBeforeRemoval(path));
        }
        // Only non-directories move. Empty directories stay, matching rsync's
        // --remove-source-files semantics.
        if kind == EntryKind::Directory {
            return Ok(());
        }
        tokio::fs::remove_file(&path)
            .await
            .map_err(|error| RemotePushError::SourceRemoval(path, error))?;
        Ok(())
    }

    #[cfg(not(unix))]
    async fn remove_committed_source(&self, source: &Entry) -> Result<()> {
        if !self.remove_source_files {
            return Ok(());
        }
        let path = self.source_root.join(source.path.as_path());
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        if metadata.is_dir() {
            return Ok(());
        }
        tokio::fs::remove_file(&path)
            .await
            .map_err(|error| RemotePushError::SourceRemoval(path, error))?;
        Ok(())
    }

    async fn prepare_delta_basis(
        &self,
        destination: Option<Entry>,
    ) -> Result<Option<RemoteDeltaBasis>> {
        let Some(destination) = destination else {
            return Ok(None);
        };
        if !delta_candidate(
            &destination,
            self.delta_min_size,
            self.remote.ready().capabilities,
        ) {
            return Ok(None);
        }
        let Some(index) = self
            .remote
            .delta_basis(&destination, self.delta_limits)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(RemoteDeltaBasis {
            entry: destination,
            index,
        }))
    }
}

fn delta_candidate(destination: &Entry, minimum_size: u64, capabilities: CapabilitySet) -> bool {
    destination.is_file()
        && destination.size >= minimum_size
        && destination.identity.is_some()
        && capabilities.contains(CapabilitySet::ROLLING_SIGNATURES)
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
            RemotePushPolicy::default(),
        )
        .unwrap();
        let RemotePushAction::TransferFile { metadata, .. } = lowered.main.unwrap().into_action()
        else {
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
            RemotePushPolicy::default(),
        )
        .unwrap();
        let RemotePushAction::TransferFile { metadata, .. } = lowered.main.unwrap().into_action()
        else {
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
            RemotePushPolicy {
                preserve_permissions: true,
                preserve_times: true,
            },
        )
        .unwrap();
        let RemotePushAction::TransferFile { metadata, .. } = lowered.main.unwrap().into_action()
        else {
            panic!("expected file transfer");
        };
        assert_eq!(metadata.unix_mode, Some(0o755));
        assert_eq!(metadata.modified, Some(Timestamp::new(123, 456).unwrap()));
    }

    #[test]
    fn directory_metadata_is_deferred_until_finalize() {
        let mut source = directory("dir", 0o750);
        source.modified = Timestamp::new(42, 0).unwrap();
        let lowered = lower_sync_op(
            SyncOp::Create { source },
            RemotePushPolicy {
                preserve_permissions: true,
                preserve_times: true,
            },
        )
        .unwrap();
        assert!(matches!(
            lowered.main.unwrap().action(),
            RemotePushAction::CreateDirectory { .. }
        ));
        assert!(matches!(
            lowered.finalize.unwrap().action(),
            RemotePushAction::ApplyMetadata { .. }
        ));
    }

    #[test]
    fn directory_type_transitions_wait_for_transactional_replace() {
        let source = file("node", 1, 0o644);
        let destination = directory("node", 0o755);
        let error = lower_sync_op(
            SyncOp::Replace {
                source,
                destination,
            },
            RemotePushPolicy::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RemotePushLowerError::TransactionalDirectoryReplace(_)
        ));
    }

    #[test]
    fn file_resource_reservation_is_bounded_independent_of_file_size() {
        let source = file("huge", 80 * 1024 * 1024 * 1024, 0o644);
        let lowered =
            lower_sync_op(SyncOp::Create { source }, RemotePushPolicy::default()).unwrap();
        let resources = lowered.main.unwrap().resources();
        assert_eq!(resources.active_files, 1);
        assert_eq!(resources.buffered_bytes, REMOTE_FILE_WORKING_SET);
        assert_eq!(resources.cpu_tasks, 1);
        assert_eq!(resources.network_writes, 1);
    }

    #[test]
    fn delta_candidate_requires_size_identity_and_negotiated_support() {
        let destination = file("file", DEFAULT_REMOTE_DELTA_MIN_SIZE, 0o644);
        assert!(delta_candidate(
            &destination,
            DEFAULT_REMOTE_DELTA_MIN_SIZE,
            CapabilitySet::ROLLING_SIGNATURES,
        ));
        assert!(!delta_candidate(
            &destination,
            DEFAULT_REMOTE_DELTA_MIN_SIZE + 1,
            CapabilitySet::ROLLING_SIGNATURES,
        ));
        assert!(!delta_candidate(
            &destination,
            DEFAULT_REMOTE_DELTA_MIN_SIZE,
            CapabilitySet::empty(),
        ));
    }

    #[test]
    fn symlink_create_is_main_phase_and_preserves_target() {
        let source = symlink("link", "../target");
        let lowered =
            lower_sync_op(SyncOp::Create { source }, RemotePushPolicy::default()).unwrap();
        let RemotePushAction::ReplaceSymlink { source, .. } = lowered.main.unwrap().into_action()
        else {
            panic!("expected symlink replacement");
        };
        assert_eq!(source.symlink_target, Some(PathBuf::from("../target")));
    }
}
