#[cfg(all(test, unix))]
#[path = "source_tests.rs"]
mod tests;
use super::declarations::{Cursor, Declarations};
use super::{next_frame, send_result, GroupError, Result};
use crate::engine::domain::EntryIdentity;
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ResolvedOperand, WireAcl, WireEntryKind,
    WireFetchCompression, WireFileEnd, WireGroupMember, WireGroupOpen, WireGroupResult,
    WireGroupRole, WireGroupSeal, WireGroupStep, WireXattr, WireXattrResult,
};
use crate::remote::path::decode_relative_path;
use crate::remote::router::{IncomingStream, RouterSender};
use crate::remote::source_file::{SourceFile, SourceMetadataRequest};
use crate::rooted_fs::RootedFs;
use bytes::Bytes;

struct Lease {
    declarations: Declarations,
    source: SourceFile,
    completion: Option<Cursor>,
    seal: WireGroupSeal,
}

impl Lease {
    fn validate_all(&mut self, peer: PlatformOs) -> Result<()> {
        let mut cursor = self.declarations.cursor()?;
        while let Some(member) = self.declarations.next(&mut cursor)? {
            self.validate_member(member, peer)?;
        }
        self.source.validate_blocking()?;
        Ok(())
    }

    fn validate_member(&self, member: WireGroupMember, peer: PlatformOs) -> Result<()> {
        let Some((WireEntryKind::File, identity)) = member.observation else {
            return Err(GroupError::Phase(
                "source member requires a regular-file observation",
            ));
        };
        self.source.validate_member_blocking(
            &decode_relative_path(member.path, peer)?,
            EntryIdentity::from_bytes(identity),
        )?;
        Ok(())
    }
}

