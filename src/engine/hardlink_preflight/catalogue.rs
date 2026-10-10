//! Complete selected membership, kept alongside the byte-commitment baseline.
use super::{
    invalid, ByteCommitment, CommitmentDisk, HardlinkCommitments, HardlinkPreflightError, Result,
};
use crate::engine::plan_journal::{PlanJournalReader, PlanRecordPosition};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

pub(super) const GROUP_HEADER_BYTES: u64 = 61;
const MEMBER_BYTES: u64 = 92;
const NO_MEMBER: u64 = u64::MAX;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy)]
pub(super) struct Membership {
    pub(super) head: u64,
    pub(super) tail: u64,
    pub(super) count: u64,
}

impl Default for Membership {
    fn default() -> Self {
        Self {
            head: NO_MEMBER,
            tail: NO_MEMBER,
            count: 0,
        }
    }
}

/// Immutable membership of a successfully checked plan. The mutex only owns
/// scratch seek cursors, never endpoint data or a live group inode owner.
pub(crate) struct HardlinkCatalogue {
    disk: Arc<Mutex<Option<CommitmentDisk>>>,
    groups: u64,
    members: u64,
    group_end: u64,
    member_end: u64,
    ready: bool,
    digest: [u8; 32],
}

impl HardlinkCommitments {
    pub(crate) async fn seal(self) -> Result<HardlinkCatalogue> {
        if !self.ready {
            return Err(invalid("incomplete hardlink preflight").into());
        }
        let empty = {
            let disk = self
                .disk
                .lock()
                .map_err(|_| invalid("hardlink commitment lock poisoned"))?;
            disk.is_none()
        };
        let (groups, members, group_end, member_end, digest) = if empty {
            (0, 0, 0, 0, *catalogue_hasher(0, 0).finalize().as_bytes())
        } else {
            self.with_disk(|disk| {
                disk.ensure_healthy()?;
                if disk.file.metadata()?.len() != disk.group_end
                    || disk.members.metadata()?.len() != disk.member_end
                {
                    return Err(invalid("hardlink catalogue extent changed").into());
                }
                Ok((
                    disk.group_count,
                    disk.member_count,
                    disk.group_end,
                    disk.member_end,
                    disk.catalogue_digest()?,
                ))
            })
            .await?
        };
        Ok(HardlinkCatalogue {
            disk: self.disk,
            groups,
            members,
            group_end,
            member_end,
            ready: true,
            digest,
        })
    }
}

impl HardlinkCatalogue {
    pub(crate) fn groups(&self) -> u64 {
        self.groups
    }
    pub(crate) fn member_count(&self) -> u64 {
        self.members
    }

