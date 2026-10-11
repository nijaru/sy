//! Sealed selected membership before planned hardlink file work.
//!
//! Each group retains its original source observation and first operation,
//! plus ordered plan locators for all selected members. Whole-group execution
//! owns byte disposition, metadata policy and native source validation.
//! Storage is disk-backed, without tree-sized collections or per-group FDs.
use super::disk_radix::DiskRadix;
use super::domain::{RelativePath, SyncOp};
use super::plan_journal::{
    decode_operation, encode_operation, PlanJournalError, PlanRecord, PlanRecordPosition,
    MAX_RECORD_PAYLOAD,
};

pub(crate) mod catalogue;
pub(crate) use catalogue::HardlinkCatalogue;
use catalogue::{Membership, GROUP_HEADER_BYTES};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum HardlinkPreflightError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Journal(#[from] PlanJournalError),
    #[error("hardlink preflight worker failed: {0}")]
    Worker(String),
    #[error("hardlink preflight requires an observed identity for '{0}'")]
    MissingObservation(RelativePath),
    #[error("selected hardlink source observations disagree at '{first}' and '{second}'")]
    SourceChanged {
        first: RelativePath,
        second: RelativePath,
    },
}

type Result<T> = std::result::Result<T, HardlinkPreflightError>;

fn selected_member(operation: &SyncOp) -> bool {
    let source = operation.source();
    // Selection/comparison-policy skips are not members to coalesce.
    source.is_file() && source.hardlink_group.is_some() && !matches!(operation, SyncOp::Skip { .. })
}

pub(crate) struct HardlinkCommitments {
    disk: Arc<Mutex<Option<MembershipDisk>>>,
    ready: bool,
}

impl Default for HardlinkCommitments {
    fn default() -> Self {
        Self {
            disk: Arc::new(Mutex::new(None)),
            ready: true,
        }
    }
}

impl HardlinkCommitments {
    async fn with_disk<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut MembershipDisk) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let disk = Arc::clone(&self.disk);
        tokio::task::spawn_blocking(move || {
            let mut disk = disk
                .lock()
                .map_err(|_| io::Error::other("hardlink membership lock poisoned"))?;
            if disk.is_none() {
                *disk = Some(MembershipDisk {
                    file: tempfile::tempfile()?,
                    index: DiskRadix::new()?,
                    healthy: true,
                    members: tempfile::tempfile()?,
                    group_count: 0,
                    member_count: 0,
                    group_end: 0,
                    member_end: 0,
                    last_position: None,
                });
            }
            let disk = disk
                .as_mut()
                .ok_or_else(|| invalid("missing hardlink membership scratch"))?;
            let result = operation(disk);
            if result.is_err() {
                disk.healthy = false;
            }
            result
        })
        .await
        .map_err(|error| HardlinkPreflightError::Worker(error.to_string()))?
    }

    /// Record selection without imposing per-member byte promises.
    /// Whole-group execution chooses one coherent byte and metadata policy.
    pub(crate) async fn collect(&mut self, record: PlanRecord) -> Result<()> {
        if !self.ready {
            return Err(invalid("incomplete hardlink catalogue collection").into());
        }
        // Failure or cancellation must never certify a partial selection.
        self.ready = false;
        if selected_member(&record.operation) {
            if let SyncOp::Update { destination, .. }
            | SyncOp::Replace { destination, .. }
            | SyncOp::Metadata { destination, .. }
            | SyncOp::Unchanged { destination, .. } = &record.operation
            {
                destination.identity.ok_or_else(|| {
                    HardlinkPreflightError::MissingObservation(destination.path.clone())
                })?;
            }
            let source = record.operation.source();
            let group = *source
                .hardlink_group
                .ok_or_else(|| invalid("missing hardlink group"))?
                .as_bytes();
            let identity = source
                .identity
                .ok_or_else(|| HardlinkPreflightError::MissingObservation(source.path.clone()))?;
            let baseline = self.with_disk(move |disk| disk.get(group)).await?;
            match baseline {
                Some(baseline) if baseline.operation.source().identity != Some(identity) => {
                    return Err(HardlinkPreflightError::SourceChanged {
                        first: baseline.operation.source().path.clone(),
                        second: source.path.clone(),
                    });
                }
                Some(_) => {}
                None => {
                    let operation = record.operation;
                    self.with_disk(move |disk| disk.insert(group, &operation))
                        .await?;
                }
            }
            self.with_disk(move |disk| disk.append_member(group, record.position))
                .await?;
        }
        self.ready = true;
        Ok(())
    }
}

struct Baseline {
    operation: SyncOp,
    membership: Membership,
    record_digest: [u8; 32],
}

