//! Deferred `--remove-source-files` for preserved hardlink groups.
//!
//! Group members share one inode, so unlinking the representative immediately
//! changes ctime/nlink for every later scanned member and makes exact identity
//! checks fail. This journal records only successfully published group members,
//! validates every recorded name before any unlink, then replays removals with a
//! disk-backed per-group inode state. Memory and descriptor use do not scale
//! with source tree size.

use super::disk_radix::DiskRadix;
use super::domain::{Entry, EntryIdentity, RelativePath};
use super::native_path;
use crate::rooted_fs::{HardlinkSourceState, RootedFs};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_PATH_BYTES: usize = 1024 * 1024;
const RECORD_FIXED_BYTES: usize = 101;
const STATE_RECORD_BYTES: usize = 32 + HardlinkSourceState::ENCODED_BYTES;

/// Publication and verified-existing parity remain distinct authorities.
pub(crate) enum GroupDestinationProof {
    Published(crate::rooted_fs::PublishedEntryProof),
    Existing {
        path: RelativePath,
        identity: EntryIdentity,
    },
}

impl GroupDestinationProof {
    pub fn revalidate_blocking(&self, rooted: &RootedFs) -> crate::rooted_fs::Result<()> {
        match self {
            Self::Published(proof) => proof.revalidate_blocking(rooted),
            Self::Existing { path, identity } => rooted.verify_retired_destination_blocking(
                path,
                super::domain::EntryKind::File,
                *identity,
            ),
        }
    }
}

#[derive(Default)]
pub(crate) struct HardlinkRemovalJournal {
    disk: Arc<Mutex<Option<File>>>,
}

impl HardlinkRemovalJournal {
    pub async fn append(
        &self,
        group: [u8; 32],
        source: &Entry,
        publication: &crate::rooted_fs::PublishedEntryProof,
    ) -> io::Result<()> {
        if publication.kind != super::domain::EntryKind::File {
            return Err(invalid("hardlink publication must be a regular file"));
        }
        let identity = source.identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "hardlink source removal requires scanned source identity",
            )
        })?;
        let path = source.path.clone();
        let publication = publication.clone();
        self.with_disk(move |disk| {
            disk.append(
                group,
                identity,
                &path,
                &publication.path,
                publication.identity,
                true,
            )
        })
        .await
    }

    pub async fn append_existing(
        &self,
        group: [u8; 32],
        source: &Entry,
        receipt: &crate::endpoint::VerifiedExistingDestinationReceipt,
    ) -> io::Result<()> {
        if receipt.source_path() != &source.path
            || Some(receipt.source_identity()) != source.identity
        {
            return Err(invalid(
                "verified-existing hardlink receipt source mismatch",
            ));
        }
        let path = source.path.clone();
        let destination = receipt.destination_path().clone();
        let identity = receipt.source_identity();
        let destination_identity = receipt.destination_identity();
        self.with_disk(move |disk| {
            disk.append(
                group,
                identity,
                &path,
                &destination,
                destination_identity,
                false,
            )
        })
        .await
    }

    /// Validate every physical destination before replay may unlink ANY source.
    /// The group owner advances its exact identity only after a held-inode link
    /// transaction; individual receipts are deliberately not independent stale tokens.
    async fn validate_destinations<F, Fut>(
        &self,
        groups: &super::hardlink_groups::HardlinkGroups,
        mut validate: F,
    ) -> io::Result<()>
    where
        F: FnMut(GroupDestinationProof) -> Fut,
        Fut: std::future::Future<Output = io::Result<()>>,
    {
        let mut offset = 0;
        loop {
            let next = self
                .with_disk(move |disk| {
                    disk.file.seek(SeekFrom::Start(offset))?;
                    let record = disk.read_record()?;
                    Ok((record, disk.file.stream_position()?))
                })
                .await?;
            let (Some(record), next_offset) = next else {
                return Ok(());
            };
            offset = next_offset;
            let proof = if record.published {
                let group = groups
                    .get(record.group)
                    .await?
                    .ok_or_else(|| invalid("missing published hardlink group"))?;
                GroupDestinationProof::Published(crate::rooted_fs::PublishedEntryProof {
                    path: record.destination_path,
                    kind: super::domain::EntryKind::File,
                    identity: group.publication,
                })
            } else {
                GroupDestinationProof::Existing {
                    path: record.destination_path,
                    identity: record.destination_identity,
                }
            };
            validate(proof).await?;
        }
    }

    pub async fn replay<F, Fut>(
        &self,
        source_root: PathBuf,
        groups: &super::hardlink_groups::HardlinkGroups,
        validate: F,
    ) -> io::Result<()>
    where
        F: FnMut(GroupDestinationProof) -> Fut,
        Fut: std::future::Future<Output = io::Result<()>>,
    {
        self.validate_destinations(groups, validate).await?;
        let disk = Arc::clone(&self.disk);
        tokio::task::spawn_blocking(move || {
            let mut state = disk
                .lock()
                .map_err(|_| io::Error::other("hardlink removal journal lock poisoned"))?;
            let Some(journal) = state.as_mut() else {
                return Ok(());
            };
            let rooted = RootedFs::open_blocking_for_worker(source_root)
                .map_err(|error| io::Error::other(error.to_string()))?;
            journal.sync_data()?;
            let mut journal = DiskJournal { file: journal };
            journal.validate_all(&rooted)?;
            journal.remove_all(&rooted)
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn with_disk<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut DiskJournal) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let disk = Arc::clone(&self.disk);
        tokio::task::spawn_blocking(move || {
            let mut state = disk
                .lock()
                .map_err(|_| io::Error::other("hardlink removal journal lock poisoned"))?;
            if state.is_none() {
                *state = Some(tempfile::tempfile()?);
            }
            let file = state
                .as_mut()
                .ok_or_else(|| io::Error::other("missing hardlink removal journal"))?;
            operation(&mut DiskJournal { file })
        })
        .await
        .map_err(io::Error::other)?
    }
}

