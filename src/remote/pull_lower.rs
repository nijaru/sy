//! Lowering semantic planner operations to v3 pull actions.
//!
//! Mirrors `crate::remote::push::lower_sync_op` with the roles inverted:
//! file transfers become whole-file fetches (delta pull is a follow-up), and
//! every destination mutation is local. Directory type transitions stay
//! transactional-refused exactly as on push — the planner's contract is
//! direction-neutral.

use crate::engine::domain::{Entry, EntryIdentity, EntryKind, SyncOp, Timestamp};
use crate::engine::planner::ExecutionPolicy;
use crate::engine::scheduler::ResourceRequest;
use crate::engine::work::WorkItem;
use crate::remote::pull::{PullTransferMetadata, RemotePullAction, RemotePullError, Result};

pub fn lower_pull_op(
    op: SyncOp,
    policy: ExecutionPolicy,
) -> Result<Option<WorkItem<RemotePullAction>>> {
    if op.path() != &op.source().path {
        return Err(RemotePullError::DestinationAddressUnsupported);
    }
    match op {
        SyncOp::Create { source, .. } => lower_create(source, policy),
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
) -> Result<Option<WorkItem<RemotePullAction>>> {
    match source.kind {
        EntryKind::Directory => Ok(Some(work_item(RemotePullAction::CreateDirectory {
            source,
        }))),
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
            Ok(Some(work_item(RemotePullAction::FetchFile {
                source,
                destination: None,
                metadata,
            })))
        }
        EntryKind::Symlink => Ok(Some(work_item(RemotePullAction::ReplaceSymlink {
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
) -> Result<Option<WorkItem<RemotePullAction>>> {
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
            Ok(Some(work_item(RemotePullAction::FetchFile {
                source,
                destination: Some(destination),
                metadata,
            })))
        }
        EntryKind::Directory => lower_metadata(source, destination, policy),
        EntryKind::Symlink => Ok(Some(work_item(RemotePullAction::ReplaceSymlink {
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
) -> Result<Option<WorkItem<RemotePullAction>>> {
    match source.kind {
        EntryKind::Directory => Err(RemotePullError::TransactionalDirectoryReplace(
            source.path.as_path().to_path_buf(),
        )),
        EntryKind::File => {
            let mode = source.unix_mode.ok_or_else(|| {
                RemotePullError::MissingScannedMode(source.path.as_path().to_path_buf())
            })?;
            let metadata = PullTransferMetadata {
                unix_mode: Some(mode),
                modified: policy.preserve_times.then_some(source.modified),
            };
            Ok(Some(work_item(RemotePullAction::FetchFile {
                source,
                destination: Some(destination),
                metadata,
            })))
        }
        EntryKind::Symlink => Ok(Some(work_item(RemotePullAction::ReplaceSymlink {
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
) -> Result<Option<WorkItem<RemotePullAction>>> {
    if source.is_directory() {
        return Ok(None);
    }
    let Some((unix_mode, modified)) = requested_metadata(&source, &destination, policy)? else {
        return Ok(None);
    };
    let expected_destination = destination.identity.ok_or_else(|| {
        RemotePullError::MissingDestinationIdentity(destination.path.as_path().to_path_buf())
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
) -> Result<Option<(Option<u32>, Option<Timestamp>)>> {
    let unix_mode = if policy.preserve_permissions
        && !source.is_symlink()
        && destination.unix_mode != source.unix_mode
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
        .filter(|_| destination.modified != source.modified);
    Ok((unix_mode.is_some() || modified.is_some()).then_some((unix_mode, modified)))
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
        RemotePullAction::ApplyMetadata { .. } => ResourceRequest {
            active_files: 0,
            buffered_bytes: 4 * crate::protocol::MAX_FRAME_PAYLOAD as u64,
            metadata_ops: 1,
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

fn work_item(action: RemotePullAction) -> WorkItem<RemotePullAction> {
    let resources = action_resources(&action);
    WorkItem::new(action, resources)
}

fn metadata_work(
    source: Entry,
    expected_destination: EntryIdentity,
    unix_mode: Option<u32>,
    modified: Option<Timestamp>,
) -> WorkItem<RemotePullAction> {
    work_item(RemotePullAction::ApplyMetadata {
        source,
        expected_destination,
        unix_mode,
        modified,
    })
}