    /// One bounded group cursor, sharing the catalogue's existing scratch FDs.
    /// Operations must still be read through the plan's append-time manifest.
    pub(crate) async fn members(&self, group: [u8; 32]) -> Result<MemberCursor<'_>> {
        let baseline = self
            .with_disk(move |disk| {
                disk.get(group)?
                    .ok_or_else(|| invalid("missing catalogue group").into())
            })
            .await?;
        Ok(MemberCursor {
            catalogue: self,
            group,
            next: baseline.membership.head,
            tail: baseline.membership.tail,
            remaining: baseline.membership.count,
            previous: None,
            healthy: true,
        })
    }

    async fn with_disk<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut CommitmentDisk) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let disk = Arc::clone(&self.disk);
        let (group_end, member_end) = (self.group_end, self.member_end);
        tokio::task::spawn_blocking(move || {
            let mut guard = disk
                .lock()
                .map_err(|_| invalid("hardlink catalogue lock poisoned"))?;
            let disk = guard
                .as_mut()
                .ok_or_else(|| invalid("missing hardlink catalogue"))?;
            let result = (|| {
                disk.ensure_healthy()?;
                if disk.file.metadata()?.len() != group_end
                    || disk.members.metadata()?.len() != member_end
                {
                    return Err(invalid("hardlink catalogue extent changed after sealing").into());
                }
                operation(disk)
            })();
            if result.is_err() {
                disk.healthy = false;
            }
            result
        })
        .await
        .map_err(|error| HardlinkPreflightError::Worker(error.to_string()))?
    }

    /// Replay every member against its sealed plan record before execution.
    /// Header/count/forward-link checks detect missing, cyclic and substituted
    /// membership; plan seals detect valid-looking operation substitutions.
    pub(crate) async fn revalidate(&mut self, plan: &PlanJournalReader) -> Result<()> {
        if !self.ready {
            return Err(invalid("incomplete catalogue validation").into());
        }
        self.ready = false;
        self.validate(plan).await?;
        self.ready = true;
        Ok(())
    }

    async fn validate(&self, plan: &PlanJournalReader) -> Result<()> {
        let mut seal = catalogue_hasher(self.groups, self.members);
        let mut group_offset = 0;
        let mut members_seen = 0_u64;
        let mut first_ordinal = None;
        for _ in 0..self.groups {
            let (baseline, next_group) = self
                .with_disk(move |disk| disk.get_at(group_offset))
                .await?;
            let group = *baseline
                .operation
                .source()
                .hardlink_group
                .ok_or_else(|| invalid("catalogue group has no identity"))?
                .as_bytes();
            seal.update(&baseline.record_digest);
            let membership = baseline.membership;
            if membership.count == 0 || membership.count > self.members {
                return Err(invalid("invalid catalogue member count").into());
            }
            let mut cursor = self.members(group).await?;
            for ordinal in 0..membership.count {
                let member = cursor
                    .next()
                    .await?
                    .ok_or_else(|| invalid("catalogue member missing"))?;
                seal.update(&member.digest);
                let position = member.position;
                if ordinal == 0 {
                    if first_ordinal.is_some_and(|last| position.ordinal <= last) {
                        return Err(invalid("catalogue groups are not in first-seen order").into());
                    }
                    first_ordinal = Some(position.ordinal);
                }
                let operation = plan.read_at(position).await?;
                if ordinal == 0 && operation != baseline.operation {
                    return Err(invalid(
                        "catalogue baseline differs from its original first member",
                    )
                    .into());
                }
                if ByteCommitment::from_operation(&operation).is_none()
                    || operation.source().hardlink_group
                        != baseline.operation.source().hardlink_group
                    || operation.source().identity != baseline.operation.source().identity
                {
                    return Err(
                        invalid("catalogue member does not belong to its original group").into(),
                    );
                }
                members_seen = members_seen
                    .checked_add(1)
                    .ok_or_else(|| invalid("catalogue member count overflow"))?;
            }
            group_offset = next_group;
        }
        if group_offset != self.group_end
            || members_seen != self.members
            || seal.finalize().as_bytes() != &self.digest
        {
            return Err(invalid("catalogue totals disagree with sealed membership").into());
        }
        Ok(())
    }
}

pub(crate) struct MemberCursor<'a> {
    catalogue: &'a HardlinkCatalogue,
    group: [u8; 32],
    next: u64,
    tail: u64,
    remaining: u64,
    previous: Option<PlanRecordPosition>,
    healthy: bool,
}

impl MemberCursor<'_> {
    pub(crate) async fn next(&mut self) -> Result<Option<Member>> {
        if !self.healthy {
            return Err(invalid("incomplete member cursor").into());
        }
        if self.remaining == 0 {
            return Ok(None);
        }
        self.healthy = false;
        let (offset, group) = (self.next, self.group);
        let member = self
            .catalogue
            .with_disk(move |disk| disk.read_member(offset, group))
            .await?;
        if self.previous.is_some_and(|last| {
            member.position.ordinal <= last.ordinal || member.position.offset <= last.offset
        }) {
            return Err(invalid("catalogue member order repeats or goes backwards").into());
        }
        if self.remaining == 1 {
            if offset != self.tail || member.next != NO_MEMBER {
                return Err(invalid("catalogue tail disagrees with complete membership").into());
            }
        } else if member.next == NO_MEMBER || member.next <= offset {
            return Err(invalid("catalogue member chain is incomplete or cyclic").into());
        }
        self.previous = Some(member.position);
        self.next = member.next;
        self.remaining -= 1;
        self.healthy = true;
        Ok(Some(member))
    }
}

pub(crate) struct Member {
    pub(crate) position: PlanRecordPosition,
    next: u64,
    digest: [u8; 32],
}

fn catalogue_hasher(groups: u64, members: u64) -> blake3::Hasher {
    let mut seal = blake3::Hasher::new();
    seal.update(&groups.to_le_bytes());
    seal.update(&members.to_le_bytes());
    seal
}

