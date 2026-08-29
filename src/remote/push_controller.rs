use crate::engine::domain::RelativePath;
use crate::engine::finalize_journal::{
    FinalizeJournal, FinalizeJournalError, FinalizeMetadata,
};
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
/// Destination-only entries are deliberately ignored here: deletion has its
/// own exact preflight journal and threshold contract. This plan represents the
/// source-backed work that is safe to execute after any required delete
/// preflight has succeeded.
pub struct RemotePushPlan {
    reader: PlanJournalReader,
    operations: u64,
}

impl RemotePushPlan {
    pub const fn operations(&self) -> u64 {
        self.operations
    }
}

/// Complete the source/destination merge without mutating either endpoint and
/// spool semantic planner output to disk in forward execution order.
pub async fn preflight_remote_push(
    source: EntryStream,
    destination: EntryStream,
    policy: ComparisonPolicy,
) -> Result<RemotePushPlan> {
    let mut reconciler = OrderedReconciler::new(source, destination);
    let mut journal = PlanJournal::new().await?;
    let mut operations = 0_u64;

    while let Some(item) = reconciler.next().await? {
        let decision = match item {
            ReconcileItem::SourceOnly(source) => plan_entry(source, None, policy),
            ReconcileItem::Matched {
                source,
                destination,
            } => plan_entry(source, Some(destination), policy),
            ReconcileItem::DestinationOnly(_) => continue,
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

    Ok(RemotePushPlan {
        reader: journal.seal().await?,
        operations,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemotePushSummary {
    pub planned_operations: u64,
    pub main_operations: u64,
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

    pub async fn execute(&self, mut plan: RemotePushPlan) -> Result<RemotePushSummary> {
        let mut summary = RemotePushSummary {
            planned_operations: plan.operations,
            ..RemotePushSummary::default()
        };
        let mut finalize = FinalizeJournal::new().await?;
        let mut workers = JoinSet::new();

        while let Some(operation) = plan.reader.next().await? {
            let lowered = lower_sync_op(operation, self.policy)?;

            if let Some(finalize_work) = lowered.finalize {
                let metadata = finalize_metadata(finalize_work)?;
                finalize.append(&metadata).await?;
            }

            let Some(main) = lowered.main else {
                continue;
            };
            summary.main_operations =
                checked_add(summary.main_operations, 1, "main operation")?;

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

fn record_transfer(summary: &mut RemotePushSummary, transfer: Option<TransferSummary>) -> Result<()> {
    let Some(transfer) = transfer else {
        return Ok(());
    };
    summary.files_transferred = checked_add(summary.files_transferred, 1, "file transfer")?;
    summary.literal_bytes = checked_add(
        summary.literal_bytes,
        transfer.literal_bytes,
        "literal-byte",
    )?;
    summary.reused_bytes = checked_add(
        summary.reused_bytes,
        transfer.reused_bytes,
        "reused-byte",
    )?;
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
        let mut entry = Entry::file(
            path(value),
            size,
            Timestamp::new(modified, 0).unwrap(),
        );
        entry.unix_mode = Some(0o644);
        entry
    }

    fn directory(value: &str, mode: u32) -> Entry {
        let mut entry = Entry::directory(path(value), Timestamp::UNIX_EPOCH);
        entry.unix_mode = Some(mode);
        entry
    }

    fn entries(values: Vec<Entry>) -> EntryStream {
        Box::pin(stream::iter(
            values.into_iter().map(Ok::<Entry, BoxError>),
        ))
    }

    #[tokio::test]
    async fn preflight_spools_source_backed_operations_in_order() {
        let source = entries(vec![file("a", 1, 1), file("c", 3, 1)]);
        let destination = entries(vec![file("b", 2, 1), file("c", 3, 1)]);
        let mut plan = preflight_remote_push(source, destination, ComparisonPolicy::default())
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
            preflight_remote_push(source, destination, policy).await,
            Err(RemotePushControllerError::UnsupportedContentComparison(value))
                if value == path("a")
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