struct DiskJournal<'a> {
    file: &'a mut File,
}

#[derive(Debug)]
struct RemovalRecord {
    group: [u8; 32],
    identity: EntryIdentity,
    path: RelativePath,
    destination_path: RelativePath,
    destination_identity: EntryIdentity,
    published: bool,
}

impl DiskJournal<'_> {
    fn append(
        &mut self,
        group: [u8; 32],
        identity: EntryIdentity,
        path: &RelativePath,
        destination: &RelativePath,
        destination_identity: EntryIdentity,
        published: bool,
    ) -> io::Result<()> {
        let destination = native_path::encode(destination.as_path().as_os_str());
        let path = native_path::encode(path.as_path().as_os_str());
        if path.len() > MAX_PATH_BYTES || destination.len() > MAX_PATH_BYTES {
            return Err(invalid("hardlink source-removal path exceeds record limit"));
        }
        let record_len = RECORD_FIXED_BYTES
            .checked_add(path.len())
            .and_then(|len| len.checked_add(destination.len()))
            .ok_or_else(|| invalid("hardlink source-removal record length overflow"))?;
        let record_len = u32::try_from(record_len)
            .map_err(|_| invalid("hardlink source-removal record exceeds u32 length"))?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&record_len.to_le_bytes())?;
        self.file.write_all(&group)?;
        self.file.write_all(identity.as_bytes())?;
        self.file.write_all(destination_identity.as_bytes())?;
        self.file.write_all(&[if published { 1 } else { 2 }])?;
        self.file.write_all(&(path.len() as u32).to_le_bytes())?;
        self.file.write_all(&path)?;
        self.file.write_all(&destination)?;
        Ok(())
    }

    fn validate_all(&mut self, rooted: &RootedFs) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        while let Some(record) = self.read_record()? {
            rooted
                .validate_hardlink_source_member_blocking(&record.path, record.identity)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        Ok(())
    }

    fn remove_all(&mut self, rooted: &RootedFs) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut states = StateIndex::new()?;
        while let Some(record) = self.read_record()? {
            let expected = states.get(record.group)?;
            let updated = rooted
                .remove_hardlink_source_member_blocking(
                    &record.path,
                    record.identity,
                    expected.as_ref(),
                )
                .map_err(|error| io::Error::other(error.to_string()))?;
            states.set(record.group, updated)?;
        }
        Ok(())
    }

    fn read_record(&mut self) -> io::Result<Option<RemovalRecord>> {
        let mut len = [0; 4];
        if self.file.read(&mut len[..1])? == 0 {
            return Ok(None);
        }
        self.file.read_exact(&mut len[1..])?;
        let len = u32::from_le_bytes(len) as usize;
        if !(RECORD_FIXED_BYTES..=RECORD_FIXED_BYTES + 2 * MAX_PATH_BYTES).contains(&len) {
            return Err(invalid("invalid hardlink source-removal record length"));
        }
        let mut fixed = [0; RECORD_FIXED_BYTES];
        self.file.read_exact(&mut fixed)?;
        let mut group = [0; 32];
        group.copy_from_slice(&fixed[..32]);
        let mut identity = [0; 32];
        identity.copy_from_slice(&fixed[32..64]);
        let mut destination_identity = [0; 32];
        destination_identity.copy_from_slice(&fixed[64..96]);
        if !matches!(fixed[96], 1 | 2) {
            return Err(invalid("invalid hardlink destination kind"));
        }
        let source_len = u32::from_le_bytes(
            fixed[97..101]
                .try_into()
                .map_err(|_| invalid("invalid source length"))?,
        ) as usize;
        let mut paths = vec![0; len - RECORD_FIXED_BYTES];
        if source_len > MAX_PATH_BYTES
            || source_len > paths.len()
            || paths.len() - source_len > MAX_PATH_BYTES
        {
            return Err(invalid("invalid hardlink path lengths"));
        }
        self.file.read_exact(&mut paths)?;
        let destination = RelativePath::new(native_path::decode(&paths[source_len..])?)
            .map_err(|_| invalid("invalid hardlink destination path"))?;
        let path = RelativePath::new(native_path::decode(&paths[..source_len])?)
            .map_err(|_| invalid("invalid hardlink source-removal path"))?;
        Ok(Some(RemovalRecord {
            group,
            identity: EntryIdentity::from_bytes(identity),
            path,
            published: fixed[96] == 1,
            destination_path: destination,
            destination_identity: EntryIdentity::from_bytes(destination_identity),
        }))
    }
}

