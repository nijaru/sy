//! The destination inbox stays registered through all publications and the
//! final original-creator proof acknowledgement, never just through FileEnd.
use super::declarations::add_to_seal;
use super::{GroupError, Result};
use crate::endpoint::io::Preservation;
use crate::engine::domain::EntryIdentity;
use crate::engine::work::TransferSummary;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, WireAcl, WireFileEnd, WireGroupMember, WireGroupOpen,
    WireGroupResult, WireGroupRole, WireGroupSeal, WireGroupStep, WireXattr, WireXattrResult,
};
use crate::remote::router::{RouterSender, StreamInbox};
use crate::remote::transfer::{ProducerItem, RemoteTransferError};

#[derive(Clone, Copy)]
pub(crate) struct DestinationReady {
    pub(crate) publish: bool,
    pub(crate) size: u64,
    pub(crate) digest: Option<[u8; 32]>,
    pub(crate) literal_bytes: u64,
    pub(crate) reused_bytes: u64,
}

pub(crate) struct DestinationGroup {
    sender: RouterSender,
    inbox: StreamInbox,
    open: WireGroupOpen,
    count: u64,
    digest: blake3::Hasher,
    seal: Option<WireGroupSeal>,
    prepared: bool,
    ready: Option<DestinationReady>,
    published: u64,
    healthy: bool,
}