/// The handler task owns the original source FD through the client's exact
/// completion request. It never mutates the source or reopens its root path.
pub(super) async fn serve(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
    resolved: ResolvedOperand,
) -> Result<()> {
    let IncomingStream { first, mut inbox } = incoming;
    let stream = inbox.stream_id();
    if first.frame().kind() != FrameKind::GroupOpen
        || first.frame().stream_id() != stream
        || !first.frame().flags().is_empty()
    {
        return Err(GroupError::Phase("invalid group opener stream or flags"));
    }
    let open = WireGroupOpen::decode(first.frame().payload())?;
    if open.role != WireGroupRole::Source || open.retain_metadata {
        return Err(GroupError::Phase(
            "invalid source lease role or retained destination metadata",
        ));
    }
    drop(first);
    let mut declarations = tokio::task::spawn_blocking(Declarations::new)
        .await
        .map_err(|error| GroupError::Worker(error.to_string()))??;
    let sealed = loop {
        let routed = next_frame(&mut inbox).await?;
        let frame = routed.frame();
        match frame.kind() {
            FrameKind::GroupMember => {
                crate::remote::runtime::scope::authorize(&resolved, frame, peer)?;
                let member = WireGroupMember::decode(frame.payload())?;
                if !matches!(member.observation, Some((WireEntryKind::File, _))) {
                    return Err(GroupError::Phase("source member requires file identity"));
                }
                declarations = tokio::task::spawn_blocking(move || {
                    declarations.append(&member)?;
                    Ok::<_, GroupError>(declarations)
                })
                .await
                .map_err(|error| GroupError::Worker(error.to_string()))??;
            }
            FrameKind::GroupMembersEnd => break WireGroupSeal::decode(frame.payload())?,
            _ => return Err(GroupError::Phase("expected source declarations")),
        }
    };
    let open_for_worker = open.clone();
    let lease = tokio::task::spawn_blocking(move || {
        declarations.seal(sealed)?;
        let basis = open_for_worker
            .file
            .basis()
            .ok_or(GroupError::Phase("source observation missing"))?;
        let mut cursor = declarations.cursor()?;
        let first = declarations
            .next(&mut cursor)?
            .ok_or(GroupError::Phase("empty source declarations"))?;
        if first.path != open_for_worker.file.path
            || first.observation != Some((WireEntryKind::File, basis.identity()))
            || basis.file_size() != open_for_worker.file.file_size()
        {
            return Err(GroupError::Phase(
                "source opener differs from first declaration",
            ));
        }
        let source = SourceFile::capture_blocking(
            rooted,
            decode_relative_path(first.path, peer)?,
            EntryIdentity::from_bytes(basis.identity()),
            basis.file_size(),
        )?;
        let mut lease = Lease {
            declarations,
            source,
            completion: None,
            seal: sealed,
        };
        lease.validate_all(peer)?;
        Ok::<_, GroupError>(lease)
    })
    .await
    .map_err(|error| GroupError::Worker(error.to_string()))??;
    send_result(sender, stream, WireGroupResult::Declarations(sealed)).await?;

    // Preserve native-owner destruction on joined blocking work on all ordinary
    // outcomes, including peer error/EOF while waiting for a control message.
    let mut lease = Some(lease);
    let result: Result<()> = async {
        let routed = next_frame(&mut inbox).await?;
        if routed.frame().kind() != FrameKind::GroupStep
            || WireGroupStep::decode(routed.frame().payload())? != WireGroupStep::Prepare
        {
            return Err(GroupError::Phase("source lease must be prepared once"));
        }
        drop(routed);
        let native = lease
            .take()
            .ok_or(GroupError::Phase("source lease ownership"))?;
        let request = SourceMetadataRequest {
            xattrs: open.xattrs,
            acls: open.acls,
            bsd_flags: open.source_flags,
        };
        let (native, metadata, flags, reader) = tokio::task::spawn_blocking(move || {
            let (metadata, flags) = native.source.read_metadata_blocking(request)?;
            let reader = if open.source_bytes {
                Some(native.source.clone_reader_blocking()?)
            } else {
                None
            };
            Ok::<_, GroupError>((native, metadata, flags, reader))
        })
        .await
        .map_err(|error| GroupError::Worker(error.to_string()))??;
        lease = Some(native);
        send_result(sender, stream, WireGroupResult::SourceReady { flags }).await?;
        if let Some(xattrs) = metadata.xattrs {
            let entries = xattrs
                .into_iter()
                .map(|(name, value)| {
                    WireXattr::new(crate::remote::xattr::name_bytes(&name), Bytes::from(value))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            sender
                .send(Frame::new(
                    FrameKind::FileXattrs,
                    FrameFlags::empty(),
                    stream,
                    WireXattrResult::new(entries)?.encode()?,
                )?)
                .await?;
        }
        if let Some(acl) = metadata.acl {
            sender
                .send(Frame::new(
                    FrameKind::FileAcls,
                    FrameFlags::empty(),
                    stream,
                    WireAcl::new(acl)?.encode(),
                )?)
                .await?;
        }
        if let Some(mut reader) = reader {
            let compression = match open.compression {
                WireFetchCompression::None => None,
                WireFetchCompression::Auto => {
                    Some(crate::engine::compression::CompressionPolicy::Auto)
                }
                WireFetchCompression::Always => {
                    Some(crate::engine::compression::CompressionPolicy::Always)
                }
            };
            let expected_size = open.file.file_size();
            let identity = EntryIdentity::from_bytes(
                open.file
                    .basis()
                    .ok_or(GroupError::Phase("source observation"))?
                    .identity(),
            );
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let worker = tokio::task::spawn_blocking(move || {
                crate::remote::transfer::validate_source(&reader, identity, expected_size)?;
                let summary = crate::remote::transfer::produce_whole(
                    &mut reader,
                    expected_size,
                    tx,
                    compression.as_ref(),
                )?;
                crate::remote::transfer::validate_source(&reader, identity, expected_size)?;
                Ok(summary)
            });
            let summary =
                crate::remote::transfer::send_produced_file(sender, stream, rx, worker).await?;
            let native = lease
                .take()
                .ok_or(GroupError::Phase("source lease ownership"))?;
            let native = tokio::task::spawn_blocking(move || {
                native.source.validate_blocking()?;
                Ok::<_, GroupError>(native)
            })
            .await
            .map_err(|error| GroupError::Worker(error.to_string()))??;
            lease = Some(native);
            sender
                .send(Frame::new(
                    FrameKind::FileEnd,
                    FrameFlags::ACK_REQUIRED,
                    stream,
                    WireFileEnd::new(summary.file_size, summary.digest).encode(),
                )?)
                .await?;
        }
        let mut ordinal = 0;
        loop {
            let routed = next_frame(&mut inbox).await?;
            if routed.frame().kind() != FrameKind::GroupStep {
                return Err(GroupError::Phase(
                    "expected source validation or completion",
                ));
            }
            let step = WireGroupStep::decode(routed.frame().payload())?;
            drop(routed);
            match step {
                WireGroupStep::Validate(requested) if requested == ordinal => {
                    let mut native = lease
                        .take()
                        .ok_or(GroupError::Phase("source lease ownership"))?;
                    let native = tokio::task::spawn_blocking(move || {
                        let mut cursor = match native.completion.take() {
                            Some(cursor) => cursor,
                            None => native.declarations.cursor()?,
                        };
                        let member = native
                            .declarations
                            .next(&mut cursor)?
                            .ok_or(GroupError::Phase("extra source validation"))?;
                        native.validate_member(member, peer)?;
                        native.completion = Some(cursor);
                        Ok::<_, GroupError>(native)
                    })
                    .await
                    .map_err(|error| GroupError::Worker(error.to_string()))??;
                    lease = Some(native);
                    send_result(sender, stream, WireGroupResult::Validated(ordinal)).await?;
                    ordinal = ordinal
                        .checked_add(1)
                        .ok_or(GroupError::Phase("source validation count overflow"))?;
                }
                WireGroupStep::Finish(seal) if seal == sealed && ordinal == sealed.count => {
                    let mut native = lease
                        .take()
                        .ok_or(GroupError::Phase("source lease ownership"))?;
                    tokio::task::spawn_blocking(move || {
                        if native.seal != seal {
                            return Err(GroupError::Phase("source completion seal"));
                        }
                        let mut cursor = native
                            .completion
                            .take()
                            .ok_or(GroupError::Phase("source completion missing"))?;
                        if native.declarations.next(&mut cursor)?.is_some() {
                            return Err(GroupError::Phase("source completion incomplete"));
                        }
                        native.validate_all(peer)
                    })
                    .await
                    .map_err(|error| GroupError::Worker(error.to_string()))??;
                    send_result(sender, stream, WireGroupResult::SourceComplete(seal)).await?;
                    return Ok(());
                }
                WireGroupStep::Abort => {
                    let native = lease.take();
                    tokio::task::spawn_blocking(move || drop(native))
                        .await
                        .map_err(|error| GroupError::Worker(error.to_string()))?;
                    send_result(sender, stream, WireGroupResult::Aborted).await?;
                    return Ok(());
                }
                _ => {
                    return Err(GroupError::Phase(
                        "source member ordinal or completion mismatch",
                    ))
                }
            }
        }
    }
    .await;
    tokio::task::spawn_blocking(move || drop(lease))
        .await
        .map_err(|error| GroupError::Worker(error.to_string()))?;
    result
}
