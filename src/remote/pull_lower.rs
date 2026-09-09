//! Lowering semantic planner operations to v3 pull actions.
//!
//! Mirrors `crate::remote::push::lower_sync_op` with the roles inverted:
//! file transfers become whole-file fetches (delta pull is a follow-up), and
//! every destination mutation is local. Directory type transitions stay
//! transactional-refused exactly as on push — the planner's contract is
//! direction-neutral.

use crate::engine::domain::{Entry, EntryKind, SyncOp, Timestamp};
use crate::engine::scheduler::ResourceRequest;
use crate::engine::work::WorkItem;
use crate::remote::pull::{PullTransferMetadata, RemotePullAction, RemotePullError, Result};

#[derive(Debug, Default)]
pub struct LoweredPull {
    pub main: Option<WorkItem<RemotePullAction>>,
    pub finalize: Option<WorkItem<RemotePullAction>>,
}

pub fn lower_pull_op(op: SyncOp, policy: PullLowerPolicy) -> Result<LoweredPull> {
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
        SyncOp::Skip { .. } => Ok(LoweredPull::default()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullLowerPolicy {
    pub preserve_permissions: bool,
    pub preserve_times: bool,
}

fn lower_create(source: Entry, policy: PullLowerPolicy) -> Result<LoweredPull> {
    match source.kind {
        EntryKind::Directory => {
            let finalize = requested_metadata(&source, None, policy, true)?
                .map(|(unix_mode, modified)| metadata_work(source.clone(), unix_mode, modified));
            Ok(LoweredPull {
                main: Some(mutation_work(RemotePullAction::CreateDirectory { source })),
                finalize,
            })
        }
        EntryKind::File => {
            // Staging is private at 0600, so a committed file always needs an
            // explicit final mode; the source's scanned mode is the create
            // default, matching push.
            let mode = source.unix_mode.ok_or_else(|| {
                RemotePullError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = PullTransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(LoweredPull {
                main: Some(file_work(RemotePullAction::FetchFile {
                    source,
                    destination: None,
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Symlink => Ok(LoweredPull {
            main: Some(mutation_work(RemotePullAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
    }
}

fn lower_update(source: Entry, destination: Entry, policy: PullLowerPolicy) -> Result<LoweredPull> {
    match source.kind {
        EntryKind::File => {
            let mode = if policy.preserve_permissions {
                source.unix_mode
            } else {
                destination.unix_mode
            }
            .ok_or_else(|| {
                RemotePullError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = PullTransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(LoweredPull {
                main: Some(file_work(RemotePullAction::FetchFile {
                    source,
                    destination: Some(destination),
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Directory => lower_metadata(source, destination, policy),
        EntryKind::Symlink => Ok(LoweredPull {
            main: Some(mutation_work(RemotePullAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
    }
}

fn lower_replace(
    source: Entry,
    _destination: Entry,
    policy: PullLowerPolicy,
) -> Result<LoweredPull> {
    if source.is_directory() {
        return Err(RemotePullError::TransactionalDirectoryReplace(
            source.path.as_path().to_path_buf(),
        ));
    }
    match source.kind {
        EntryKind::File => {
            let mode = source.unix_mode.ok_or_else(|| {
                RemotePullError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = PullTransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(LoweredPull {
                main: Some(file_work(RemotePullAction::FetchFile {
                    source,
                    destination: None,
                    metadata,
                })),
                finalize: None,
            })
        }
        EntryKind::Symlink => Ok(LoweredPull {
            main: Some(mutation_work(RemotePullAction::ReplaceSymlink {
                modified: policy.preserve_times.then_some(source.modified),
                source,
            })),
            finalize: None,
        }),
        EntryKind::Directory => unreachable!("directory transition refused above"),
    }
}

fn lower_metadata(
    source: Entry,
    destination: Entry,
    policy: PullLowerPolicy,
) -> Result<LoweredPull> {
    let Some((unix_mode, modified)) =
        requested_metadata(&source, Some(&destination), policy, false)?
    else {
        return Ok(LoweredPull::default());
    };
    let work = metadata_work(source.clone(), unix_mode, modified);
    if source.is_directory() {
        Ok(LoweredPull {
            main: None,
            finalize: Some(work),
        })
    } else {
        Ok(LoweredPull {
            main: Some(work),
            finalize: None,
        })
    }
}

fn requested_metadata(
    source: &Entry,
    destination: Option<&Entry>,
    policy: PullLowerPolicy,
    include_requested_even_if_unknown_destination: bool,
) -> Result<Option<(Option<u32>, Option<Timestamp>)>> {
    let unix_mode = if policy.preserve_permissions
        && (include_requested_even_if_unknown_destination
            || destination.is_some_and(|entry| entry.unix_mode != source.unix_mode))
    {
        Some(source.unix_mode.ok_or_else(|| {
            RemotePullError::MissingScannedMode(source.path.as_path().to_path_buf())
        })?)
    } else {
        None
    };
    let modified = policy
        .preserve_times
        .then_some(source.modified)
        .filter(|_| destination.is_none_or(|entry| entry.modified != source.modified));
    Ok(Some((unix_mode, modified)))
}

pub fn action_resources(action: &RemotePullAction) -> crate::engine::scheduler::ResourceRequest {
    match action {
        RemotePullAction::FetchFile { .. } => crate::engine::scheduler::ResourceRequest {
            active_files: 1,
            buffered_bytes: crate::remote::pull::REMOTE_FETCH_WORKING_SET,
            metadata_ops: 0,
            cpu_tasks: 1,
            network_writes: 1,
        },
        _ => crate::engine::scheduler::ResourceRequest {
            active_files: 0,
            buffered_bytes: 0,
            metadata_ops: 1,
            cpu_tasks: 0,
            network_writes: 0,
        },
    }
}

fn file_work(action: RemotePullAction) -> WorkItem<RemotePullAction> {
    WorkItem::new(
        action,
        ResourceRequest {
            active_files: 1,
            // Mirrors the push side's per-file working set so the scheduler
            // byte budget stays direction-symmetric.
            buffered_bytes: crate::remote::pull::REMOTE_FETCH_WORKING_SET,
            metadata_ops: 0,
            cpu_tasks: 1,
            network_writes: 1,
        },
    )
}

fn mutation_work(action: RemotePullAction) -> WorkItem<RemotePullAction> {
    WorkItem::new(
        action,
        ResourceRequest {
            active_files: 0,
            buffered_bytes: 0,
            metadata_ops: 1,
            cpu_tasks: 0,
            network_writes: 0,
        },
    )
}

fn metadata_work(
    source: Entry,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
) -> WorkItem<RemotePullAction> {
    mutation_work(RemotePullAction::ApplyMetadata {
        source,
        unix_mode,
        modified,
    })
}
