use crate::engine::delete_plan::{DeletePlan, DeletePlanError, DeletePolicy, DeleteTracker};
use crate::engine::domain::{Entry, EntryKind, RelativePath, SyncOp};
use crate::engine::finalize_journal::{
    FinalizeJournal, FinalizeJournalError, FinalizeJournalReader, FinalizeMetadata,
};
use crate::engine::plan_journal::{PlanJournal, PlanJournalError, PlanJournalReader};
use crate::engine::planner::{
    finish_content_comparison, plan_entry, ComparisonPolicy, PlanDecision,
};
use crate::engine::reconcile::{EngineError, EntryStream, OrderedReconciler, ReconcileItem};
use crate::remote::push::{
    lower_sync_op, RemotePushAction, RemotePushError, RemotePushExecutor, RemotePushLowerError,
    RemotePushPolicy,
};
use crate::remote::transfer::TransferSummary;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::task::JoinSet;

#[derive(Debug, thiserror::Error)]
pub enum RemotePushControllerError {
    #[error(transparent)]
    Reconcile(#[from] EngineError),

    #[error(transparent)]
    DeletePlan(#[from] DeletePlanError),

    #[error(transparent)]
    PlanJournal(#[from] PlanJournalError),

    #[error(transparent)]
    FinalizeJournal(#[from] FinalizeJournalError),

    #[error(transparent)]
    Lower(#[from] RemotePushLowerError),

    #[error(transparent)]
    Execute(#[from] RemotePushError),

    #[error(transparent)]
    Hash(#[from] crate::remote::hash::RemoteHashError),

    #[error("v3 remote checksum comparison is not implemented for {0}")]
    UnsupportedContentComparison(RelativePath),

    #[error("remote push worker failed: {0}")]
    Worker(String),

    #[error("remote push {0} counter overflow")]
    CounterOverflow(&'static str),
}

pub type Result<T> = std::result::Result<T, RemotePushControllerError>;

/// Disk-backed semantic result of a complete no-mutation ordered merge.
///
/// Source-backed semantic work, requested directory final metadata, and exact
/// destination-only deletion candidates are derived from the same ordered stream.
/// Returning this value means the
/// whole merge completed and any enabled deletion threshold passed without
/// mutating either endpoint.
pub struct RemotePushPlan {
    reader: PlanJournalReader,
    finalize: FinalizeJournalReader,
    operations: u64,
    delete: Option<DeletePlan>,
    execution_policy: RemotePushPolicy,
}

impl RemotePushPlan {
    pub const fn operations(&self) -> u64 {
        self.operations
    }

    pub fn eligible_destination_entries(&self) -> u64 {
        self.delete
            .as_ref()
            .map_or(0, DeletePlan::eligible_destination_entries)
    }

    pub fn delete_candidates(&self) -> u64 {
        self.delete
            .as_ref()
            .map_or(0, DeletePlan::delete_candidates)
    }
}

/// Complete the source/destination merge without mutating either endpoint and
/// spool semantic planner output plus any exact delete plan to disk.
///
/// `delete_in_scope` is evaluated only when deletion is enabled. Keeping scope
/// policy at this boundary lets excluded destination subtrees protect candidate
/// ancestors while the engine retains ownership of exact counting and replay.
pub async fn preflight_remote_push(
    source: EntryStream,
    destination: EntryStream,
    policy: ComparisonPolicy,
    delete_policy: Option<DeletePolicy>,
    delete_in_scope: impl FnMut(&Entry) -> bool,
) -> Result<RemotePushPlan> {
    preflight_remote_push_scoped(
        source,
        destination,
        policy,
        delete_policy,
        |_| true,
        delete_in_scope,
    )
    .await
}

/// Complete remote-push preflight while independently controlling semantic work
/// and deletion scope.
///
/// `plan_in_scope` is evaluated for every source-backed entry. Entries outside
/// semantic scope remain visible to deletion tracking, so a selection rule can
/// suppress transfer work without making existing destination content appear
/// source-absent. `delete_in_scope` continues to define which destination entries
/// may participate in deletion.
pub async fn preflight_remote_push_scoped(
    source: EntryStream,
    destination: EntryStream,
    policy: ComparisonPolicy,
    delete_policy: Option<DeletePolicy>,
    plan_in_scope: impl FnMut(&Entry) -> bool,
    delete_in_scope: impl FnMut(&Entry) -> bool,
) -> Result<RemotePushPlan> {
    preflight_remote_push_scoped_with_content(
        source,
        destination,
        policy,
        delete_policy,
        plan_in_scope,
        delete_in_scope,
        |source, _destination| async move {
            Err::<bool, _>(RemotePushControllerError::UnsupportedContentComparison(
                source.path,
            ))
        },
    )
    .await
}

/// Complete remote-push preflight with asynchronous content comparison
/// for planner decisions that cannot be resolved from scan metadata.
///
/// The comparison future runs while the controller is still in the
/// no-mutation phase. A comparison failure therefore prevents a plan,
/// delete replay, and all namespace or file mutations.
pub async fn preflight_remote_push_scoped_with_content<F, Fut>(
    source: EntryStream,
    destination: EntryStream,
    policy: ComparisonPolicy,
    delete_policy: Option<DeletePolicy>,
    mut plan_in_scope: impl FnMut(&Entry) -> bool,
    mut delete_in_scope: impl FnMut(&Entry) -> bool,
    mut compare_content: F,
) -> Result<RemotePushPlan>
where
    F: FnMut(Entry, Entry) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let mut reconciler = OrderedReconciler::new(source, destination);
    let mut journal = PlanJournal::new().await?;
    let mut finalize = FinalizeJournal::new().await?;
    let execution_policy = RemotePushPolicy {
        preserve_permissions: policy.preserve_permissions,
        preserve_times: policy.preserve_times,
    };
    let mut delete = match delete_policy {
        Some(policy) => Some(DeleteTracker::new(policy).await?),
        None => None,
    };
    let mut operations = 0_u64;

    while let Some(item) = reconciler.next().await? {
        let decision = match item {
            ReconcileItem::SourceOnly(source) => {
                if let Some(delete) = &mut delete {
                    delete.observe_source_only(&source).await?;
                }
                if !plan_in_scope(&source) {
                    continue;
                }
                if !policy.existing_only {
                    append_directory_finalize(&mut finalize, &source, None, policy).await?;
                }
                plan_entry(source, None, policy)
            }
            ReconcileItem::Matched {
                source,
                destination,
            } => {
                if let Some(delete) = &mut delete {
                    let in_scope = delete_in_scope(&destination);
                    delete.observe_matched(&destination, in_scope).await?;
                }
                if !plan_in_scope(&source) {
                    continue;
                }
                append_directory_finalize(&mut finalize, &source, Some(&destination), policy)
                    .await?;
                plan_entry(source, Some(destination), policy)
            }
            ReconcileItem::DestinationOnly(destination) => {
                if let Some(delete) = &mut delete {
                    let in_scope = delete_in_scope(&destination);
                    delete
                        .observe_destination_only(&destination, in_scope)
                        .await?;
                }
                continue;
            }
        };
        let operation = match decision {
            PlanDecision::Ready(operation) => operation,
            PlanDecision::NeedContentComparison {
                source,
                destination,
            } => {
                let contents_equal = compare_content(source.clone(), destination.clone()).await?;
                finish_content_comparison(source, destination, contents_equal, policy)
            }
        };
        journal.append(&operation).await?;
        operations = checked_add(operations, 1, "operation")?;
    }

    let delete = match delete {
        Some(delete) => Some(delete.finish().await?),
        None => None,
    };
    Ok(RemotePushPlan {
        reader: journal.seal().await?,
        finalize: finalize.seal().await?,
        operations,
        delete,
        execution_policy,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemotePushPreview {
    pub planned_operations: u64,
    pub delete_candidates: u64,
    pub files_created: u64,
    pub files_updated: u64,
    pub files_skipped: u64,
    pub dirs_created: u64,
    pub symlinks_created: u64,
    pub bytes_to_create: u64,
    pub bytes_to_update: u64,
}

/// Consume a completed remote-push plan without executing endpoint mutations.
///
/// Dry-run uses the exact same full-tree preflight as execution, including
/// deletion protection and threshold checks, then reads only the disk-backed
/// semantic journal. Dropping the delete/finalize journals performs no remote
/// operations.
pub async fn preview_remote_push(plan: RemotePushPlan) -> Result<RemotePushPreview> {
    let RemotePushPlan {
        mut reader,
        operations,
        delete,
        ..
    } = plan;
    let mut preview = RemotePushPreview {
        planned_operations: operations,
        delete_candidates: delete.as_ref().map_or(0, DeletePlan::delete_candidates),
        ..RemotePushPreview::default()
    };

    while let Some(operation) = reader.next().await? {
        record_preview_operation(&mut preview, &operation)?;
    }
    Ok(preview)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemotePushSummary {
    pub planned_operations: u64,
    pub main_operations: u64,
    pub delete_candidates: u64,
    pub deleted_entries: u64,
    pub finalized_metadata: u64,
    pub files_transferred: u64,
    pub files_created: u64,
    pub files_updated: u64,
    pub files_skipped: u64,
    pub dirs_created: u64,
    pub symlinks_created: u64,
    pub delta_files: u64,
    pub literal_bytes: u64,
    pub reused_bytes: u64,
}

/// Executes one preflighted v3 push with bounded task fan-out.
///
/// Directory creation is awaited in preorder before reconciliation advances to
/// descendants. Independent leaf work may run concurrently, but at most
/// `max_in_flight` worker futures exist even before scheduler admission. All
/// directory metadata is preflighted into a reverse journal and replayed
/// child-before-parent only after main work and reverse deletes complete.
pub struct RemotePushController {
    executor: Arc<RemotePushExecutor>,
    max_in_flight: NonZeroUsize,
}

impl RemotePushController {
    pub fn new(executor: RemotePushExecutor, max_in_flight: NonZeroUsize) -> Self {
        Self {
            executor: Arc::new(executor),
            max_in_flight,
        }
    }

    pub async fn execute(&self, plan: RemotePushPlan) -> Result<RemotePushSummary> {
        let RemotePushPlan {
            mut reader,
            mut finalize,
            operations,
            delete,
            execution_policy,
        } = plan;
        let delete_candidates = delete.as_ref().map_or(0, DeletePlan::delete_candidates);
        let mut summary = RemotePushSummary {
            planned_operations: operations,
            delete_candidates,
            ..RemotePushSummary::default()
        };
        let mut workers = JoinSet::new();

        while let Some(operation) = reader.next().await? {
            record_semantic_operation(&mut summary, &operation)?;
            let Some(main) = lower_sync_op(operation, execution_policy)?.main else {
                continue;
            };
            summary.main_operations = checked_add(summary.main_operations, 1, "main operation")?;

            if matches!(main.action(), RemotePushAction::CreateDirectory { .. }) {
                let result = self.executor.execute(main).await?;
                record_transfer(&mut summary, result)?;
                continue;
            }

            while workers.len() >= self.max_in_flight.get() {
                collect_one(&mut workers, &mut summary).await?;
            }
            let executor = Arc::clone(&self.executor);
            workers.spawn(async move { executor.execute(main).await });
        }

        while !workers.is_empty() {
            collect_one(&mut workers, &mut summary).await?;
        }

        if let Some(delete) = delete {
            let mut replay = delete.into_replay();
            while let Some(action) = replay.next_action().await? {
                self.executor.execute_delete(action).await?;
                summary.deleted_entries = checked_add(summary.deleted_entries, 1, "deleted entry")?;
            }
        }

        while let Some(metadata) = finalize.next().await? {
            self.executor.execute_finalize(metadata).await?;
            summary.finalized_metadata =
                checked_add(summary.finalized_metadata, 1, "finalized metadata")?;
        }

        Ok(summary)
    }
}

async fn append_directory_finalize(
    journal: &mut FinalizeJournal,
    source: &Entry,
    destination: Option<&Entry>,
    policy: ComparisonPolicy,
) -> Result<()> {
    if let Some(metadata) = directory_finalize_metadata(source, destination, policy)? {
        journal.append(&metadata).await?;
    }
    Ok(())
}

fn directory_finalize_metadata(
    source: &Entry,
    destination: Option<&Entry>,
    policy: ComparisonPolicy,
) -> Result<Option<FinalizeMetadata>> {
    if !source.is_directory()
        || destination.is_some_and(|entry| !entry.is_directory())
        || (!policy.preserve_permissions && !policy.preserve_times)
    {
        return Ok(None);
    }

    let final_entry = match destination {
        Some(destination)
            if policy.ignore_existing
                || (policy.update_only && destination.modified > source.modified) =>
        {
            destination
        }
        _ => source,
    };
    let unix_mode = if policy.preserve_permissions {
        Some(final_entry.unix_mode.ok_or_else(|| {
            RemotePushLowerError::MissingPreservedMode(source.path.as_path().to_path_buf())
        })?)
    } else {
        None
    };
    let modified = policy.preserve_times.then_some(final_entry.modified);

    Ok(Some(FinalizeMetadata {
        path: source.path.clone(),
        kind: source.kind,
        unix_mode,
        modified,
    }))
}
async fn collect_one(
    workers: &mut JoinSet<std::result::Result<Option<TransferSummary>, RemotePushError>>,
    summary: &mut RemotePushSummary,
) -> Result<()> {
    let result = workers
        .join_next()
        .await
        .ok_or_else(|| RemotePushControllerError::Worker("worker set ended early".to_string()))?
        .map_err(|error| RemotePushControllerError::Worker(error.to_string()))??;
    record_transfer(summary, result)
}

fn record_transfer(
    summary: &mut RemotePushSummary,
    transfer: Option<TransferSummary>,
) -> Result<()> {
    let Some(transfer) = transfer else {
        return Ok(());
    };
    summary.files_transferred = checked_add(summary.files_transferred, 1, "file transfer")?;
    if transfer.reused_bytes > 0 {
        summary.delta_files = checked_add(summary.delta_files, 1, "delta file")?;
    }
    summary.literal_bytes = checked_add(
        summary.literal_bytes,
        transfer.literal_bytes,
        "literal-byte",
    )?;
    summary.reused_bytes = checked_add(summary.reused_bytes, transfer.reused_bytes, "reused-byte")?;
    Ok(())
}

fn record_preview_operation(preview: &mut RemotePushPreview, operation: &SyncOp) -> Result<()> {
    match operation {
        SyncOp::Create { source } => match source.kind {
            EntryKind::File => {
                preview.files_created =
                    checked_add(preview.files_created, 1, "preview created file")?;
                preview.bytes_to_create =
                    checked_add(preview.bytes_to_create, source.size, "preview create byte")?;
            }
            EntryKind::Directory => {
                preview.dirs_created =
                    checked_add(preview.dirs_created, 1, "preview created directory")?;
            }
            EntryKind::Symlink => {
                preview.symlinks_created =
                    checked_add(preview.symlinks_created, 1, "preview created symlink")?;
            }
        },
        SyncOp::Update { source, .. } | SyncOp::Replace { source, .. } => match source.kind {
            EntryKind::File => {
                preview.files_updated =
                    checked_add(preview.files_updated, 1, "preview updated file")?;
                preview.bytes_to_update =
                    checked_add(preview.bytes_to_update, source.size, "preview update byte")?;
            }
            EntryKind::Directory => {}
            EntryKind::Symlink => {
                preview.symlinks_created =
                    checked_add(preview.symlinks_created, 1, "preview replaced symlink")?;
            }
        },
        SyncOp::Metadata { source, .. } => {
            if !matches!(source.kind, EntryKind::Directory) {
                preview.files_updated =
                    checked_add(preview.files_updated, 1, "preview metadata update")?;
            }
        }
        SyncOp::Skip { .. } => {
            preview.files_skipped = checked_add(preview.files_skipped, 1, "preview skipped entry")?;
        }
    }
    Ok(())
}

fn record_semantic_operation(summary: &mut RemotePushSummary, operation: &SyncOp) -> Result<()> {
    match operation {
        SyncOp::Create { source } => match source.kind {
            EntryKind::File => {
                summary.files_created = checked_add(summary.files_created, 1, "created file")?;
            }
            EntryKind::Directory => {
                summary.dirs_created = checked_add(summary.dirs_created, 1, "created directory")?;
            }
            EntryKind::Symlink => {
                summary.symlinks_created =
                    checked_add(summary.symlinks_created, 1, "created symlink")?;
            }
        },
        SyncOp::Update { source, .. } | SyncOp::Replace { source, .. } => match source.kind {
            EntryKind::File => {
                summary.files_updated = checked_add(summary.files_updated, 1, "updated file")?;
            }
            EntryKind::Directory => {}
            EntryKind::Symlink => {
                summary.symlinks_created =
                    checked_add(summary.symlinks_created, 1, "replaced symlink")?;
            }
        },
        SyncOp::Metadata { source, .. } => {
            if !matches!(source.kind, EntryKind::Directory) {
                summary.files_updated = checked_add(summary.files_updated, 1, "metadata update")?;
            }
        }
        SyncOp::Skip { .. } => {
            summary.files_skipped = checked_add(summary.files_skipped, 1, "skipped entry")?;
        }
    }
    Ok(())
}

fn checked_add(value: u64, increment: u64, counter: &'static str) -> Result<u64> {
    value
        .checked_add(increment)
        .ok_or(RemotePushControllerError::CounterOverflow(counter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::domain::{Entry, EntryKind, SyncOp, Timestamp};
    use crate::engine::reconcile::BoxError;
    use futures::stream;
    use std::path::PathBuf;

    fn path(value: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(value)).unwrap()
    }

    fn file(value: &str, size: u64, modified: i64) -> Entry {
        let mut entry = Entry::file(path(value), size, Timestamp::new(modified, 0).unwrap());
        entry.unix_mode = Some(0o644);
        entry
    }

    fn directory(value: &str, mode: u32) -> Entry {
        let mut entry = Entry::directory(path(value), Timestamp::UNIX_EPOCH);
        entry.unix_mode = Some(mode);
        entry
    }

    fn entries(values: Vec<Entry>) -> EntryStream {
        Box::pin(stream::iter(values.into_iter().map(Ok::<Entry, BoxError>)))
    }

    #[tokio::test]
    async fn preflight_spools_source_backed_operations_in_order() {
        let source = entries(vec![file("a", 1, 1), file("c", 3, 1)]);
        let destination = entries(vec![file("b", 2, 1), file("c", 3, 1)]);
        let mut plan = preflight_remote_push(
            source,
            destination,
            ComparisonPolicy::default(),
            None,
            |_| true,
        )
        .await
        .unwrap();

        assert_eq!(plan.operations(), 2);
        assert!(matches!(
            plan.reader.next().await.unwrap(),
            Some(SyncOp::Create { source }) if source.path == path("a")
        ));
        assert!(matches!(
            plan.reader.next().await.unwrap(),
            Some(SyncOp::Skip { path: value, .. }) if value == path("c")
        ));
        assert!(plan.reader.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn semantic_scope_skips_work_without_hiding_source_from_delete_preflight() {
        let source = entries(vec![file("parent/keep", 1, 1)]);
        let destination = entries(vec![directory("parent", 0o755), file("remove", 1, 1)]);
        let mut plan = preflight_remote_push_scoped(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                threshold: 100,
                force: false,
            }),
            |_| false,
            |_| true,
        )
        .await
        .unwrap();

        assert_eq!(plan.operations(), 0);
        assert_eq!(plan.eligible_destination_entries(), 2);
        assert_eq!(plan.delete_candidates(), 1);
        assert!(plan.reader.next().await.unwrap().is_none());

        let mut replay = plan.delete.take().unwrap().into_replay();
        assert_eq!(
            replay.next_action().await.unwrap(),
            Some(crate::engine::delete_plan::DeleteAction {
                path: path("remove"),
                is_directory: false,
            })
        );
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn existing_only_source_directory_does_not_queue_finalize_metadata() {
        let mut plan = preflight_remote_push(
            entries(vec![directory("missing", 0o755)]),
            entries(vec![]),
            ComparisonPolicy {
                existing_only: true,
                preserve_times: true,
                ..ComparisonPolicy::default()
            },
            None,
            |_| true,
        )
        .await
        .unwrap();

        assert!(matches!(
            plan.reader.next().await.unwrap(),
            Some(SyncOp::Skip {
                path: value,
                reason: crate::engine::domain::SkipReason::MissingDestination,
            }) if value == path("missing")
        ));
        assert!(plan.reader.next().await.unwrap().is_none());
        assert!(plan.finalize.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn preview_counts_semantic_work_and_exact_deletes_without_execution() {
        let source = entries(vec![
            file("create", 4, 1),
            file("skip", 1, 1),
            file("update", 5, 2),
        ]);
        let destination = entries(vec![
            file("remove", 3, 1),
            file("skip", 1, 1),
            file("update", 3, 1),
        ]);
        let plan = preflight_remote_push(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                threshold: 100,
                force: false,
            }),
            |_| true,
        )
        .await
        .unwrap();

        let preview = preview_remote_push(plan).await.unwrap();
        assert_eq!(preview.planned_operations, 3);
        assert_eq!(preview.files_created, 1);
        assert_eq!(preview.files_updated, 1);
        assert_eq!(preview.files_skipped, 1);
        assert_eq!(preview.delete_candidates, 1);
        assert_eq!(preview.bytes_to_create, 4);
        assert_eq!(preview.bytes_to_update, 5);
    }

    #[tokio::test]
    async fn checksum_preflight_fails_before_execution() {
        let source = entries(vec![file("a", 4, 1)]);
        let destination = entries(vec![file("a", 4, 2)]);
        let policy = ComparisonPolicy {
            mode: crate::engine::planner::ComparisonMode::Checksum,
            ..ComparisonPolicy::default()
        };

        assert!(matches!(
            preflight_remote_push(source, destination, policy, None, |_| true).await,
            Err(RemotePushControllerError::UnsupportedContentComparison(value))
                if value == path("a")
        ));
    }

    #[tokio::test]
    async fn checksum_preflight_finishes_async_content_decisions_before_plan_return() {
        let policy = ComparisonPolicy {
            mode: crate::engine::planner::ComparisonMode::Checksum,
            ..ComparisonPolicy::default()
        };
        let mut equal = preflight_remote_push_scoped_with_content(
            entries(vec![file("a", 4, 1)]),
            entries(vec![file("a", 4, 2)]),
            policy,
            None,
            |_| true,
            |_| true,
            |_source, _destination| async { Ok(true) },
        )
        .await
        .unwrap();
        assert!(matches!(
            equal.reader.next().await.unwrap(),
            Some(SyncOp::Skip { path: value, .. }) if value == path("a")
        ));

        let mut changed = preflight_remote_push_scoped_with_content(
            entries(vec![file("b", 4, 1)]),
            entries(vec![file("b", 4, 2)]),
            policy,
            None,
            |_| true,
            |_| true,
            |_source, _destination| async { Ok(false) },
        )
        .await
        .unwrap();
        assert!(matches!(
            changed.reader.next().await.unwrap(),
            Some(SyncOp::Update { source, .. }) if source.path == path("b")
        ));
    }

    #[tokio::test]
    async fn preflight_integrates_exact_delete_plan_from_same_merge() {
        let source = entries(vec![file("parent/keep", 1, 1)]);
        let destination = entries(vec![
            directory("parent", 0o755),
            file("parent/keep", 1, 1),
            file("remove", 1, 1),
        ]);
        let mut plan = preflight_remote_push(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                threshold: 100,
                force: false,
            }),
            |_| true,
        )
        .await
        .unwrap();

        assert_eq!(plan.operations(), 1);
        assert_eq!(plan.eligible_destination_entries(), 3);
        assert_eq!(plan.delete_candidates(), 1);
        let mut replay = plan.delete.take().unwrap().into_replay();
        assert_eq!(
            replay.next_action().await.unwrap(),
            Some(crate::engine::delete_plan::DeleteAction {
                path: path("remove"),
                is_directory: false,
            })
        );
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn integrated_delete_threshold_fails_before_plan_is_returned() {
        let source = entries(vec![file("keep", 1, 1)]);
        let destination = entries(vec![file("keep", 1, 1), file("remove", 1, 1)]);

        assert!(matches!(
            preflight_remote_push(
                source,
                destination,
                ComparisonPolicy::default(),
                Some(DeletePolicy {
                    threshold: 49,
                    force: false,
                }),
                |_| true,
            )
            .await,
            Err(RemotePushControllerError::DeletePlan(
                DeletePlanError::ThresholdExceeded {
                    eligible_destination_entries: 2,
                    delete_candidates: 1,
                    threshold: 49,
                }
            ))
        ));
    }

    #[tokio::test]
    async fn preflight_journals_equal_directory_time_for_post_namespace_restore() {
        let modified = Timestamp::new(123, 456).unwrap();
        let mut source_directory = directory("parent", 0o755);
        source_directory.modified = modified;
        let destination_directory = source_directory.clone();
        let mut plan = preflight_remote_push(
            entries(vec![source_directory]),
            entries(vec![destination_directory]),
            ComparisonPolicy {
                preserve_times: true,
                ..ComparisonPolicy::default()
            },
            None,
            |_| true,
        )
        .await
        .unwrap();

        assert!(matches!(
            plan.reader.next().await.unwrap(),
            Some(SyncOp::Skip { path: value, .. }) if value == path("parent")
        ));
        assert_eq!(
            plan.finalize.next().await.unwrap(),
            Some(FinalizeMetadata {
                path: path("parent"),
                kind: EntryKind::Directory,
                unix_mode: None,
                modified: Some(modified),
            })
        );
        assert!(plan.finalize.next().await.unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_push_commits_delete_before_restoring_parent_metadata() {
        use crate::endpoint::local_entry_scan::local_entry_stream;
        use crate::engine::scan::{EntryMetadataRequest, ScanRequest};
        use crate::engine::scheduler::{ResourceBudget, Scheduler};
        use crate::protocol::Operation;
        use crate::remote::router::RouterConfig;
        use crate::remote::runtime::{ClientRemoteSession, IncomingRequest, ServerRemoteSession};
        use crate::transfer::delta::BasisIndexLimits;
        use std::fs::{File, FileTimes, Permissions};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::time::{Duration, SystemTime};

        let source_root = tempfile::TempDir::new().unwrap();
        let destination_root = tempfile::TempDir::new().unwrap();
        let source_parent = source_root.path().join("parent");
        let destination_parent = destination_root.path().join("parent");
        std::fs::create_dir(&source_parent).unwrap();
        std::fs::create_dir(&destination_parent).unwrap();
        std::fs::write(source_parent.join("new"), b"new").unwrap();
        std::fs::write(destination_parent.join("remove"), b"old").unwrap();
        std::fs::set_permissions(&source_parent, Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&destination_parent, Permissions::from_mode(0o755)).unwrap();

        let fixed_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::open(&source_parent)
            .unwrap()
            .set_times(FileTimes::new().set_modified(fixed_time))
            .unwrap();
        File::open(&destination_parent)
            .unwrap()
            .set_times(FileTimes::new().set_modified(fixed_time))
            .unwrap();

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        let server = tokio::spawn(async move {
            let mut session =
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default())
                    .await
                    .unwrap();
            let scan_handler = session.scan_handler();
            let file_handler = session.file_handler();
            let mutation_handler = session.mutation_handler();
            let metadata_handler = session.metadata_handler();
            let mut order = Vec::new();

            for _ in 0..4 {
                match session.next_request().await.unwrap().unwrap() {
                    IncomingRequest::Scan(incoming) => {
                        order.push("scan");
                        scan_handler.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::File(incoming) => {
                        order.push("file");
                        file_handler.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Mutation(incoming) => {
                        order.push("mutation");
                        mutation_handler.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Metadata(incoming) => {
                        order.push("metadata");
                        metadata_handler.serve(incoming).await.unwrap();
                    }
                    IncomingRequest::Hash(_) => panic!("unexpected hash request"),
                    IncomingRequest::Signatures(_) => panic!("unexpected signature request"),
                }
            }
            order
        });

        let session = ClientRemoteSession::connect(
            client_reader,
            client_writer,
            Operation::Push,
            destination_root.path(),
            RouterConfig::default(),
        )
        .await
        .unwrap();
        let handle = session.request_handle();
        let scan_request = ScanRequest {
            respect_gitignore: false,
            include_git_dir: true,
            max_depth: None,
            metadata: EntryMetadataRequest {
                unix_mode: true,
                symlink_target: true,
                identity: true,
                hardlink_group: false,
            },
        };
        let destination = handle.scan(scan_request).await.unwrap();
        let source = local_entry_stream(source_root.path().to_path_buf(), scan_request);
        let comparison = ComparisonPolicy {
            preserve_permissions: true,
            preserve_times: true,
            ..ComparisonPolicy::default()
        };
        let plan = preflight_remote_push(
            source,
            destination,
            comparison,
            Some(DeletePolicy {
                threshold: 100,
                force: false,
            }),
            |_| true,
        )
        .await
        .unwrap();

        let executor = RemotePushExecutor::new(
            source_root.path().to_path_buf(),
            handle,
            Scheduler::new(ResourceBudget::default()).unwrap(),
            BasisIndexLimits::default(),
        );
        let controller = RemotePushController::new(executor, NonZeroUsize::new(4).unwrap());
        let summary = controller.execute(plan).await.unwrap();
        let order = server.await.unwrap();

        assert_eq!(order, vec!["scan", "file", "mutation", "metadata"]);
        assert_eq!(summary.planned_operations, 2);
        assert_eq!(summary.main_operations, 1);
        assert_eq!(summary.delete_candidates, 1);
        assert_eq!(summary.deleted_entries, 1);
        assert_eq!(summary.finalized_metadata, 1);
        assert_eq!(summary.files_transferred, 1);
        assert_eq!(
            std::fs::read(destination_parent.join("new")).unwrap(),
            b"new"
        );
        assert!(!destination_parent.join("remove").exists());

        let source_metadata = std::fs::metadata(&source_parent).unwrap();
        let destination_metadata = std::fs::metadata(&destination_parent).unwrap();
        assert_eq!(destination_metadata.mode() & 0o7777, 0o700);
        assert_eq!(destination_metadata.mtime(), source_metadata.mtime());
        assert_eq!(
            destination_metadata.mtime_nsec(),
            source_metadata.mtime_nsec()
        );
    }
}
