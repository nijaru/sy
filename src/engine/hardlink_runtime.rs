//! Authenticated, bounded replay and exact completion of one selected group.
use super::domain::{EntryIdentity, SyncOp};
use super::hardlink_preflight::catalogue::{HardlinkCatalogue, MemberCursor};
use super::plan_journal::{PlanJournalReader, PlanRecordPosition};
use crate::rooted_fs::PublishedFileProof;

pub(crate) mod intent;

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error(transparent)]
    Catalogue(#[from] super::hardlink_preflight::HardlinkPreflightError),
    #[error(transparent)]
    Journal(#[from] super::plan_journal::PlanJournalError),
    #[error("invalid hardlink group completion: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Accounting(Box<super::controller::ControllerError>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Endpoint executors may replay selection repeatedly, but completion has its
/// own non-rewindable cursor. A count or a representative alone cannot certify
/// publication of every original selected destination.
pub struct HardlinkGroupPlan<'a> {
    catalogue: &'a HardlinkCatalogue,
    journal: &'a PlanJournalReader,
    group: [u8; 32],
    completion: MemberCursor<'a>,
    identity: Option<EntryIdentity>,
    seal: blake3::Hasher,
    completed: u64,
    healthy: bool,
    summary: super::controller::SyncSummary,
}

impl<'a> HardlinkGroupPlan<'a> {
    pub(crate) async fn new(
        catalogue: &'a HardlinkCatalogue,
        journal: &'a PlanJournalReader,
        group: [u8; 32],
    ) -> Result<Self, GroupError> {
        Ok(Self {
            catalogue,
            journal,
            group,
            completion: catalogue.members(group).await?,
            identity: None,
            seal: blake3::Hasher::new(),
            completed: 0,
            healthy: true,
            summary: Default::default(),
        })
    }

    pub(crate) const fn key(&self) -> [u8; 32] {
        self.group
    }

    pub(crate) async fn members(&self) -> Result<GroupCursor<'a>, GroupError> {
        Ok(GroupCursor {
            cursor: self.catalogue.members(self.group).await?,
            journal: self.journal,
        })
    }

    /// Called only with the endpoint's final original-FD proof, after anchor
    /// cleanup and required flags. Exact path/order and one final inode identity
    /// are checked against the sealed catalogue, not against caller counters.
    pub(crate) async fn confirm_member(
        &mut self,
        operation: &SyncOp,
        proof: &PublishedFileProof,
    ) -> Result<(), GroupError> {
        if !self.healthy {
            return Err(GroupError::Invalid("completion is poisoned"));
        }
        self.healthy = false;
        let member = self
            .completion
            .next()
            .await?
            .ok_or(GroupError::Invalid("completion has an extra member"))?;
        if self.journal.read_at(member.position).await? != *operation
            || proof.path != *operation.path()
            || self
                .identity
                .is_some_and(|identity| identity != proof.identity)
        {
            return Err(GroupError::Invalid(
                "completion differs from selected membership",
            ));
        }
        self.identity = Some(proof.identity);
        self.seal.update(&member.position.ordinal.to_le_bytes());
        self.seal.update(&member.position.digest);
        self.seal.update(proof.identity.as_bytes());
        self.completed = self
            .completed
            .checked_add(1)
            .ok_or(GroupError::Invalid("completion count overflow"))?;
        self.healthy = true;
        Ok(())
    }

    pub(crate) fn record_work(
        &mut self,
        operation: &SyncOp,
        result: super::work::WorkResult,
    ) -> Result<(), GroupError> {
        self.summary.main_operations = self
            .summary
            .main_operations
            .checked_add(1)
            .ok_or(GroupError::Invalid("group operation count overflow"))?;
        if matches!(operation, SyncOp::Unchanged { .. }) {
            // Repaired topology is an update, not a skipped quick-equal file.
            self.summary.files_updated = self
                .summary
                .files_updated
                .checked_add(1)
                .ok_or(GroupError::Invalid("repair count overflow"))?;
            self.summary.files_skipped = self.summary.files_updated;
        }
        super::controller::record_work_result(&mut self.summary, result)
            .map_err(|error| GroupError::Accounting(Box::new(error)))?;
        Ok(())
    }

    /// Remote completion acknowledgements must not precede the same exact
    /// membership check that the controller requires on return.
    pub(crate) async fn verify_complete(&mut self) -> Result<(), GroupError> {
        if !self.healthy || self.completion.next().await?.is_some() || self.completed == 0 {
            self.healthy = false;
            return Err(GroupError::Invalid("selected members remain incomplete"));
        }
        Ok(())
    }

    pub(crate) async fn finish(
        mut self,
    ) -> Result<(super::controller::SyncSummary, [u8; 32]), GroupError> {
        self.verify_complete().await?;
        Ok((self.summary, *self.seal.finalize().as_bytes()))
    }
}