impl CommitmentDisk {
    fn catalogue_digest(&mut self) -> Result<[u8; 32]> {
        let mut seal = catalogue_hasher(self.group_count, self.member_count);
        let mut group_offset = 0;
        let mut members = 0_u64;
        for _ in 0..self.group_count {
            let (baseline, next_group) = self.get_at(group_offset)?;
            seal.update(&baseline.record_digest);
            let group = *baseline
                .operation
                .source()
                .hardlink_group
                .ok_or_else(|| invalid("catalogue group has no identity"))?
                .as_bytes();
            let membership = baseline.membership;
            if membership.count == 0 || membership.count > self.member_count {
                return Err(invalid("invalid catalogue member count").into());
            }
            let mut offset = membership.head;
            for ordinal in 0..membership.count {
                let member = self.read_member(offset, group)?;
                seal.update(&member.digest);
                if ordinal + 1 == membership.count {
                    if offset != membership.tail || member.next != NO_MEMBER {
                        return Err(
                            invalid("catalogue tail disagrees with complete membership").into()
                        );
                    }
                } else if member.next == NO_MEMBER || member.next <= offset {
                    return Err(invalid("catalogue member chain is incomplete or cyclic").into());
                }
                offset = member.next;
                members = members
                    .checked_add(1)
                    .ok_or_else(|| invalid("catalogue member count overflow"))?;
            }
            group_offset = next_group;
        }
        if group_offset != self.group_end || members != self.member_count {
            return Err(invalid("catalogue totals disagree with complete membership").into());
        }
        Ok(*seal.finalize().as_bytes())
    }

    pub(super) fn append_member(
        &mut self,
        group: [u8; 32],
        position: PlanRecordPosition,
    ) -> Result<()> {
        let baseline = self
            .get(group)?
            .ok_or_else(|| invalid("missing catalogue group"))?;
        if self
            .last_position
            .is_some_and(|last| position.ordinal <= last.ordinal || position.offset <= last.offset)
            || position.payload_bytes == 0
            || position.payload_bytes as usize > super::MAX_RECORD_PAYLOAD
        {
            return Err(invalid("invalid or repeated catalogue plan position").into());
        }
        let offset = self.member_end;
        let next_end = offset
            .checked_add(MEMBER_BYTES)
            .ok_or_else(|| invalid("catalogue extent overflow"))?;
        let count = baseline
            .membership
            .count
            .checked_add(1)
            .ok_or_else(|| invalid("group member count overflow"))?;
        let total = self
            .member_count
            .checked_add(1)
            .ok_or_else(|| invalid("catalogue member count overflow"))?;
        let group_offset = self
            .offset(group)?
            .ok_or_else(|| invalid("missing catalogue group"))?;
        self.healthy = false;
        self.members.seek(SeekFrom::Start(offset))?;
        self.members.write_all(&group)?;
        self.members.write_all(&position.offset.to_le_bytes())?;
        self.members.write_all(&position.ordinal.to_le_bytes())?;
        self.members
            .write_all(&position.payload_bytes.to_le_bytes())?;
        self.members.write_all(&position.digest)?;
        self.members.write_all(&NO_MEMBER.to_le_bytes())?;
        if baseline.membership.count != 0 {
            self.members
                .seek(SeekFrom::Start(baseline.membership.tail + MEMBER_BYTES - 8))?;
            self.members.write_all(&offset.to_le_bytes())?;
        }
        let head = if baseline.membership.count == 0 {
            offset
        } else {
            baseline.membership.head
        };
        self.file.seek(SeekFrom::Start(group_offset + 37))?;
        self.file.write_all(&head.to_le_bytes())?;
        self.file.write_all(&offset.to_le_bytes())?;
        self.file.write_all(&count.to_le_bytes())?;
        self.member_end = next_end;
        self.member_count = total;
        self.last_position = Some(position);
        self.healthy = true;
        Ok(())
    }

    fn read_member(&mut self, offset: u64, group: [u8; 32]) -> Result<Member> {
        if !offset.is_multiple_of(MEMBER_BYTES)
            || offset
                .checked_add(MEMBER_BYTES)
                .is_none_or(|end| end > self.member_end)
        {
            return Err(invalid("catalogue member offset outside sealed records").into());
        }
        let mut bytes = [0; MEMBER_BYTES as usize];
        self.members.seek(SeekFrom::Start(offset))?;
        self.members.read_exact(&mut bytes)?;
        if bytes[..32] != group {
            return Err(invalid("catalogue member group substituted").into());
        }
        let u64_at = |start| -> Result<u64> {
            Ok(u64::from_le_bytes(
                bytes[start..start + 8]
                    .try_into()
                    .map_err(|_| invalid("invalid catalogue integer"))?,
            ))
        };
        let position = PlanRecordPosition {
            offset: u64_at(32)?,
            ordinal: u64_at(40)?,
            payload_bytes: u32::from_le_bytes(
                bytes[48..52]
                    .try_into()
                    .map_err(|_| invalid("invalid catalogue payload length"))?,
            ),
            digest: bytes[52..84]
                .try_into()
                .map_err(|_| invalid("invalid catalogue payload seal"))?,
        };
        Ok(Member {
            position,
            next: u64_at(84)?,
            digest: *blake3::hash(&bytes).as_bytes(),
        })
    }
}
