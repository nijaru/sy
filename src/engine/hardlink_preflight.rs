//! Byte commitments and sealed membership before planned hardlink file work.
//!
//! Transfer work promises source bytes; quick-equal metadata/unchanged work
//! retains old destination bytes. Neither may silently win over the other.
//! The first commitment and its demand-driven digest are indexed per group;
//! complete membership retains original plan locators alongside that baseline.
//! All storage is disk-backed, without a tree-sized resident collection or
//! per-group file descriptors.
use super::disk_radix::DiskRadix;
use super::domain::{Entry, RelativePath, SyncOp};
use super::plan_journal::{
    decode_operation, encode_operation, PlanJournalError, PlanRecord, PlanRecordPosition,
    MAX_RECORD_PAYLOAD,
};

mod catalogue;
use super::scheduler::{ResourceRequest, Scheduler, SchedulerError};
pub(crate) use catalogue::HardlinkCatalogue;
use catalogue::{Membership, GROUP_HEADER_BYTES};
use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum HardlinkPreflightError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Journal(#[from] PlanJournalError),
    #[error(transparent)]
    Resources(#[from] SchedulerError),
    #[error("hardlink preflight worker failed: {0}")]
    Worker(String),
    #[error("hardlink preflight requires an observed identity for '{0}'")]
    MissingObservation(RelativePath),
    #[error("selected hardlink source observations disagree at '{first}' and '{second}'")]
    SourceChanged {
        first: RelativePath,
        second: RelativePath,
    },
    #[error(
        "incompatible byte commitments for selected hardlink group at '{first}' and '{second}'"
    )]
    IncompatibleBytes {
        first: RelativePath,
        second: RelativePath,
    },
}

type Result<T> = std::result::Result<T, HardlinkPreflightError>;

/// Which endpoint owns bytes that the semantic operation promises to retain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteCommitment {
    Source(Entry),
    Destination(Entry),
}

impl ByteCommitment {
    pub fn entry(&self) -> &Entry {
        match self {
            Self::Source(entry) | Self::Destination(entry) => entry,
        }
    }

    fn from_operation(operation: &SyncOp) -> Option<Self> {
        let source = operation.source();
        if !source.is_file() || source.hardlink_group.is_none() {
            return None;
        }
        match operation {
            SyncOp::Create { source, .. }
            | SyncOp::Update { source, .. }
            | SyncOp::Replace { source, .. } => Some(Self::Source(source.clone())),
            SyncOp::Metadata { destination, .. } | SyncOp::Unchanged { destination, .. } => {
                Some(Self::Destination(destination.clone()))
            }
            // Selection/comparison-policy skips are not members to coalesce.
            SyncOp::Skip { .. } => None,
        }
    }

    fn same_observation(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Source(_), Self::Source(_)) | (Self::Destination(_), Self::Destination(_))
        ) && self.entry().identity == other.entry().identity
            && self.entry().size == other.entry().size
    }
}

