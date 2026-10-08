use crate::engine::delete_plan::DeleteAction;
use crate::engine::delete_plan::{DeletePlan, DeletePlanError, DeletePolicy, DeleteTracker};
use crate::engine::domain::{Entry, EntryKind, RelativePath, SyncOp};
use crate::engine::finalize_journal::{
    DirectoryTarget, FinalizeJournal, FinalizeJournalError, FinalizeJournalReader, FinalizeMetadata,
};
use crate::engine::plan_journal::{PlanJournal, PlanJournalError, PlanJournalReader};
use crate::engine::planner::{
    finish_content_comparison, plan_entry, ComparisonPolicy, PlanDecision,
};
use crate::engine::reconcile::{EngineError, EntryStream, OrderedReconciler, ReconcileItem};

use crate::engine::planner::ExecutionPolicy;
use crate::engine::work::WorkItem;
use crate::engine::work::{TransferSummary, WorkResult};
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::task::JoinSet;

#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    #[error(transparent)]
    Reconcile(#[from] EngineError),

    #[error(transparent)]
    DeletePlan(#[from] DeletePlanError),

    #[error(transparent)]
    PlanJournal(#[from] PlanJournalError),

    #[error(transparent)]
    FinalizeJournal(#[from] FinalizeJournalError),

    #[error(transparent)]
    Namespace(#[from] crate::engine::namespace::NamespacePreflightError),

    #[error("sync {phase} failed: {source}")]
    Backend {
        phase: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("metadata preservation requires Unix mode metadata for {0}")]
    MissingPreservedMode(RelativePath),

    #[error("committed directory finalization failed for {path}: {source}")]
    CommittedDirectoryFinalization {
        path: RelativePath,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("directory observation is missing for {0}")]
    MissingDirectoryIdentity(RelativePath),

    #[error("invalid directory preparation receipt for {0}")]
    InvalidDirectoryReceipt(RelativePath),

    #[error("content comparison was not supplied for {0}")]
    UnsupportedContentComparison(RelativePath),

    #[error(
        "unsupported type transition at '{path}': a {source_kind:?} cannot transactionally replace a {destination_kind:?}"
    )]
    UnsupportedTypeTransition {
        path: RelativePath,
        source_kind: crate::engine::domain::EntryKind,
        destination_kind: crate::engine::domain::EntryKind,
    },

    #[error(
        "unsupported type transition at '{path}': discarding a non-empty directory ({discarded_entries} entries) requires --delete"
    )]
    NonEmptyDirectoryReplacementRequiresDelete {
        path: RelativePath,
        discarded_entries: u64,
    },

    #[error(
        "unsupported type transition at '{path}': directory contains protected or excluded entry '{descendant}'"
    )]
    CannotReplaceDirectoryWithProtectedDescendant {
        path: RelativePath,
        descendant: RelativePath,
    },

    #[error("unsupported type transition at '{path}': non-empty directory replacement ({discarded_entries} entries) requires identity-bearing descendant cleanup authorization, which is not implemented")]
    NonEmptyDirectoryReplacementUnsupported {
        path: RelativePath,
        discarded_entries: u64,
    },

    #[error("preflight failed ({operation}) and scan draining failed ({drain})")]
    PreflightDrain {
        operation: Box<ControllerError>,
        drain: EngineError,
    },

    #[error("sync worker failed: {0}")]
    Worker(String),

    #[error("sync {0} counter overflow")]
    CounterOverflow(&'static str),
}

pub type Result<T> = std::result::Result<T, ControllerError>;

impl ControllerError {
    pub fn backend(
        phase: &'static str,
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Backend {
            phase,
            source: Box::new(error),
        }
    }
}

/// Disk-backed semantic result of a complete no-mutation ordered merge.
///
/// Source-backed semantic work, requested directory final metadata, and exact
/// destination-only deletion candidates are derived from the same ordered stream.
/// Returning this value means the
/// whole merge completed and any enabled deletion threshold passed without
/// mutating either endpoint.
pub struct SyncPlan {
    reader: PlanJournalReader,
    finalize: FinalizeJournalReader,
    operations: u64,
    delete: Option<DeletePlan>,
    execution_policy: ExecutionPolicy,
}

impl SyncPlan {
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

impl std::fmt::Debug for SyncPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncPlan")
            .field("operations", &self.operations)
            .field(
                "eligible_destination_entries",
                &self.eligible_destination_entries(),
            )
            .field("delete_candidates", &self.delete_candidates())
            .finish()
    }
}