struct MembershipDisk {
    file: File,
    index: DiskRadix,
    healthy: bool,
    members: File,
    group_count: u64,
    member_count: u64,
    group_end: u64,
    member_end: u64,
    last_position: Option<PlanRecordPosition>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl MembershipDisk {
    fn ensure_healthy(&self) -> Result<()> {
        if self.healthy {
            Ok(())
        } else {
            Err(invalid("incomplete hardlink membership scratch").into())
        }
    }

    fn offset(&mut self, group: [u8; 32]) -> Result<Option<u64>> {
        self.ensure_healthy()?;
        Ok(self.index.get(group)?)
    }

    fn get(&mut self, group: [u8; 32]) -> Result<Option<Baseline>> {
        let Some(offset) = self.offset(group)? else {
            return Ok(None);
        };
        let baseline = self.get_at(offset)?.0;
        if baseline
            .operation
            .source()
            .hardlink_group
            .map(|key| *key.as_bytes())
            != Some(group)
        {
            return Err(invalid("catalogue lookup selected another group").into());
        }
        Ok(Some(baseline))
    }

    fn get_at(&mut self, offset: u64) -> Result<(Baseline, u64)> {
        self.ensure_healthy()?;
        if offset
            .checked_add(GROUP_HEADER_BYTES)
            .is_none_or(|end| end > self.group_end)
        {
            return Err(invalid("catalogue group offset outside sealed extent").into());
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut header = [0; GROUP_HEADER_BYTES as usize];
        self.file.read_exact(&mut header)?;
        let len = u32::from_le_bytes(
            header[..4]
                .try_into()
                .map_err(|_| invalid("invalid catalogue length"))?,
        ) as usize;
        if len == 0 || len > MAX_RECORD_PAYLOAD {
            return Err(invalid("invalid catalogue header").into());
        }
        let next = offset
            .checked_add(GROUP_HEADER_BYTES)
            .and_then(|start| start.checked_add(len as u64))
            .filter(|next| *next <= self.group_end)
            .ok_or_else(|| invalid("catalogue group payload outside sealed extent"))?;
        let mut bytes = vec![0; len];
        self.file.read_exact(&mut bytes)?;
        let mut seal = blake3::Hasher::new();
        seal.update(&header);
        seal.update(&bytes);
        let record_digest = *seal.finalize().as_bytes();
        let operation = decode_operation(&bytes)?;
        let group = *operation
            .source()
            .hardlink_group
            .ok_or_else(|| invalid("catalogue group has no identity"))?
            .as_bytes();
        if self.index.get(group)? != Some(offset)
            || !selected_member(&operation)
            || operation.source().identity.is_none()
        {
            return Err(invalid("invalid catalogue group").into());
        }
        Ok((
            Baseline {
                operation,
                record_digest,
                membership: Membership {
                    head: u64::from_le_bytes(
                        header[4..12]
                            .try_into()
                            .map_err(|_| invalid("invalid membership head"))?,
                    ),
                    tail: u64::from_le_bytes(
                        header[12..20]
                            .try_into()
                            .map_err(|_| invalid("invalid membership tail"))?,
                    ),
                    count: u64::from_le_bytes(
                        header[20..28]
                            .try_into()
                            .map_err(|_| invalid("invalid membership count"))?,
                    ),
                },
            },
            next,
        ))
    }

    fn insert(&mut self, group: [u8; 32], operation: &SyncOp) -> Result<()> {
        if self.offset(group)?.is_some() {
            return Err(invalid("duplicate catalogue group").into());
        }
        let bytes = encode_operation(operation)?;
        if bytes.is_empty() || bytes.len() > MAX_RECORD_PAYLOAD {
            return Err(invalid("invalid catalogue length").into());
        }
        let len = u32::try_from(bytes.len()).map_err(|_| invalid("invalid catalogue length"))?;
        self.healthy = false;
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&len.to_le_bytes())?;
        let membership = Membership::default();
        self.file.write_all(&membership.head.to_le_bytes())?;
        self.file.write_all(&membership.tail.to_le_bytes())?;
        self.file.write_all(&membership.count.to_le_bytes())?;
        self.file.write_all(&bytes)?;
        self.index.insert(group, offset)?;
        self.group_count = self
            .group_count
            .checked_add(1)
            .ok_or_else(|| invalid("catalogue group count overflow"))?;
        self.group_end = offset
            .checked_add(GROUP_HEADER_BYTES)
            .and_then(|start| start.checked_add(u64::from(len)))
            .ok_or_else(|| invalid("catalogue group extent overflow"))?;
        self.healthy = true;
        Ok(())
    }
}