/// One exact current observation per source group, not a replay-length history.
/// Radix lookup visits at most 256 ordered branch bits regardless of how many
/// aliases have already been removed. Partial scratch I/O cannot grant authority
/// to another unlink after a successfully completed native effect.
struct StateIndex {
    file: File,
    index: DiskRadix,
    healthy: bool,
}

impl StateIndex {
    fn new() -> io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
            index: DiskRadix::new()?,
            healthy: true,
        })
    }

    fn read(&mut self, group: [u8; 32]) -> io::Result<Option<(u64, HardlinkSourceState)>> {
        if !self.healthy {
            return Err(invalid("incomplete hardlink source state scratch"));
        }
        self.healthy = false;
        let Some(offset) = self.index.get(group)? else {
            self.healthy = true;
            return Ok(None);
        };
        if offset % STATE_RECORD_BYTES as u64 != 0 {
            return Err(invalid("invalid hardlink source state offset"));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut record = [0; STATE_RECORD_BYTES];
        self.file.read_exact(&mut record)?;
        if record[..32] != group {
            return Err(invalid("hardlink source state group mismatch"));
        }
        let state = HardlinkSourceState::decode(&record[32..])?;
        self.healthy = true;
        Ok(Some((offset, state)))
    }

    fn get(&mut self, group: [u8; 32]) -> io::Result<Option<HardlinkSourceState>> {
        Ok(self.read(group)?.map(|(_, state)| state))
    }

    fn set(&mut self, group: [u8; 32], state: HardlinkSourceState) -> io::Result<()> {
        let existing = self.read(group)?;
        self.healthy = false;
        match existing {
            Some((offset, _)) => {
                self.file.seek(SeekFrom::Start(offset + 32))?;
                self.file.write_all(&state.encode())?;
            }
            None => {
                let offset = self.file.seek(SeekFrom::End(0))?;
                if offset % STATE_RECORD_BYTES as u64 != 0 {
                    return Err(invalid("truncated hardlink source state scratch"));
                }
                self.file.write_all(&group)?;
                self.file.write_all(&state.encode())?;
                self.index.insert(group, offset)?;
            }
        }
        self.healthy = true;
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::domain::{EntryKind, Timestamp};

    fn rel(path: &str) -> RelativePath {
        RelativePath::new(path).unwrap()
    }

    fn publish(rooted: &RootedFs, path: &RelativePath) -> crate::rooted_fs::PublishedEntryProof {
        let mut staged = rooted
            .begin_staged_file_with_expectation_blocking(
                path,
                crate::endpoint::ExpectedDestination::Absent,
            )
            .unwrap();
        staged.file_mut().write_all(b"x").unwrap();
        staged
            .commit()
            .unwrap()
            .finalize_blocking(None)
            .unwrap()
            .into()
    }

    fn state(ino: u64, nlink: u64) -> HardlinkSourceState {
        HardlinkSourceState {
            dev: 1,
            ino,
            size: 4,
            mode: 0o100640,
            mtime: 1,
            mtime_nsec: 2,
            ctime: 3,
            ctime_nsec: 4,
            nlink,
            uid: 5,
            gid: 6,
            flags: 7,
        }
    }

    #[test]
    fn interleaved_source_groups_keep_one_exact_current_state_each() {
        const GROUPS: u64 = 2_048;
        let mut states = StateIndex::new().unwrap();
        for remaining in (0..8).rev() {
            for ino in (0..GROUPS).rev() {
                let mut group = [0; 32];
                group[24..].copy_from_slice(&ino.to_be_bytes());
                let expected = (remaining != 7).then_some(state(ino, remaining + 1));
                assert_eq!(states.get(group).unwrap(), expected);
                states.set(group, state(ino, remaining)).unwrap();
            }
            // Scratch space follows groups, not the number of preceding unlinks.
            assert_eq!(
                states.file.metadata().unwrap().len(),
                GROUPS * STATE_RECORD_BYTES as u64
            );
        }
        for ino in 0..GROUPS {
            let mut group = [0; 32];
            group[24..].copy_from_slice(&ino.to_be_bytes());
            assert_eq!(states.get(group).unwrap(), Some(state(ino, 0)));
        }
        assert_eq!(states.get([255; 32]).unwrap(), None);
    }

    #[test]
    fn corrupt_source_state_cannot_fall_back_to_missing_or_previous_authority() {
        for substitute_group in [false, true] {
            let mut states = StateIndex::new().unwrap();
            states.set([1; 32], state(1, 2)).unwrap();
            states.set([2; 32], state(2, 3)).unwrap();
            if substitute_group {
                states.file.seek(SeekFrom::Start(0)).unwrap();
                states.file.write_all(&[2; 32]).unwrap();
            } else {
                states.file.set_len(STATE_RECORD_BYTES as u64 - 1).unwrap();
            }
            assert!(states.get([1; 32]).is_err());
            // Failed scratch reads poison the whole authority, including a
            // different group and apparently absent groups, before more unlinks.
            assert!(states.get([2; 32]).is_err());
            assert!(states.get([3; 32]).is_err());
            assert!(states.set([1; 32], state(1, 1)).is_err());
        }
    }

    #[test]
    fn failed_source_state_write_poison_cannot_authorize_later_unlinks() {
        let mut states = StateIndex::new().unwrap();
        states.set([1; 32], state(1, 2)).unwrap();
        let read_only = tempfile::NamedTempFile::new().unwrap();
        states.file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = Vec::new();
        states.file.read_to_end(&mut contents).unwrap();
        std::fs::write(read_only.path(), contents).unwrap();
        states.file = File::open(read_only.path()).unwrap();
        assert!(states.set([1; 32], state(1, 1)).is_err());
        assert!(states.get([1; 32]).is_err());
        assert!(states.set([2; 32], state(2, 1)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn large_group_replay_stays_within_low_descriptor_limit() {
        const CHILD: &str = "SY_HARDLINK_REMOVAL_RESOURCE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new("sh")
                .args([
                    "-c",
                    "ulimit -n 32; exec \"$1\" --exact engine::hardlink_removals::tests::large_group_replay_stays_within_low_descriptor_limit --nocapture",
                    "hardlink-removal-resource-test",
                ])
                .arg(std::env::current_exe().unwrap())
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            const LINKS: usize = 2_048;
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("member-0"), b"x").unwrap();
            for index in 1..LINKS {
                std::fs::hard_link(
                    root.path().join("member-0"),
                    root.path().join(format!("member-{index}")),
                )
                .unwrap();
            }
            let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
            let journal = HardlinkRemovalJournal::default();
            let destination = tempfile::tempdir().unwrap();
            let dest = RootedFs::open(destination.path().to_path_buf())
                .await
                .unwrap();
            let groups = super::super::hardlink_groups::HardlinkGroups::default();
            let group = [9; 32];
            let mut proof = publish(&dest, &rel("member-0"));
            groups
                .insert(
                    group,
                    super::super::hardlink_groups::HardlinkRepresentative {
                        path: proof.path.clone(),
                        publication: proof.identity,
                        unix_mode: None,
                        modified: None,
                    },
                )
                .await
                .unwrap();
            for index in 0..LINKS {
                let path = rel(&format!("member-{index}"));
                let (_, identity) = rooted.path_identity_blocking(&path).unwrap().unwrap();
                let mut entry = Entry::file(path, 1, Timestamp::UNIX_EPOCH);
                entry.identity = Some(identity);
                if index != 0 {
                    proof = dest
                        .publish_hardlink_blocking(
                            &rel("member-0"),
                            &entry.path,
                            proof.identity,
                            crate::endpoint::ExpectedDestination::Absent,
                        )
                        .unwrap();
                    groups.advance(group, proof.identity).await.unwrap();
                }
                journal.append(group, &entry, &proof).await.unwrap();
            }
            journal
                .replay(root.path().to_path_buf(), &groups, |proof| {
                    std::future::ready(proof.revalidate_blocking(&dest).map_err(io::Error::other))
                })
                .await
                .unwrap();
            for index in 0..LINKS {
                assert!(!root.path().join(format!("member-{index}")).exists());
            }
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn validates_all_members_before_unlinking() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"same").unwrap();
        std::fs::hard_link(root.path().join("a"), root.path().join("b")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let (_, a_identity) = rooted.path_identity_blocking(&rel("a")).unwrap().unwrap();
        let (_, b_identity) = rooted.path_identity_blocking(&rel("b")).unwrap().unwrap();
        let group = [1; 32];
        let mut a = Entry::file(rel("a"), 4, Timestamp::UNIX_EPOCH);
        a.kind = EntryKind::File;
        a.identity = Some(a_identity);
        let mut b = Entry::file(rel("b"), 4, Timestamp::UNIX_EPOCH);
        b.identity = Some(b_identity);

        let destination = tempfile::tempdir().unwrap();
        let dest = RootedFs::open(destination.path().to_path_buf())
            .await
            .unwrap();
        let first = publish(&dest, &rel("a"));
        let second = dest
            .publish_hardlink_blocking(
                &rel("a"),
                &rel("b"),
                first.identity,
                crate::endpoint::ExpectedDestination::Absent,
            )
            .unwrap();
        let groups = super::super::hardlink_groups::HardlinkGroups::default();
        groups
            .insert(
                group,
                super::super::hardlink_groups::HardlinkRepresentative {
                    path: first.path.clone(),
                    publication: second.identity,
                    unix_mode: None,
                    modified: None,
                },
            )
            .await
            .unwrap();
        let journal = HardlinkRemovalJournal::default();
        journal.append(group, &a, &first).await.unwrap();
        journal.append(group, &b, &second).await.unwrap();
        std::fs::write(root.path().join("b"), b"changed").unwrap();

        assert!(journal
            .replay(root.path().to_path_buf(), &groups, |proof| {
                std::future::ready(proof.revalidate_blocking(&dest).map_err(io::Error::other))
            })
            .await
            .is_err());
        assert!(root.path().join("a").exists());
        assert!(root.path().join("b").exists());
    }
}
