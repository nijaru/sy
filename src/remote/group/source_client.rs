//! Client ownership of one source lease: its inbox outlives byte reception and
//! remains registered until all native destination proofs have completed.
use super::declarations::add_to_seal;
use super::{GroupError, Result};
use crate::endpoint::io::{Preservation, StagedWriter};
use crate::engine::hardlink_runtime::HardlinkGroupPlan;
use crate::engine::work::TransferSummary;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, WireAcl, WireGroupMember, WireGroupOpen, WireGroupResult,
    WireGroupRole, WireGroupSeal, WireGroupStep, WireXattrResult,
};
use crate::remote::router::{RouterSender, StreamInbox};

pub(crate) struct SourceLease {
    sender: RouterSender,
    inbox: StreamInbox,
    open: WireGroupOpen,
    count: u64,
    digest: blake3::Hasher,
    seal: Option<WireGroupSeal>,
    prepared: bool,
    received: bool,
    validated: u64,
    healthy: bool,
}

impl SourceLease {
    pub(crate) async fn open(sender: RouterSender, open: WireGroupOpen) -> Result<Self> {
        if open.role != WireGroupRole::Source {
            return Err(GroupError::Phase("source lease role"));
        }
        let inbox = sender.open_stream()?;
        sender
            .send(Frame::new(
                FrameKind::GroupOpen,
                FrameFlags::empty(),
                inbox.stream_id(),
                open.encode(),
            )?)
            .await?;
        Ok(Self {
            sender,
            inbox,
            open,
            count: 0,
            digest: blake3::Hasher::new(),
            seal: None,
            prepared: false,
            received: false,
            validated: 0,
            healthy: true,
        })
    }

    pub(crate) async fn declare(&mut self, member: WireGroupMember) -> Result<()> {
        if !self.healthy || self.seal.is_some() || member.ordinal != self.count {
            self.healthy = false;
            return Err(GroupError::Phase("source declaration ordinal or phase"));
        }
        self.healthy = false;
        add_to_seal(&mut self.digest, &member)?;
        self.send(FrameKind::GroupMember, member.encode()).await?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or(GroupError::Phase("source declaration count overflow"))?;
        self.healthy = true;
        Ok(())
    }

    pub(crate) async fn prepare(&mut self) -> Result<(Preservation, Option<u32>)> {
        if !self.healthy || self.count == 0 || self.prepared {
            return Err(GroupError::Phase("source prepare phase"));
        }
        self.healthy = false;
        let seal = WireGroupSeal {
            count: self.count,
            digest: *self.digest.finalize().as_bytes(),
        };
        self.send(FrameKind::GroupMembersEnd, seal.encode()).await?;
        if self.reply().await? != WireGroupResult::Declarations(seal) {
            return Err(GroupError::Phase("source declaration acknowledgement"));
        }
        self.seal = Some(seal);
        self.send(FrameKind::GroupStep, WireGroupStep::Prepare.encode())
            .await?;
        let WireGroupResult::SourceReady { flags } = self.reply().await? else {
            return Err(GroupError::Phase("source readiness"));
        };
        if flags.is_some() != self.open.source_flags {
            return Err(GroupError::Phase("source flags acknowledgement"));
        }
        let xattrs = if self.open.xattrs {
            let routed = self
                .inbox
                .recv()
                .await?
                .ok_or(GroupError::Phase("source xattrs missing"))?;
            let frame = routed.frame();
            if frame.kind() != FrameKind::FileXattrs || !frame.flags().is_empty() {
                return Err(GroupError::Phase("source xattrs frame"));
            }
            Some(
                WireXattrResult::decode(frame.payload())?
                    .entries()
                    .iter()
                    .map(|entry| {
                        (
                            crate::remote::xattr::os_string_from_name(entry.name()),
                            entry.value().to_vec(),
                        )
                    })
                    .collect(),
            )
        } else {
            None
        };
        let acl = if self.open.acls {
            let routed = self
                .inbox
                .recv()
                .await?
                .ok_or(GroupError::Phase("source ACL missing"))?;
            let frame = routed.frame();
            if frame.kind() != FrameKind::FileAcls || !frame.flags().is_empty() {
                return Err(GroupError::Phase("source ACL frame"));
            }
            Some(WireAcl::decode(frame.payload())?.text().to_owned())
        } else {
            None
        };
        self.prepared = true;
        self.received = !self.open.source_bytes;
        self.healthy = true;
        Ok((Preservation { xattrs, acl }, flags))
    }

    pub(crate) async fn receive(
        &mut self,
        staged: &mut dyn StagedWriter,
        rate_limiter: Option<&std::sync::Mutex<crate::sync::ratelimit::RateLimiter>>,
    ) -> Result<TransferSummary> {
        if !self.healthy || !self.prepared || self.received || !self.open.source_bytes {
            return Err(GroupError::Phase("source payload phase"));
        }
        self.healthy = false;
        let result = crate::remote::fetch::receive_payload(
            &mut self.inbox,
            self.open.file.file_size(),
            staged,
            crate::remote::fetch::FetchPreservationRequest::default(),
            rate_limiter,
            FrameFlags::ACK_REQUIRED,
        )
        .await?;
        self.received = true;
        self.healthy = true;
        Ok(result.summary)
    }

    /// Await this BEFORE scheduling a native publication. The reply is an
    /// observation, not an atomic guarantee across the network/syscall gap.
    pub(crate) async fn validate_member(&mut self, ordinal: u64) -> Result<()> {
        if !self.healthy || !self.received || ordinal != self.validated || ordinal >= self.count {
            return Err(GroupError::Phase("source validation phase or ordinal"));
        }
        self.healthy = false;
        self.send(
            FrameKind::GroupStep,
            WireGroupStep::Validate(ordinal).encode(),
        )
        .await?;
        if self.reply().await? != WireGroupResult::Validated(ordinal) {
            return Err(GroupError::Phase("source validation acknowledgement"));
        }
        self.validated += 1;
        self.healthy = true;
        Ok(())
    }

    pub(crate) async fn finish(mut self, group: &mut HardlinkGroupPlan<'_>) -> Result<()> {
        if !self.healthy || !self.received || self.validated != self.count {
            return Err(GroupError::Phase("source completion incomplete"));
        }
        group.verify_complete().await.map_err(|error| {
            GroupError::Transfer(crate::remote::transfer::RemoteTransferError::Io(
                std::io::Error::other(error),
            ))
        })?;
        let seal = self
            .seal
            .ok_or(GroupError::Phase("source declarations unsealed"))?;
        self.send(FrameKind::GroupStep, WireGroupStep::Finish(seal).encode())
            .await?;
        if self.reply().await? != WireGroupResult::SourceComplete(seal) {
            return Err(GroupError::Phase("source completion acknowledgement"));
        }
        Ok(())
    }

    async fn send(&self, kind: FrameKind, payload: bytes::Bytes) -> Result<()> {
        self.sender
            .send(Frame::new(
                kind,
                FrameFlags::empty(),
                self.inbox.stream_id(),
                payload,
            )?)
            .await?;
        Ok(())
    }

    async fn reply(&mut self) -> Result<WireGroupResult> {
        let routed = self
            .inbox
            .recv()
            .await?
            .ok_or(GroupError::Phase("source reply missing"))?;
        let frame = routed.frame();
        if frame.kind() != FrameKind::GroupResult || !frame.flags().is_empty() {
            return Err(GroupError::Phase("source reply frame"));
        }
        Ok(WireGroupResult::decode(frame.payload())?)
    }
}