/// Complete the source/destination merge without mutating either endpoint and
/// spool semantic planner output plus any exact delete plan to disk.
///
/// `delete_in_scope` is evaluated only when deletion is enabled. Keeping scope
/// policy at this boundary lets excluded destination subtrees protect candidate
/// ancestors while the engine retains ownership of exact counting and replay.
pub async fn preflight_sync(
    source: EntryStream,
    destination: EntryStream,
    policy: ComparisonPolicy,
    delete_policy: Option<DeletePolicy>,
    delete_in_scope: impl FnMut(&Entry) -> bool,
) -> Result<SyncPlan> {
    preflight_sync_scoped(
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
pub async fn preflight_sync_scoped(
    source: EntryStream,
    destination: EntryStream,
    policy: ComparisonPolicy,
    delete_policy: Option<DeletePolicy>,
    plan_in_scope: impl FnMut(&Entry) -> bool,
    delete_in_scope: impl FnMut(&Entry) -> bool,
) -> Result<SyncPlan> {
    preflight_sync_scoped_with_content(
        source,
        destination,
        policy,
        delete_policy,
        plan_in_scope,
        delete_in_scope,
        |source, _destination| async move {
            Err::<bool, _>(ControllerError::UnsupportedContentComparison(source.path))
        },
    )
    .await
}

fn is_descendant_of(path: &RelativePath, ancestor: &RelativePath) -> bool {
    path.as_path() != ancestor.as_path() && path.as_path().starts_with(ancestor.as_path())
}

/// Complete remote-push preflight with asynchronous content comparison
/// for planner decisions that cannot be resolved from scan metadata.
///
/// The comparison future runs while the controller is still in the
/// no-mutation phase. A comparison failure therefore prevents a plan,
/// delete replay, and all namespace or file mutations.
pub async fn preflight_sync_scoped_with_content<F, Fut>(
    source: EntryStream,
    destination: EntryStream,
    policy: ComparisonPolicy,
    delete_policy: Option<DeletePolicy>,
    plan_in_scope: impl FnMut(&Entry) -> bool,
    delete_in_scope: impl FnMut(&Entry) -> bool,
    compare_content: F,
) -> Result<SyncPlan>
where
    F: FnMut(Entry, Entry) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let mut reconciler = OrderedReconciler::new(source, destination);
    let result = collect_preflight_plan(
        &mut reconciler,
        policy,
        delete_policy,
        plan_in_scope,
        delete_in_scope,
        compare_content,
    )
    .await;
    let drained = reconciler.close().await;
    match (result, drained) {
        (Err(operation), Err(drain)) => Err(ControllerError::PreflightDrain {
            operation: Box::new(operation),
            drain,
        }),
        (Err(operation), _) => Err(operation),
        (_, Err(drain)) => Err(drain.into()),
        (Ok(plan), Ok(())) => Ok(plan),
    }
}

async fn collect_preflight_plan<F, Fut>(
    reconciler: &mut OrderedReconciler,
    policy: ComparisonPolicy,
    delete_policy: Option<DeletePolicy>,
    mut plan_in_scope: impl FnMut(&Entry) -> bool,
    mut delete_in_scope: impl FnMut(&Entry) -> bool,
    mut compare_content: F,
) -> Result<SyncPlan>
where
    F: FnMut(Entry, Entry) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let mut journal = PlanJournal::new().await?;
    let mut finalize = FinalizeJournal::new().await?;
    let execution_policy = ExecutionPolicy {
        preserve_permissions: policy.preserve_permissions,
        preserve_times: policy.preserve_times,
    };
    let mut delete = match delete_policy {
        Some(policy) => Some(DeleteTracker::new(policy).await?),
        None => None,
    };
    let mut collision_detector =
        crate::engine::namespace::NamespaceCollisionDetector::new(policy.namespace_semantics);
    let mut operations = 0_u64;
    let mut active_replaced_dir: Option<(RelativePath, u64)> = None;
    let mut unsupported_nonempty = None;

    while let Some(item) = reconciler.next().await? {
        let item_path = match &item {
            ReconcileItem::SourceOnly(source) => &source.path,
            ReconcileItem::Matched { source, .. } => &source.path,
            ReconcileItem::DestinationOnly(destination) => &destination.path,
        };

        if let Some((replaced_path, _)) = &active_replaced_dir {
            if !is_descendant_of(item_path, replaced_path) {
                if let Some((path, count)) = active_replaced_dir.take() {
                    if count > 0 && delete.is_none() {
                        return Err(
                            ControllerError::NonEmptyDirectoryReplacementRequiresDelete {
                                path,
                                discarded_entries: count,
                            },
                        );
                    }
                    if count > 0 {
                        unsupported_nonempty.get_or_insert((path, count));
                    }
                }
            }
        }

        if let Some((replaced_path, discarded_count)) = &mut active_replaced_dir {
            match item {
                ReconcileItem::DestinationOnly(destination) => {
                    let in_scope = delete_in_scope(&destination);
                    if !in_scope {
                        return Err(
                            ControllerError::CannotReplaceDirectoryWithProtectedDescendant {
                                path: replaced_path.clone(),
                                descendant: destination.path,
                            },
                        );
                    }
                    *discarded_count = checked_add(*discarded_count, 1, "discarded descendant")?;
                    if let Some(delete) = &mut delete {
                        delete
                            .observe_replaced_directory_descendant(&destination)
                            .await?;
                    }
                    collision_detector.record(&destination.path).await?;
                    continue;
                }
                ReconcileItem::SourceOnly(source) | ReconcileItem::Matched { source, .. } => {
                    return Err(ControllerError::UnsupportedTypeTransition {
                        path: source.path,
                        source_kind: source.kind,
                        destination_kind: crate::engine::domain::EntryKind::Directory,
                    });
                }
            }
        }

        let decision = match item {
            ReconcileItem::SourceOnly(source) => {
                if let Some(delete) = &mut delete {
                    delete.observe_source_only(&source).await?;
                }
                if !plan_in_scope(&source) {
                    continue;
                }
                collision_detector.record(&source.path).await?;
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
                collision_detector.record(&source.path).await?;
                append_directory_finalize(&mut finalize, &source, Some(&destination), policy)
                    .await?;
                plan_entry(source, Some(destination), policy)
            }
            ReconcileItem::DestinationOnly(destination) => {
                let in_scope = delete_in_scope(&destination);
                if let Some(delete) = &mut delete {
                    delete
                        .observe_destination_only(&destination, in_scope)
                        .await?;
                }
                collision_detector.record(&destination.path).await?;
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
        if let SyncOp::Replace {
            source,
            destination,
        } = &operation
        {
            if source.is_directory() {
                return Err(ControllerError::UnsupportedTypeTransition {
                    path: source.path.clone(),
                    source_kind: source.kind,
                    destination_kind: destination.kind,
                });
            }
            if destination.is_directory() {
                if !delete_in_scope(destination) {
                    return Err(
                        ControllerError::CannotReplaceDirectoryWithProtectedDescendant {
                            path: destination.path.clone(),
                            descendant: destination.path.clone(),
                        },
                    );
                }
                active_replaced_dir = Some((destination.path.clone(), 0));
            }
        }
        journal.append(&operation).await?;
        operations = checked_add(operations, 1, "operation")?;
    }

    if let Some((path, count)) = active_replaced_dir.take() {
        if count > 0 && delete.is_none() {
            return Err(
                ControllerError::NonEmptyDirectoryReplacementRequiresDelete {
                    path,
                    discarded_entries: count,
                },
            );
        }
        if count > 0 {
            unsupported_nonempty.get_or_insert((path, count));
        }
    }

    // Alias proof completes with the rest of preflight: no mutation may run
    // before both the collision check and the delete threshold have passed.
    collision_detector.finish().await?;
    let delete = match delete {
        Some(delete) => Some(delete.finish().await?),
        None => None,
    };
    // Counting, threshold and protected-descendant validation above remain
    // authoritative. A path-only subtree sweep cannot consume that authority:
    // until descendant identities reach cleanup, refuse before any execution.
    if let Some((path, discarded_entries)) = unsupported_nonempty {
        return Err(ControllerError::NonEmptyDirectoryReplacementUnsupported {
            path,
            discarded_entries,
        });
    }
    Ok(SyncPlan {
        reader: journal.seal().await?,
        finalize: finalize.seal().await?,
        operations,
        delete,
        execution_policy,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncPreview {
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

/// One planned deletion surfaced to `--diff` dry-run detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewDelete {
    pub path: RelativePath,
    pub is_directory: bool,
}

/// Consume a completed remote-push plan without executing endpoint mutations.
///
/// Dry-run uses the exact same full-tree preflight as execution, including
/// deletion protection and threshold checks, then reads only the disk-backed
/// semantic journal. Dropping the finalize journals performs no remote
/// operations.
///
/// `diff_detail` optionally receives one line per planned operation and one
/// per delete candidate, the `--diff` byte-accounting view (`Would create:
/// path (size)`).
pub async fn preview_sync(
    plan: SyncPlan,
    mut diff_detail: impl FnMut(PreviewOp),
) -> Result<SyncPreview> {
    let SyncPlan {
        mut reader,
        operations,
        delete,
        ..
    } = plan;
    let mut preview = SyncPreview {
        planned_operations: operations,
        delete_candidates: delete.as_ref().map_or(0, DeletePlan::delete_candidates),
        ..SyncPreview::default()
    };

    while let Some(operation) = reader.next().await? {
        diff_detail(PreviewOp::Operation(&operation));
        record_preview_operation(&mut preview, &operation)?;
    }
    if let Some(delete) = delete {
        let mut replay = delete.into_replay();
        while let Some(action) = replay.next_action().await? {
            diff_detail(PreviewOp::Delete(PreviewDelete {
                path: action.path,
                is_directory: action.kind == EntryKind::Directory,
            }));
        }
    }
    Ok(preview)
}

/// A `--diff` dry-run detail item: a planned transfer operation or a planned
/// deletion.
pub enum PreviewOp<'a> {
    Operation(&'a SyncOp),
    Delete(PreviewDelete),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncSummary {
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

/// Executor interface shared by every v3 direction. The controller owns the
/// safety ordering — preorder directories, bounded concurrent leaves,
/// reverse-order deletes, reverse-order finalize — and executors own
/// per-item admission and the concrete mutation/fetch. `remove_verified_-
/// parity_source` is push-only (`--remove-source-files` removes the local
/// source after commit); pulls have no local source to remove, so the
/// default is a no-op.
pub trait SyncPlanExecutor: Send + Sync + 'static {
    type Action: Send + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    fn lower(
        &self,
        op: SyncOp,
        policy: ExecutionPolicy,
    ) -> std::result::Result<Option<WorkItem<Self::Action>>, Self::Error>;
    fn is_directory_action(&self, action: &Self::Action) -> bool;

    fn execute(
        &self,
        item: WorkItem<Self::Action>,
    ) -> impl Future<Output = std::result::Result<WorkResult, Self::Error>> + Send;
    fn execute_delete(
        &self,
        action: DeleteAction,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;
    fn execute_finalize(
        &self,
        metadata: FinalizeMetadata,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;
    fn finish_deferred_source_removals(
        &self,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;
    fn remove_unchanged_source(
        &self,
        source: &Entry,
        destination: &Entry,
        policy: ExecutionPolicy,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;
}

/// Executes one preflighted v3 sync with bounded task fan-out.
///
/// Directory creation is awaited in preorder before reconciliation advances to
/// descendants. Independent leaf work may run concurrently, but at most
/// `max_in_flight` worker futures exist even before scheduler admission. All
/// directory metadata is preflighted into a reverse journal and replayed
/// child-before-parent only after main work and reverse deletes complete.
pub struct SyncController<E: SyncPlanExecutor> {
    executor: Arc<E>,
    max_in_flight: NonZeroUsize,
}

impl<E: SyncPlanExecutor> SyncController<E> {
    pub fn new(executor: E, max_in_flight: NonZeroUsize) -> Self {
        Self {
            executor: Arc::new(executor),
            max_in_flight,
        }
    }

    pub async fn execute(&self, plan: SyncPlan) -> Result<SyncSummary> {
        let SyncPlan {
            mut reader,
            mut finalize,
            operations,
            delete,
            execution_policy,
        } = plan;
        let delete_candidates = delete.as_ref().map_or(0, DeletePlan::delete_candidates);
        let mut summary = SyncSummary {
            planned_operations: operations,
            delete_candidates,
            ..SyncSummary::default()
        };
        let mut workers = JoinSet::<std::result::Result<WorkResult, E::Error>>::new();
        let mut runtime_finalize = FinalizeJournal::new().await?;
        let mut next_directory = finalize.next_forward().await?;

        let main_result: Result<()> = async {
            while let Some(operation) = reader.next().await? {
                record_semantic_operation(&mut summary, &operation)?;
                let directory = if next_directory
                    .as_ref()
                    .is_some_and(|record| &record.path == operation.path())
                {
                    let record = next_directory.take();
                    next_directory = finalize.next_forward().await?;
                    record
                } else {
                    None
                };
                if let Some(record) = &directory {
                    if matches!(record.target, DirectoryTarget::Observed(_)) {
                        runtime_finalize.append(record).await?;
                    }
                }
                if let SyncOp::Unchanged {
                    source,
                    destination,
                    comparison,
                } = &operation
                {
                    let remove_after_preservation = *comparison
                        == crate::engine::domain::ContentComparison::Blake3
                        && source.is_file();
                    let source = source.clone();
                    let destination = destination.clone();
                    let operation_path = operation.path().clone();
                    let lowered = self
                        .executor
                        .lower(operation, execution_policy)
                        .map_err(|error| ControllerError::backend("execution", error))?;
                    if let Some(main) = lowered {
                        summary.main_operations =
                            checked_add(summary.main_operations, 1, "main operation")?;
                        if self.executor.is_directory_action(main.action()) {
                            return Err(ControllerError::InvalidDirectoryReceipt(operation_path));
                        }
                        let result = self
                            .executor
                            .execute(main)
                            .await
                            .map_err(|error| ControllerError::backend("execution", error))?;
                        record_work_result(&mut summary, result)?;
                    } else if remove_after_preservation {
                        self.executor
                            .remove_unchanged_source(&source, &destination, execution_policy)
                            .await
                            .map_err(|error| ControllerError::backend("execution", error))?;
                    }
                    continue;
                }
                let operation_path = operation.path().clone();
                let lowered = self
                    .executor
                    .lower(operation, execution_policy)
                    .map_err(|error| ControllerError::backend("execution", error))?;
                let Some(main) = lowered else {
                    if directory.is_some_and(|record| record.target == DirectoryTarget::Create) {
                        return Err(ControllerError::InvalidDirectoryReceipt(operation_path));
                    }
                    continue;
                };
                summary.main_operations =
                    checked_add(summary.main_operations, 1, "main operation")?;

                if self.executor.is_directory_action(main.action()) {
                    let result = self
                        .executor
                        .execute(main)
                        .await
                        .map_err(|error| ControllerError::backend("execution", error))?;
                    let Some(mut record) = directory else {
                        return Err(ControllerError::InvalidDirectoryReceipt(operation_path));
                    };
                    let WorkResult::DirectoryPrepared(identity) = result else {
                        return Err(ControllerError::InvalidDirectoryReceipt(record.path));
                    };
                    if record.target != DirectoryTarget::Create {
                        return Err(ControllerError::InvalidDirectoryReceipt(record.path));
                    }
                    record.target = DirectoryTarget::Observed(identity);
                    runtime_finalize.append(&record).await?;
                    continue;
                }

                while workers.len() >= self.max_in_flight.get() {
                    collect_one::<E>(&mut workers, &mut summary).await?;
                }
                let executor = Arc::clone(&self.executor);
                workers.spawn(async move { executor.execute(main).await });
            }

            while !workers.is_empty() {
                collect_one::<E>(&mut workers, &mut summary).await?;
            }

            Ok(())
        }
        .await;
        if let Err(error) = main_result {
            // Stop admitting work, but do not cancel an admitted transaction:
            // it may be crossing its publication point or awaiting blocking
            // filesystem work. Finish every worker before releasing session
            // ownership; failed main work never authorizes delete/finalize.
            while let Some(result) = workers.join_next().await {
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(secondary)) => {
                        tracing::warn!(error = %secondary, "additional sync worker failure")
                    }
                    Err(secondary) => {
                        tracing::warn!(error = %secondary, "sync worker join failed while draining")
                    }
                }
            }
            return Err(error);
        }

        if let Some(record) = next_directory {
            return Err(ControllerError::InvalidDirectoryReceipt(record.path));
        }

        let mut finalize = runtime_finalize.seal().await?;

        if let Some(delete) = delete {
            let mut replay = delete.into_replay();
            while let Some(action) = replay.next_action().await? {
                self.executor
                    .execute_delete(action)
                    .await
                    .map_err(|error| ControllerError::backend("execution", error))?;
                summary.deleted_entries = checked_add(summary.deleted_entries, 1, "deleted entry")?;
            }
        }

        while let Some(metadata) = finalize.next().await? {
            let path = metadata.path.clone();
            self.executor
                .execute_finalize(metadata)
                .await
                .map_err(|error| ControllerError::CommittedDirectoryFinalization {
                    path,
                    source: Box::new(error),
                })?;
            summary.finalized_metadata =
                checked_add(summary.finalized_metadata, 1, "finalized metadata")?;
        }

        self.executor
            .finish_deferred_source_removals()
            .await
            .map_err(|error| ControllerError::backend("execution", error))?;

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
    if !source.is_directory() || destination.is_some_and(|entry| !entry.is_directory()) {
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
        Some(
            final_entry
                .unix_mode
                .ok_or_else(|| ControllerError::MissingPreservedMode(source.path.clone()))?,
        )
    } else {
        None
    };
    let modified = policy.preserve_times.then_some(final_entry.modified);

    let preserve_source = !destination.is_some_and(|entry| {
        policy.ignore_existing || (policy.update_only && entry.modified > source.modified)
    });
    if !preserve_source && unix_mode.is_none() && modified.is_none() {
        return Ok(None);
    }
    Ok(Some(FinalizeMetadata {
        path: source.path.clone(),
        kind: source.kind,
        source_identity: source
            .identity
            .ok_or_else(|| ControllerError::MissingDirectoryIdentity(source.path.clone()))?,
        target: match destination {
            None => DirectoryTarget::Create,
            Some(entry) => DirectoryTarget::Observed(
                entry
                    .identity
                    .ok_or_else(|| ControllerError::MissingDirectoryIdentity(entry.path.clone()))?,
            ),
        },
        preserve_source,
        unix_mode,
        modified,
    }))
}
async fn collect_one<E: SyncPlanExecutor>(
    workers: &mut JoinSet<std::result::Result<WorkResult, E::Error>>,
    summary: &mut SyncSummary,
) -> Result<()> {
    let result = workers
        .join_next()
        .await
        .ok_or_else(|| ControllerError::Worker("worker set ended early".to_string()))?
        .map_err(|error| ControllerError::Worker(error.to_string()))?;
    match result {
        Ok(result) => record_work_result(summary, result),
        Err(error) => Err(ControllerError::backend("execution", error)),
    }
}

fn record_work_result(summary: &mut SyncSummary, result: WorkResult) -> Result<()> {
    match result {
        WorkResult::Transfer(transfer) => record_transfer(summary, transfer),
        WorkResult::Metadata => Ok(()),
        WorkResult::DirectoryPrepared(_) => Err(ControllerError::Worker(
            "directory executed as leaf work".into(),
        )),
    }
}

fn record_transfer(summary: &mut SyncSummary, transfer: TransferSummary) -> Result<()> {
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

fn record_preview_operation(preview: &mut SyncPreview, operation: &SyncOp) -> Result<()> {
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
        SyncOp::Skip { .. } | SyncOp::Unchanged { .. } => {
            preview.files_skipped = checked_add(preview.files_skipped, 1, "preview skipped entry")?;
        }
    }
    Ok(())
}

fn record_semantic_operation(summary: &mut SyncSummary, operation: &SyncOp) -> Result<()> {
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
        SyncOp::Skip { .. } | SyncOp::Unchanged { .. } => {
            summary.files_skipped = checked_add(summary.files_skipped, 1, "skipped entry")?;
        }
    }
    Ok(())
}

fn checked_add(value: u64, increment: u64, counter: &'static str) -> Result<u64> {
    value
        .checked_add(increment)
        .ok_or(ControllerError::CounterOverflow(counter))
}

#[cfg(test)]
mod tests {
    use crate::remote::push::RemotePushExecutor;

    use super::*;
    use crate::engine::delete_plan::DeleteLimit;
    use crate::engine::domain::{Entry, EntryKind, SyncOp, Timestamp};
    use crate::engine::namespace::NamespacePreflightError;
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
        entry.identity = Some(crate::engine::domain::EntryIdentity::from_bytes([7; 32]));
        entry
    }

    fn entries(values: Vec<Entry>) -> EntryStream {
        EntryStream::new(stream::iter(values.into_iter().map(Ok::<Entry, BoxError>)))
    }

    #[tokio::test]
    async fn failed_preflight_stops_both_scans_and_waits_for_native_cleanup() {
        let mut scans = Vec::new();
        let mut gates = Vec::new();
        let mut scratch_paths = Vec::new();
        for source in [true, false] {
            let scratch = tempfile::tempdir().unwrap();
            scratch_paths.push(scratch.path().to_path_buf());
            let (closed_tx, closed_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let scan = EntryStream::spawn_blocking(1, move |sender| {
                let _scratch = scratch;
                if source {
                    let _ = sender.blocking_send(Err(Box::new(std::io::Error::other(
                        "injected source failure",
                    ))));
                }
                let entry = Entry::file(
                    RelativePath::new("queued").unwrap(),
                    1,
                    Timestamp::UNIX_EPOCH,
                );
                while sender.blocking_send(Ok(entry.clone())).is_ok() {}
                closed_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            });
            // Source filters must retain, rather than erase, worker ownership.
            scans.push(scan.filter_map(|entry| std::future::ready(Some(entry))));
            gates.push((closed_rx, release_tx));
        }
        let destination = scans.pop().unwrap();
        let source = scans.pop().unwrap();
        let task = tokio::spawn(preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            None,
            |_| true,
        ));
        let ((source_closed, source_release), (dest_closed, dest_release)) =
            (gates.remove(0), gates.remove(0));
        let queues_closed = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Duration::from_secs(10);
            source_closed.recv_timeout(deadline).is_ok()
                && dest_closed.recv_timeout(deadline).is_ok()
        })
        .await
        .unwrap();
        let returned_early = task.is_finished();
        let scratch_retained = scratch_paths.iter().all(|path| path.exists());
        // Release native work even when testing a broken drain implementation.
        let _ = source_release.send(());
        let _ = dest_release.send(());
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            queues_closed,
            "both queues must close before joining either worker"
        );
        assert!(!returned_early, "preflight detached admitted native work");
        assert!(scratch_retained);
        assert!(matches!(
            outcome,
            Err(ControllerError::Reconcile(EngineError::Endpoint {
                side: crate::engine::reconcile::Side::Source,
                ..
            }))
        ));
        assert!(
            scratch_paths.iter().all(|path| !path.exists()),
            "worker scratch survived preflight return"
        );
    }

    struct FailingExecutor {
        started: Arc<tokio::sync::Notify>,
        failed: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        completed: Arc<std::sync::atomic::AtomicBool>,
        tail_mutations: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SyncPlanExecutor for FailingExecutor {
        type Action = Entry;
        type Error = std::io::Error;

        fn lower(
            &self,
            op: SyncOp,
            _policy: ExecutionPolicy,
        ) -> std::io::Result<Option<WorkItem<Entry>>> {
            match op {
                SyncOp::Create { source } => Ok(Some(WorkItem::new(source, Default::default()))),
                _ => Ok(None),
            }
        }

        fn is_directory_action(&self, action: &Entry) -> bool {
            action.is_directory()
        }

        async fn execute(&self, item: WorkItem<Entry>) -> std::io::Result<WorkResult> {
            if item.action().path == path("a") {
                self.started.notify_one();
                self.release.notified().await;
                self.completed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(WorkResult::Metadata)
            } else {
                self.started.notified().await;
                self.failed.notify_one();
                Err(std::io::Error::other("injected main-work failure"))
            }
        }

        async fn execute_delete(&self, _action: DeleteAction) -> std::io::Result<()> {
            self.tail_mutations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn execute_finalize(&self, _metadata: FinalizeMetadata) -> std::io::Result<()> {
            self.tail_mutations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn finish_deferred_source_removals(&self) -> std::io::Result<()> {
            Ok(())
        }

        async fn remove_unchanged_source(
            &self,
            _source: &Entry,
            _destination: &Entry,
            _policy: ExecutionPolicy,
        ) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn main_failure_drains_admitted_work_without_delete_or_finalize() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let plan = preflight_sync(
            entries(vec![
                directory("0dir", 0o755),
                file("a", 1, 1),
                file("b", 1, 1),
            ]),
            entries(vec![directory("0dir", 0o755), file("obsolete", 1, 1)]),
            ComparisonPolicy {
                preserve_times: true,
                ..Default::default()
            },
            Some(DeletePolicy {
                limit: DeleteLimit::Percentage(100),
                force: false,
            }),
            |_| true,
        )
        .await
        .unwrap();
        let failed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let tail_mutations = Arc::new(AtomicUsize::new(0));
        let controller = SyncController::new(
            FailingExecutor {
                started: Arc::new(tokio::sync::Notify::new()),
                failed: Arc::clone(&failed),
                release: Arc::clone(&release),
                completed: Arc::clone(&completed),
                tail_mutations: Arc::clone(&tail_mutations),
            },
            NonZeroUsize::new(2).unwrap(),
        );
        let execution = tokio::spawn(async move { controller.execute(plan).await });
        failed.notified().await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(
            !execution.is_finished(),
            "session returned while a transaction was still active"
        );
        release.notify_one();
        let error = execution.await.unwrap().unwrap_err();
        assert!(matches!(error, ControllerError::Backend { .. }));
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(tail_mutations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn preflight_spools_source_backed_operations_in_order() {
        let source = entries(vec![file("a", 1, 1), file("c", 3, 1)]);
        let destination = entries(vec![file("b", 2, 1), file("c", 3, 1)]);
        let mut plan = preflight_sync(
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
            Some(SyncOp::Unchanged { source, .. }) if source.path == path("c")
        ));
        assert!(plan.reader.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn semantic_scope_skips_work_without_hiding_source_from_delete_preflight() {
        let source = entries(vec![file("parent/keep", 1, 1)]);
        let destination = entries(vec![directory("parent", 0o755), file("remove", 1, 1)]);
        let mut plan = preflight_sync_scoped(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                limit: DeleteLimit::Percentage(100),
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
                kind: EntryKind::File,
                identity: None,
            })
        );
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn existing_only_source_directory_does_not_queue_finalize_metadata() {
        let mut plan = preflight_sync(
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
                source,
                reason: crate::engine::domain::SkipReason::MissingDestination,
            }) if source.path == path("missing")
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
        let plan = preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                limit: DeleteLimit::Percentage(100),
                force: false,
            }),
            |_| true,
        )
        .await
        .unwrap();

        let preview = preview_sync(plan, |_| {}).await.unwrap();
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
            preflight_sync(source, destination, policy, None, |_| true).await,
            Err(ControllerError::UnsupportedContentComparison(value))
                if value == path("a")
        ));
    }

    #[tokio::test]
    async fn checksum_preflight_finishes_async_content_decisions_before_plan_return() {
        let policy = ComparisonPolicy {
            mode: crate::engine::planner::ComparisonMode::Checksum,
            ..ComparisonPolicy::default()
        };
        let mut equal = preflight_sync_scoped_with_content(
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
            Some(SyncOp::Unchanged { source, .. }) if source.path == path("a")
        ));

        let mut changed = preflight_sync_scoped_with_content(
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
        let mut plan = preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                limit: DeleteLimit::Percentage(100),
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
                kind: EntryKind::File,
                identity: None,
            })
        );
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn integrated_delete_threshold_fails_before_plan_is_returned() {
        let source = entries(vec![file("keep", 1, 1)]);
        let destination = entries(vec![file("keep", 1, 1), file("remove", 1, 1)]);

        assert!(matches!(
            preflight_sync(
                source,
                destination,
                ComparisonPolicy::default(),
                Some(DeletePolicy {
                    limit: DeleteLimit::Percentage(49),
                    force: false,
                }),
                |_| true,
            )
            .await,
            Err(ControllerError::DeletePlan(
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
        let mut plan = preflight_sync(
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
            Some(SyncOp::Unchanged { source, .. }) if source.path == path("parent")
        ));
        assert_eq!(
            plan.finalize.next().await.unwrap(),
            Some(FinalizeMetadata {
                path: path("parent"),
                kind: EntryKind::Directory,
                source_identity: crate::engine::domain::EntryIdentity::from_bytes([7; 32]),
                target: DirectoryTarget::Observed(
                    crate::engine::domain::EntryIdentity::from_bytes([7; 32])
                ),
                preserve_source: true,
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
                    IncomingRequest::FileFetch(_) => panic!("unexpected fetch request"),
                    IncomingRequest::Signatures(_) => panic!("unexpected signature request"),
                    IncomingRequest::Xattr(_) => panic!("unexpected xattr request"),
                    IncomingRequest::Acl(_) => panic!("unexpected acl request"),
                    IncomingRequest::BsdFlags(_) => panic!("unexpected bsd flags request"),
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
            follow_symlinks: false,
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
        let plan = preflight_sync(
            source,
            destination,
            comparison,
            Some(DeletePolicy {
                limit: DeleteLimit::Percentage(100),
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
        let controller = SyncController::new(executor, NonZeroUsize::new(4).unwrap());
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

    #[tokio::test]
    async fn preflight_detects_source_case_collision_on_case_insensitive_target() {
        let source = entries(vec![
            file("dir/File.txt", 10, 1),
            file("dir/file.txt", 10, 1),
        ]);
        let destination = entries(vec![]);
        let policy = ComparisonPolicy {
            namespace_semantics:
                crate::engine::namespace::NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED,
            ..ComparisonPolicy::default()
        };

        let err = preflight_sync(source, destination, policy, None, |_| true)
            .await
            .unwrap_err();

        match err {
            ControllerError::Namespace(NamespacePreflightError::Collision(collision)) => {
                assert_eq!(collision.existing, path("dir/File.txt"));
                assert_eq!(collision.colliding, path("dir/file.txt"));
            }
            other => panic!("expected Namespace collision error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn preflight_detects_source_destination_case_collision_on_case_insensitive_target() {
        let source = entries(vec![file("foo.txt", 10, 1)]);
        let destination = entries(vec![file("FOO.txt", 10, 1)]);
        let policy = ComparisonPolicy {
            namespace_semantics:
                crate::engine::namespace::NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED,
            ..ComparisonPolicy::default()
        };

        let err = preflight_sync(source, destination, policy, None, |_| true)
            .await
            .unwrap_err();

        match err {
            ControllerError::Namespace(NamespacePreflightError::Collision(collision)) => {
                assert_eq!(collision.existing, path("FOO.txt"));
                assert_eq!(collision.colliding, path("foo.txt"));
            }
            other => panic!("expected Namespace collision error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn preflight_detects_unicode_normalization_collision() {
        let nfc = "caf\u{e9}.txt";
        let nfd = "cafe\u{301}.txt";
        let source = entries(vec![file(nfd, 10, 1), file(nfc, 10, 1)]);
        let destination = entries(vec![]);
        let policy = ComparisonPolicy {
            namespace_semantics:
                crate::engine::namespace::NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED,
            ..ComparisonPolicy::default()
        };

        let err = preflight_sync(source, destination, policy, None, |_| true)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ControllerError::Namespace(NamespacePreflightError::Collision(_))
        ));
    }

    #[tokio::test]
    async fn preflight_permits_case_differences_on_case_sensitive_target() {
        let source = entries(vec![file("FILE.txt", 10, 1), file("file.txt", 10, 1)]);
        let destination = entries(vec![]);
        let policy = ComparisonPolicy {
            namespace_semantics: crate::engine::namespace::NamespaceSemantics::BYTE_EXACT,
            ..ComparisonPolicy::default()
        };

        let plan = preflight_sync(source, destination, policy, None, |_| true)
            .await
            .unwrap();

        assert_eq!(plan.operations, 2);
    }

    #[tokio::test]
    async fn preflight_rejects_source_directory_replacement_before_subtree_staging() {
        let source = entries(vec![directory("swap", 0o755)]);
        let destination = entries(vec![file("swap", 3, 1)]);
        let err = preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            None,
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ControllerError::UnsupportedTypeTransition { .. }
        ));
    }

    #[tokio::test]
    async fn preflight_rejects_non_empty_directory_replacement_without_delete() {
        let source = entries(vec![file("swap", 3, 1)]);
        let destination = entries(vec![directory("swap", 0o755), file("swap/child", 10, 1)]);
        let err = preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            None,
            |_| true,
        )
        .await
        .unwrap_err();
        match err {
            ControllerError::NonEmptyDirectoryReplacementRequiresDelete {
                path,
                discarded_entries,
            } => {
                assert_eq!(path, RelativePath::new("swap").unwrap());
                assert_eq!(discarded_entries, 1);
            }
            other => panic!(
                "expected NonEmptyDirectoryReplacementRequiresDelete, got {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn preflight_allows_empty_directory_replacement_without_delete() {
        let source = entries(vec![file("swap", 3, 1)]);
        let destination = entries(vec![directory("swap", 0o755)]);
        let plan = preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            None,
            |_| true,
        )
        .await
        .unwrap();
        assert_eq!(plan.operations, 1);
        assert!(plan.delete.is_none());
    }

    #[tokio::test]
    async fn preflight_refuses_nonempty_replacement_even_with_delete() {
        let source = entries(vec![file("swap", 3, 1)]);
        let destination = entries(vec![
            directory("swap", 0o755),
            file("swap/first", 10, 1),
            file("swap/second", 20, 1),
        ]);
        let policy = DeletePolicy {
            limit: crate::engine::delete_plan::DeleteLimit::Unlimited,
            force: false,
        };
        let error = preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(policy),
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ControllerError::NonEmptyDirectoryReplacementUnsupported {
                discarded_entries: 2,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn preflight_enforces_delete_threshold_for_directory_replacement_descendants() {
        let source = entries(vec![file("swap", 3, 1)]);
        let destination = entries(vec![
            directory("swap", 0o755),
            file("swap/first", 10, 1),
            file("swap/second", 20, 1),
        ]);
        let policy = DeletePolicy {
            limit: crate::engine::delete_plan::DeleteLimit::Count(1),
            force: false,
        };
        let err = preflight_sync(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(policy),
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ControllerError::DeletePlan(DeletePlanError::CountExceeded {
                delete_candidates: 2,
                limit: 1,
            })
        ));
    }

    #[tokio::test]
    async fn preflight_rejects_directory_replacement_with_protected_descendant() {
        let source = entries(vec![file("swap", 3, 1)]);
        let destination = entries(vec![
            directory("swap", 0o755),
            file("swap/protected", 10, 1),
        ]);
        let err = preflight_sync_scoped(
            source,
            destination,
            ComparisonPolicy::default(),
            Some(DeletePolicy {
                limit: crate::engine::delete_plan::DeleteLimit::Unlimited,
                force: false,
            }),
            |_| true,
            |dest| dest.path.as_path() != std::path::Path::new("swap/protected"),
        )
        .await
        .unwrap_err();
        match err {
            ControllerError::CannotReplaceDirectoryWithProtectedDescendant { path, descendant } => {
                assert_eq!(path, RelativePath::new("swap").unwrap());
                assert_eq!(descendant, RelativePath::new("swap/protected").unwrap());
            }
            other => panic!(
                "expected CannotReplaceDirectoryWithProtectedDescendant, got {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn preflight_reports_ambiguity_when_semantics_are_unknown() {
        let source = entries(vec![file("FILE.txt", 10, 1), file("file.txt", 10, 1)]);
        let destination = entries(vec![]);
        let policy = ComparisonPolicy {
            namespace_semantics: crate::engine::namespace::NamespaceSemantics::UNSPECIFIED,
            ..ComparisonPolicy::default()
        };

        let err = preflight_sync(source, destination, policy, None, |_| true)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            ControllerError::Namespace(NamespacePreflightError::Ambiguity(_))
        ));
    }
}
