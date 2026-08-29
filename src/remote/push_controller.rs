use crate::engine::delete_plan::{DeletePlan, DeletePlanError, DeletePolicy, DeleteTracker};
use crate::engine::domain::{Entry, RelativePath};
use crate::engine::finalize_journal::{FinalizeJournal, FinalizeJournalError, FinalizeMetadata};
use crate::engine::plan_journal::{PlanJournal, PlanJournalError, PlanJournalReader};
use crate::engine::planner::{plan_entry, ComparisonPolicy, PlanDecision};
use crate::engine::reconcile::{EngineError, EntryStream, OrderedReconciler, ReconcileItem};
use crate::engine::work::WorkItem;
use crate::remote::push::{
    lower_sync_op, RemotePushAction, RemotePushError, RemotePushExecutor, RemotePushLowerError,
    RemotePushPolicy,
};
use crate::remote::transfer::TransferSummary;
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

    #[error("v3 remote checksum comparison is not implemented for {0}")]
    UnsupportedContentComparison(RelativePath),

    #[error("remote push worker failed: {0}")]
    Worker(String),

    #[error("lowered finalize work was not a metadata action")]
    InvalidFinalizeWork,

    #[error("remote push {0} counter overflow")]
    CounterOverflow(&'static str),
}

pub type Result<T> = std::result::Result<T, RemotePushControllerError>;

/// Disk-backed semantic result of a complete no-mutation ordered merge.
///
/// Source-backed semantic work and exact destination-only deletion candidates
/// are derived from the same ordered stream. Returning this value means the
/// whole merge completed and any enabled deletion threshold passed without
/// mutating either endpoint.
pub struct RemotePushPlan {
    reader: PlanJournalReader,
    operations: u64,
    delete: Option<DeletePlan>,
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
    mut delete_in_scope: impl FnMut(&Entry) -> bool,
) -> Result<RemotePushPlan> {
    let mut reconciler = OrderedReconciler::new(source, destination);
    let mut journal = PlanJournal::new().await?;
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
            PlanDecision::NeedContentComparison { source, .. } => {
                return Err(RemotePushControllerError::UnsupportedContentComparison(
                    source.path,
                ));
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
        operations,
        delete,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemotePushSummary {
    pub planned_operations: u64,
    pub main_operations: u64,
    pub delete_candidates: u64,
    pub deleted_entries: u64,
    pub finalized_metadata: u64,
    pub files_transferred: u64,
    pub literal_bytes: u64,
    pub reused_bytes: u64,
}

/// Executes one preflighted v3 push with bounded task fan-out.
///
/// Directory creation is awaited in preorder before reconciliation advances to
/// descendants. Independent leaf work may run concurrently, but at most
/// `max_in_flight` worker futures exist even before scheduler admission. All
/// directory metadata is journaled and replayed child-before-parent only after
/// main work completes.
pub struct RemotePushController {
    executor: Arc<RemotePushExecutor>,
    policy: RemotePushPolicy,
    max_in_flight: NonZeroUsize,
}

impl RemotePushController {
    pub fn new(
        executor: RemotePushExecutor,
        policy: RemotePushPolicy,
        max_in_flight: NonZeroUsize,
    ) -> Self {
        Self {
            executor: Arc::new(executor),
            policy,
            max_in_flight,
        }
    }

    pub async fn execute(&self, plan: RemotePushPlan) -> Result<RemotePushSummary> {
        let RemotePushPlan {
            mut reader,
            operations,
            delete,
        } = plan;
        let delete_candidates = delete.as_ref().map_or(0, DeletePlan::delete_candidates);
        let mut summary = RemotePushSummary {
            planned_operations: operations,
            delete_candidates,
            ..RemotePushSummary::default()
        };
        let mut finalize = FinalizeJournal::new().await?;
        let mut workers = JoinSet::new();

        while let Some(operation) = reader.next().await? {
            let lowered = lower_sync_op(operation, self.policy)?;

            if let Some(finalize_work) = lowered.finalize {
                let metadata = finalize_metadata(finalize_work)?;
                finalize.append(&metadata).await?;
            }

            let Some(main) = lowered.main else {
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

        let mut finalize = finalize.seal().await?;
        while let Some(metadata) = finalize.next().await? {
            self.executor.execute_finalize(metadata).await?;
            summary.finalized_metadata =
                checked_add(summary.finalized_metadata, 1, "finalized metadata")?;
        }

        Ok(summary)
    }
}

fn finalize_metadata(item: WorkItem<RemotePushAction>) -> Result<FinalizeMetadata> {
    match item.into_action() {
        RemotePushAction::ApplyMetadata {
            source,
            unix_mode,
            modified,
        } => Ok(FinalizeMetadata {
            path: source.path,
            kind: source.kind,
            unix_mode,
            modified,
        }),
        _ => Err(RemotePushControllerError::InvalidFinalizeWork),
    }
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
    summary.literal_bytes = checked_add(
        summary.literal_bytes,
        transfer.literal_bytes,
        "literal-byte",
    )?;
    summary.reused_bytes = checked_add(summary.reused_bytes, transfer.reused_bytes, "reused-byte")?;
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

    #[test]
    fn directory_finalize_work_becomes_endpoint_neutral_record() {
        let source = directory("parent/child", 0o700);
        let lowered = lower_sync_op(
            SyncOp::Create {
                source: source.clone(),
            },
            RemotePushPolicy {
                preserve_permissions: true,
                preserve_times: true,
            },
        )
        .unwrap();
        let metadata = finalize_metadata(lowered.finalize.unwrap()).unwrap();

        assert_eq!(metadata.path, source.path);
        assert_eq!(metadata.kind, EntryKind::Directory);
        assert_eq!(metadata.unix_mode, Some(0o700));
        assert_eq!(metadata.modified, Some(Timestamp::UNIX_EPOCH));
    }
}