pub(crate) struct GroupCursor<'a> {
    cursor: MemberCursor<'a>,
    journal: &'a PlanJournalReader,
}

impl GroupCursor<'_> {
    pub(crate) async fn next(&mut self) -> Result<Option<SyncOp>, GroupError> {
        match self.cursor.next().await? {
            Some(member) => Ok(Some(self.journal.read_at(member.position).await?)),
            None => Ok(None),
        }
    }

    pub(crate) async fn first_position(&mut self) -> Result<PlanRecordPosition, GroupError> {
        self.cursor
            .next()
            .await?
            .map(|member| member.position)
            .ok_or(GroupError::Invalid("selected group is empty"))
    }
}

/// Completed groups stay on disk. Later sequential encounters may skip only
/// after exact selected membership has joined a successful endpoint finish.
#[derive(Default)]
pub(crate) struct CompletedGroups {
    disk: std::sync::Arc<std::sync::Mutex<Option<CompletionDisk>>>,
}

struct CompletionDisk {
    index: super::disk_radix::DiskRadix,
    records: std::fs::File,
    end: u64,
    healthy: bool,
}

impl CompletedGroups {
    async fn with_disk<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut CompletionDisk) -> Result<T, GroupError> + Send + 'static,
    ) -> Result<T, GroupError> {
        let disk = std::sync::Arc::clone(&self.disk);
        tokio::task::spawn_blocking(move || {
            let mut guard = disk
                .lock()
                .map_err(|_| GroupError::Invalid("completion lock poisoned"))?;
            if guard.is_none() {
                *guard = Some(CompletionDisk {
                    index: super::disk_radix::DiskRadix::new()?,
                    records: tempfile::tempfile()?,
                    end: 0,
                    healthy: true,
                });
            }
            let disk = guard
                .as_mut()
                .ok_or(GroupError::Invalid("missing completion index"))?;
            if !disk.healthy || disk.records.metadata()?.len() != disk.end {
                return Err(GroupError::Invalid("completion index is incomplete"));
            }
            disk.healthy = false;
            let result = work(disk)?;
            disk.healthy = true;
            Ok(result)
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
    }

    pub(crate) async fn contains(
        &self,
        group: [u8; 32],
        first: PlanRecordPosition,
    ) -> Result<bool, GroupError> {
        self.with_disk(move |disk| {
            use std::io::{Read, Seek, SeekFrom};
            let Some(offset) = disk.index.get(group)? else {
                return Ok(false);
            };
            if offset.checked_add(136).is_none_or(|end| end > disk.end) {
                return Err(GroupError::Invalid("completion outside recorded extent"));
            }
            disk.records.seek(SeekFrom::Start(offset))?;
            let mut record = [0; 136];
            disk.records.read_exact(&mut record)?;
            if record[..32] != group
                || record[32..40] != first.ordinal.to_le_bytes()
                || record[40..72] != first.digest
                || record[104..] != *blake3::hash(&record[..104]).as_bytes()
            {
                return Err(GroupError::Invalid(
                    "completion does not match original first member",
                ));
            }
            Ok(true)
        })
        .await
    }

    pub(crate) async fn insert(
        &self,
        group: [u8; 32],
        first: PlanRecordPosition,
        seal: [u8; 32],
    ) -> Result<(), GroupError> {
        self.with_disk(move |disk| {
            use std::io::{Seek, SeekFrom, Write};
            if disk.index.get(group)?.is_some() {
                return Err(GroupError::Invalid("group completed twice"));
            }
            let mut record = [0; 136];
            record[..32].copy_from_slice(&group);
            record[32..40].copy_from_slice(&first.ordinal.to_le_bytes());
            record[40..72].copy_from_slice(&first.digest);
            record[72..104].copy_from_slice(&seal);
            let digest = *blake3::hash(&record[..104]).as_bytes();
            record[104..].copy_from_slice(&digest);
            disk.records.seek(SeekFrom::Start(disk.end))?;
            disk.records.write_all(&record)?;
            disk.index.insert(group, disk.end)?;
            disk.end = disk
                .end
                .checked_add(136)
                .ok_or(GroupError::Invalid("completion extent overflow"))?;
            Ok(())
        })
        .await
    }
}
