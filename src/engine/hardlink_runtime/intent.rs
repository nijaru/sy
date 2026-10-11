//! Direction-neutral intended result of the selected catalogue members.
use super::super::domain::{Entry, EntryIdentity, SyncOp, Timestamp};
use crate::engine::planner::ExecutionPolicy;

/// Selection chooses the inode's result; endpoints choose how to populate and
/// publish it. An unchanged member is not a separate byte commitment.
pub(crate) struct GroupIntent {
    pub canonical_source: bool,
    pub retain_metadata: bool,
    pub unix_mode: Option<u32>,
    pub modified: Option<Timestamp>,
    coalesced_identity: Option<EntryIdentity>,
}

impl GroupIntent {
    pub(crate) fn new(first: &SyncOp, policy: ExecutionPolicy) -> Self {
        let source = first.source();
        let representative = destination(first);
        let retain_metadata = matches!(first, SyncOp::Metadata { .. } | SyncOp::Unchanged { .. });
        Self {
            canonical_source: transfers_bytes(first),
            retain_metadata,
            unix_mode: if policy.preserve_permissions
                || representative.is_none()
                || matches!(first, SyncOp::Replace { .. })
            {
                source.unix_mode
            } else {
                representative.and_then(|entry| entry.unix_mode)
            },
            modified: if policy.preserve_times {
                Some(source.modified)
            } else if retain_metadata {
                representative.map(|entry| entry.modified)
            } else {
                None
            },
            coalesced_identity: representative
                .filter(|entry| entry.is_file())
                .and_then(|entry| entry.identity),
        }
    }

    pub(crate) fn include(&mut self, member: &SyncOp) {
        self.canonical_source |= transfers_bytes(member)
            || self.coalesced_identity.is_none()
            || destination(member).and_then(|entry| entry.identity) != self.coalesced_identity;
    }
}

pub(crate) fn destination(operation: &SyncOp) -> Option<&Entry> {
    match operation {
        SyncOp::Create { .. } | SyncOp::Skip { .. } => None,
        SyncOp::Update { destination, .. }
        | SyncOp::Replace { destination, .. }
        | SyncOp::Metadata { destination, .. }
        | SyncOp::Unchanged { destination, .. } => Some(destination),
    }
}

fn transfers_bytes(operation: &SyncOp) -> bool {
    matches!(
        operation,
        SyncOp::Create { .. } | SyncOp::Update { .. } | SyncOp::Replace { .. }
    )
}