pub(crate) struct HardlinkCommitments {
    disk: Arc<Mutex<Option<CommitmentDisk>>>,
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
        operation: impl FnOnce(&mut CommitmentDisk) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let disk = Arc::clone(&self.disk);
        tokio::task::spawn_blocking(move || {
            let mut disk = disk
                .lock()
                .map_err(|_| io::Error::other("hardlink commitment lock poisoned"))?;
            if disk.is_none() {
                *disk = Some(CommitmentDisk {
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
                .ok_or_else(|| invalid("missing hardlink commitment scratch"))?;
            let result = operation(disk);
            if result.is_err() {
                disk.healthy = false;
            }
            result
        })
        .await
        .map_err(|error| HardlinkPreflightError::Worker(error.to_string()))?
    }

    pub(crate) async fn check<F, Fut, E>(
        &mut self,
        record: PlanRecord,
        scheduler: &Scheduler,
        hash: &mut F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(ByteCommitment) -> Fut,
        Fut: Future<Output = std::result::Result<[u8; 32], E>>,
        E: From<HardlinkPreflightError>,
    {
        if !self.ready {
            return Err(
                HardlinkPreflightError::from(invalid("incomplete hardlink preflight")).into(),
            );
        }
        self.ready = false;
        let group = ByteCommitment::from_operation(&record.operation)
            .and_then(|_| record.operation.source().hardlink_group)
            .map(|group| *group.as_bytes());
        self.check_bytes(record.operation, scheduler, hash).await?;
        if let Some(group) = group {
            self.with_disk(move |disk| disk.append_member(group, record.position))
                .await?;
        }
        self.ready = true;
        Ok(())
    }

    async fn check_bytes<F, Fut, E>(
        &self,
        operation: SyncOp,
        scheduler: &Scheduler,
        hash: &mut F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(ByteCommitment) -> Fut,
        Fut: Future<Output = std::result::Result<[u8; 32], E>>,
        E: From<HardlinkPreflightError>,
    {
        let Some(current) = ByteCommitment::from_operation(&operation) else {
            return Ok(());
        };
        let source = operation.source();
        let group = *source
            .hardlink_group
            .ok_or_else(|| invalid("missing hardlink group"))
            .map_err(HardlinkPreflightError::from)?
            .as_bytes();
        let source_identity = source
            .identity
            .ok_or_else(|| HardlinkPreflightError::MissingObservation(source.path.clone()))?;
        current.entry().identity.ok_or_else(|| {
            HardlinkPreflightError::MissingObservation(current.entry().path.clone())
        })?;
        let baseline = self.with_disk(move |disk| disk.get(group)).await?;
        let Some(baseline) = baseline else {
            self.with_disk(move |disk| disk.insert(group, &operation))
                .await?;
            return Ok(());
        };
        let first_source = baseline.operation.source();
        if first_source.identity != Some(source_identity) {
            return Err(HardlinkPreflightError::SourceChanged {
                first: first_source.path.clone(),
                second: source.path.clone(),
            }
            .into());
        }
        let first = ByteCommitment::from_operation(&baseline.operation)
            .ok_or_else(|| invalid("invalid hardlink commitment operation"))
            .map_err(HardlinkPreflightError::from)?;
        if first.same_observation(&current) {
            return Ok(());
        }
        let conflict = || HardlinkPreflightError::IncompatibleBytes {
            first: baseline.operation.path().clone(),
            second: operation.path().clone(),
        };
        if first.entry().size != current.entry().size {
            return Err(conflict().into());
        }
        if baseline.source_compatible && matches!(current, ByteCommitment::Source(_)) {
            return Ok(());
        }
        let first_digest = match baseline.digest {
            Some(digest) => digest,
            None => {
                let digest = hash_admitted(first, scheduler, hash).await?;
                self.with_disk(move |disk| disk.set_digest(group, digest))
                    .await?;
                digest
            }
        };
        let source_commitment = matches!(current, ByteCommitment::Source(_));
        if hash_admitted(current, scheduler, hash).await? != first_digest {
            return Err(conflict().into());
        }
        if source_commitment {
            // Every source member was required to carry the same observation.
            // Once its bytes match the retained baseline, later source aliases
            // need no repeated full-file read. This remains preflight evidence,
            // not publication/source-removal authority or a filesystem snapshot.
            self.with_disk(move |disk| disk.mark_source_compatible(group))
                .await?;
        }
        Ok(())
    }
}

async fn hash_admitted<F, Fut, E>(
    commitment: ByteCommitment,
    scheduler: &Scheduler,
    hash: &mut F,
) -> std::result::Result<[u8; 32], E>
where
    F: FnMut(ByteCommitment) -> Fut,
    Fut: Future<Output = std::result::Result<[u8; 32], E>>,
    E: From<HardlinkPreflightError>,
{
    // One joined bounded fingerprint at a time. Even a metadata-only member
    // needs actual file/byte/CPU admission when byte commitments differ.
    let _permit = scheduler
        .acquire(ResourceRequest {
            active_files: 1,
            buffered_bytes: 1024 * 1024,
            metadata_ops: 1,
            cpu_tasks: 1,
            network_writes: 1,
        })
        .await
        .map_err(HardlinkPreflightError::from)?;
    hash(commitment).await
}

struct Baseline {
    operation: SyncOp,
    digest: Option<[u8; 32]>,
    source_compatible: bool,
    membership: Membership,
    record_digest: [u8; 32],
}

struct CommitmentDisk {
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

impl CommitmentDisk {
    fn ensure_healthy(&self) -> Result<()> {
        if self.healthy {
            Ok(())
        } else {
            Err(invalid("incomplete hardlink commitment scratch").into())
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
            return Err(invalid("commitment lookup selected another group").into());
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
                .map_err(|_| invalid("invalid commitment length"))?,
        ) as usize;
        if len == 0 || len > MAX_RECORD_PAYLOAD || header[4] & !3 != 0 || header[4] == 2 {
            return Err(invalid("invalid commitment header").into());
        }
        let digest = if header[4] & 1 != 0 {
            Some(
                header[5..37]
                    .try_into()
                    .map_err(|_| invalid("invalid commitment digest"))?,
            )
        } else {
            None
        };
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
            || ByteCommitment::from_operation(&operation).is_none()
        {
            return Err(invalid("invalid commitment group").into());
        }
        Ok((
            Baseline {
                operation,
                digest,
                source_compatible: header[4] & 2 != 0,
                record_digest,
                membership: Membership {
                    head: u64::from_le_bytes(
                        header[37..45]
                            .try_into()
                            .map_err(|_| invalid("invalid membership head"))?,
                    ),
                    tail: u64::from_le_bytes(
                        header[45..53]
                            .try_into()
                            .map_err(|_| invalid("invalid membership tail"))?,
                    ),
                    count: u64::from_le_bytes(
                        header[53..61]
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
            return Err(invalid("duplicate commitment group").into());
        }
        let bytes = encode_operation(operation)?;
        if bytes.is_empty() || bytes.len() > MAX_RECORD_PAYLOAD {
            return Err(invalid("invalid commitment length").into());
        }
        let len = u32::try_from(bytes.len()).map_err(|_| invalid("invalid commitment length"))?;
        self.healthy = false;
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(&[0; 33])?;
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

    fn mark_source_compatible(&mut self, group: [u8; 32]) -> Result<()> {
        let offset = self
            .offset(group)?
            .ok_or_else(|| invalid("missing commitment group"))?;
        self.file.seek(SeekFrom::Start(offset + 4))?;
        let mut flags = [0];
        self.file.read_exact(&mut flags)?;
        if flags[0] != 1 && flags[0] != 3 {
            return Err(invalid("source comparison requires a baseline digest").into());
        }
        self.healthy = false;
        self.file.seek(SeekFrom::Start(offset + 4))?;
        self.file.write_all(&[3])?;
        self.healthy = true;
        Ok(())
    }

    fn set_digest(&mut self, group: [u8; 32], digest: [u8; 32]) -> Result<()> {
        let offset = self
            .offset(group)?
            .ok_or_else(|| invalid("missing commitment group"))?;
        self.healthy = false;
        self.file.seek(SeekFrom::Start(offset + 5))?;
        self.file.write_all(&digest)?;
        self.file.seek(SeekFrom::Start(offset + 4))?;
        self.file.write_all(&[1])?;
        self.healthy = true;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::domain::{ContentComparison, EntryIdentity, SkipReason, Timestamp};

    fn source(name: &str, group: [u8; 32]) -> Entry {
        let mut entry = Entry::file(RelativePath::new(name).unwrap(), 6, Timestamp::UNIX_EPOCH);
        entry.identity = Some(EntryIdentity::from_bytes(group));
        entry.hardlink_group = Some(EntryIdentity::from_bytes(group));
        entry
    }

    fn create(name: &str, group: [u8; 32]) -> SyncOp {
        let source = source(name, group);
        SyncOp::Create {
            destination_path: source.path.clone(),
            source,
        }
    }

    fn retained(name: &str, identity: [u8; 32]) -> SyncOp {
        let source = source(name, [1; 32]);
        let mut destination = source.clone();
        destination.identity = Some(EntryIdentity::from_bytes(identity));
        destination.hardlink_group = None;
        SyncOp::Unchanged {
            source,
            destination,
            comparison: ContentComparison::Unverified,
        }
    }

    #[tokio::test]
    async fn source_groups_and_identical_retained_observations_need_no_payload_reads() {
        let scheduler = Scheduler::new(Default::default()).unwrap();
        for operations in [
            vec![create("a", [1; 32]), create("b", [1; 32])],
            vec![retained("a", [2; 32]), retained("b", [2; 32])],
            vec![
                create("a", [1; 32]),
                SyncOp::Skip {
                    source: source("b", [1; 32]),
                    reason: SkipReason::DestinationNewer,
                },
            ],
        ] {
            let commitments = HardlinkCommitments::default();
            let mut hash = |_| async { panic!("this group must not read any payload") };
            for operation in operations {
                commitments
                    .check_bytes::<_, _, HardlinkPreflightError>(operation, &scheduler, &mut hash)
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn distinct_retained_bases_cache_first_digest_and_reject_changed_source_observations() {
        let scheduler = Scheduler::new(Default::default()).unwrap();
        let commitments = HardlinkCommitments::default();
        let mut first_reads = 0;
        let mut hash = |commitment: ByteCommitment| {
            if commitment.entry().identity == Some(EntryIdentity::from_bytes([2; 32])) {
                first_reads += 1;
            }
            std::future::ready(Ok::<_, HardlinkPreflightError>([7; 32]))
        };
        for operation in [
            retained("a", [2; 32]),
            retained("b", [3; 32]),
            retained("c", [4; 32]),
        ] {
            commitments
                .check_bytes(operation, &scheduler, &mut hash)
                .await
                .unwrap();
        }
        assert_eq!(
            first_reads, 1,
            "the baseline payload must not be reread for every member"
        );
        let mut changed = retained("d", [2; 32]);
        if let SyncOp::Unchanged { source, .. } = &mut changed {
            source.identity = Some(EntryIdentity::from_bytes([8; 32]));
        }
        let result = commitments
            .check_bytes(changed, &scheduler, &mut |_| async {
                panic!("a group key must not replace the original source observation")
            })
            .await;
        assert!(matches!(
            result,
            Err(HardlinkPreflightError::SourceChanged { .. })
        ));
    }

    #[tokio::test]
    async fn retained_first_groups_compare_source_bytes_once_without_adopting_new_observations() {
        let scheduler = Scheduler::new(Default::default()).unwrap();
        let commitments = HardlinkCommitments::default();
        let mut source_reads = 0;
        let mut hash = |commitment: ByteCommitment| {
            if matches!(commitment, ByteCommitment::Source(_)) {
                source_reads += 1;
            }
            std::future::ready(Ok::<_, HardlinkPreflightError>([7; 32]))
        };
        commitments
            .check_bytes(retained("a", [2; 32]), &scheduler, &mut hash)
            .await
            .unwrap();
        for member in 0..32 {
            commitments
                .check_bytes(
                    create(&format!("source-{member}"), [1; 32]),
                    &scheduler,
                    &mut hash,
                )
                .await
                .unwrap();
        }
        assert_eq!(
            source_reads, 1,
            "selected source aliases must not multiply full-file fingerprint work"
        );
        let mut changed = create("changed", [1; 32]);
        if let SyncOp::Create { source, .. } = &mut changed {
            source.identity = Some(EntryIdentity::from_bytes([8; 32]));
        }
        let result = commitments
            .check_bytes(changed, &scheduler, &mut |_| async {
                panic!("cached parity must not adopt a changed source observation")
            })
            .await;
        assert!(matches!(
            result,
            Err(HardlinkPreflightError::SourceChanged { .. })
        ));
    }

    #[tokio::test]
    async fn corrupt_record_lengths_and_digest_tags_fail_before_allocation() {
        for (offset, bytes) in [(0, u32::MAX.to_le_bytes().to_vec()), (4, vec![2])] {
            let commitments = HardlinkCommitments::default();
            commitments
                .with_disk(|disk| disk.insert([1; 32], &create("a", [1; 32])))
                .await
                .unwrap();
            commitments
                .with_disk(move |disk| {
                    disk.file.seek(SeekFrom::Start(offset))?;
                    disk.file.write_all(&bytes)?;
                    Ok(())
                })
                .await
                .unwrap();
            assert!(
                matches!(commitments.with_disk(|disk| disk.get([1; 32])).await,
                Err(HardlinkPreflightError::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn commitments_remain_exact_with_bounded_memory_and_descriptors() {
        const CHILD: &str = "SY_HARDLINK_COMMITMENT_RESOURCE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new("sh")
                .args(["-c", "ulimit -n 32; exec \"$1\" --exact engine::hardlink_preflight::tests::commitments_remain_exact_with_bounded_memory_and_descriptors --nocapture", "hardlink-commitment-test"])
                .arg(std::env::current_exe().unwrap()).env(CHILD, "1").output().unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        fn rss_kib() -> usize {
            let output = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let scheduler = Scheduler::new(Default::default()).unwrap();
            let mut commitments = HardlinkCommitments::default();
            let mut journal = super::super::plan_journal::PlanJournal::new()
                .await
                .unwrap();
            let mut hash = |_| async { panic!("new source groups require no fingerprint") };
            let operation = create("a", [0; 32]);
            let position = journal.append(&operation).await.unwrap();
            commitments
                .check::<_, _, HardlinkPreflightError>(
                    PlanRecord {
                        position,
                        operation,
                    },
                    &scheduler,
                    &mut hash,
                )
                .await
                .unwrap();
            let before = rss_kib();
            let path = "p".repeat(4096);
            // 64 MiB of native path records, without a tree-sized resident map.
            for index in 1..=8192_u64 {
                let group = *blake3::hash(&index.to_le_bytes()).as_bytes();
                let operation = create(&path, group);
                let position = journal.append(&operation).await.unwrap();
                commitments
                    .check::<_, _, HardlinkPreflightError>(
                        PlanRecord {
                            position,
                            operation,
                        },
                        &scheduler,
                        &mut hash,
                    )
                    .await
                    .unwrap();
            }
            let after = rss_kib();
            assert!(
                after < before + 8 * 1024,
                "RSS grew from {before} to {after} KiB"
            );
            for index in [1_u64, 4096, 8192] {
                let group = *blake3::hash(&index.to_le_bytes()).as_bytes();
                let operation = create("later-member", group);
                let position = journal.append(&operation).await.unwrap();
                commitments
                    .check::<_, _, HardlinkPreflightError>(
                        PlanRecord {
                            position,
                            operation,
                        },
                        &scheduler,
                        &mut hash,
                    )
                    .await
                    .unwrap();
            }
            let mut catalogue = commitments.seal().await.unwrap();
            let reader = journal.seal().await.unwrap();
            assert_eq!((catalogue.groups(), catalogue.member_count()), (8193, 8196));
            catalogue.revalidate(&reader).await.unwrap();
            assert!(rss_kib() < before + 8 * 1024);
        });
    }
}