impl DestinationGroup {
    pub(crate) async fn open(sender: RouterSender, open: WireGroupOpen) -> Result<Self> {
        if open.role != WireGroupRole::Destination {
            return Err(GroupError::Phase("destination group role"));
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
            ready: None,
            published: 0,
            healthy: true,
        })
    }

    pub(crate) async fn declare(&mut self, member: WireGroupMember) -> Result<()> {
        if !self.healthy || self.seal.is_some() || member.ordinal != self.count {
            self.healthy = false;
            return Err(GroupError::Phase(
                "destination declaration ordinal or phase",
            ));
        }
        self.healthy = false;
        add_to_seal(&mut self.digest, &member)?;
        self.send(FrameKind::GroupMember, member.encode()).await?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or(GroupError::Phase("destination declaration count overflow"))?;
        self.healthy = true;
        Ok(())
    }

    pub(crate) async fn prepare(
        &mut self,
        preservation: Preservation,
    ) -> Result<Option<DestinationReady>> {
        if !self.healthy
            || self.count == 0
            || self.prepared
            || preservation.xattrs.is_some() != self.open.xattrs
            || preservation.acl.is_some() != self.open.acls
        {
            self.healthy = false;
            return Err(GroupError::Phase(
                "destination preparation phase or metadata",
            ));
        }
        self.healthy = false;
        let seal = WireGroupSeal {
            count: self.count,
            digest: *self.digest.finalize().as_bytes(),
        };
        self.send(FrameKind::GroupMembersEnd, seal.encode()).await?;
        if self.reply().await? != WireGroupResult::Declarations(seal) {
            return Err(GroupError::Phase("destination declaration acknowledgement"));
        }
        self.seal = Some(seal);
        if let Some(xattrs) = preservation.xattrs {
            let entries = xattrs
                .into_iter()
                .map(|(name, value)| WireXattr::new(crate::remote::xattr::name_bytes(&name), value))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            self.send(
                FrameKind::FileXattrs,
                WireXattrResult::new(entries)?.encode()?,
            )
            .await?;
        }
        if let Some(acl) = preservation.acl {
            self.send(FrameKind::FileAcls, WireAcl::new(acl)?.encode())
                .await?;
        }
        self.send(FrameKind::GroupStep, WireGroupStep::Prepare.encode())
            .await?;
        self.prepared = true;
        // Canonical preparation starts a private receiver. Waiting for Ready
        // here would deadlock: readiness follows verified population.
        if !self.open.source_bytes {
            self.receive_ready().await?;
        }
        self.healthy = true;
        Ok(self.ready)
    }

    /// The factory starts the bounded byte producer only AFTER FileBegin was
    /// accepted by the transport. send_produced_file owns and joins its worker
    /// on success and every ordinary send failure.
    pub(crate) async fn populate(
        &mut self,
        produce: impl FnOnce() -> (
            tokio::sync::mpsc::Receiver<ProducerItem>,
            tokio::task::JoinHandle<std::result::Result<TransferSummary, RemoteTransferError>>,
        ),
    ) -> Result<DestinationReady> {
        if !self.healthy || !self.prepared || !self.open.source_bytes || self.ready.is_some() {
            self.healthy = false;
            return Err(GroupError::Phase("destination population phase"));
        }
        self.healthy = false;
        self.send(FrameKind::FileBegin, self.open.file.encode())
            .await?;
        let (receiver, worker) = produce();
        let summary = crate::remote::transfer::send_produced_file(
            &self.sender,
            self.inbox.stream_id(),
            receiver,
            worker,
        )
        .await?;
        if summary.file_size != self.open.file.file_size() {
            return Err(GroupError::Phase("destination population source size"));
        }
        self.sender
            .send(Frame::new(
                FrameKind::FileEnd,
                FrameFlags::ACK_REQUIRED,
                self.inbox.stream_id(),
                WireFileEnd::new(summary.file_size, summary.digest).encode(),
            )?)
            .await?;
        let ready = self.receive_ready().await?;
        if !ready.publish
            || ready.size != summary.file_size
            || ready.digest != Some(summary.digest)
            || ready.literal_bytes != summary.literal_bytes
            || ready.reused_bytes != summary.reused_bytes
        {
            return Err(GroupError::Phase(
                "destination verified population acknowledgement",
            ));
        }
        self.healthy = true;
        Ok(ready)
    }

    pub(crate) async fn publish(&mut self, ordinal: u64) -> Result<()> {
        if !self.healthy
            || self.ready.is_none()
            || ordinal != self.published
            || ordinal >= self.count
        {
            self.healthy = false;
            return Err(GroupError::Phase(
                "destination publication ordinal or phase",
            ));
        }
        self.healthy = false;
        self.send(
            FrameKind::GroupStep,
            WireGroupStep::Publish(ordinal).encode(),
        )
        .await?;
        if self.reply().await? != WireGroupResult::Published(ordinal) {
            return Err(GroupError::Phase("destination publication acknowledgement"));
        }
        self.published += 1;
        self.healthy = true;
        Ok(())
    }

    /// This reply certifies the declared native bindings through the original
    /// creator FD. It is not a whole-group rollback or source-removal receipt.
    pub(crate) async fn finish(mut self) -> Result<EntryIdentity> {
        if !self.healthy || self.ready.is_none() || self.published != self.count {
            return Err(GroupError::Phase("destination completion incomplete"));
        }
        let seal = self
            .seal
            .ok_or(GroupError::Phase("destination declarations unsealed"))?;
        self.send(FrameKind::GroupStep, WireGroupStep::Finish(seal).encode())
            .await?;
        match self.reply().await? {
            WireGroupResult::DestinationComplete {
                seal: accepted,
                identity,
            } if accepted == seal => Ok(EntryIdentity::from_bytes(identity)),
            _ => Err(GroupError::Phase("destination completion acknowledgement")),
        }
    }

    /// Only quiescent control phases promise an abort acknowledgement. During
    /// population or a lost reply, the session owner must fail and drain the
    /// connection; it must not reinterpret partial publication as rollback.
    pub(crate) async fn abort(mut self) -> Result<()> {
        if !self.healthy
            || self.seal.is_none()
            || (self.prepared && self.ready.is_none())
            || self.published != 0
        {
            return Err(GroupError::Phase(
                "destination abort requires session drain",
            ));
        }
        self.send(FrameKind::GroupStep, WireGroupStep::Abort.encode())
            .await?;
        if self.reply().await? != WireGroupResult::Aborted {
            return Err(GroupError::Phase("destination abort acknowledgement"));
        }
        Ok(())
    }

    async fn receive_ready(&mut self) -> Result<DestinationReady> {
        let WireGroupResult::DestinationReady {
            publish,
            size,
            digest,
            literal_bytes,
            reused_bytes,
        } = self.reply().await?
        else {
            return Err(GroupError::Phase("destination readiness"));
        };
        if (publish || self.open.verify_bytes) && digest.is_none() {
            return Err(GroupError::Phase(
                "destination readiness missing verification digest",
            ));
        }
        let ready = DestinationReady {
            publish,
            size,
            digest,
            literal_bytes,
            reused_bytes,
        };
        self.ready = Some(ready);
        Ok(ready)
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
            .ok_or(GroupError::Phase("destination reply missing"))?;
        let frame = routed.frame();
        if frame.kind() != FrameKind::GroupResult || !frame.flags().is_empty() {
            return Err(GroupError::Phase("destination reply kind or flags"));
        }
        Ok(WireGroupResult::decode(frame.payload())?)
    }
}
