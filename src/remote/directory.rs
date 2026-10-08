//! Directory-only observation-bound preservation RPCs on Metadata streams.
use super::metadata::{RemoteMetadataError, Result};
use crate::engine::domain::{EntryIdentity, RelativePath, Timestamp};
use crate::protocol::{
    Frame, FrameFlags, FrameKind, PlatformOs, ProtocolError, WireAcl, WireDirectoryAction,
    WireDirectoryMetadata, WireDirectoryPreservation, WireXattr, WireXattrResult,
};
use crate::remote::path::{
    decode_relative_path, encode_relative_path, ensure_compatible_path_encoding,
};
use crate::remote::router::{IncomingStream, RouterSender};
use crate::rooted_fs::{DirectoryPreservation, DirectoryPreservationRequest, RootedFs};
use bytes::Bytes;

fn to_wire(value: &DirectoryPreservation) -> Result<WireDirectoryPreservation> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let xattrs = value
            .xattrs
            .as_ref()
            .map(|entries| {
                WireXattrResult::new(
                    entries
                        .iter()
                        .map(|(name, value)| {
                            WireXattr::new(
                                Bytes::copy_from_slice(name.as_bytes()),
                                Bytes::copy_from_slice(value),
                            )
                        })
                        .collect::<crate::protocol::Result<Vec<_>>>()?,
                )
            })
            .transpose()?;
        Ok(WireDirectoryPreservation {
            xattrs,
            acl: value.acl.clone().map(WireAcl::new).transpose()?,
            flags: value.bsd_flags,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = value;
        Err(ProtocolError::InvalidMessage("directory preservation requires Unix").into())
    }
}
fn from_wire(value: WireDirectoryPreservation) -> Result<DirectoryPreservation> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(DirectoryPreservation {
            xattrs: value.xattrs.map(|set| {
                set.entries()
                    .iter()
                    .map(|entry| {
                        (
                            std::ffi::OsString::from_vec(entry.name().to_vec()),
                            entry.value().to_vec(),
                        )
                    })
                    .collect()
            }),
            acl: value.acl.map(|acl| acl.text().to_owned()),
            bsd_flags: value.flags,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = value;
        Err(ProtocolError::InvalidMessage("directory preservation requires Unix").into())
    }
}
async fn request(
    sender: &RouterSender,
    path: &RelativePath,
    expected: EntryIdentity,
    action: WireDirectoryAction,
    peer: PlatformOs,
) -> Result<Bytes> {
    ensure_compatible_path_encoding(peer)?;
    let reading = matches!(action, WireDirectoryAction::Read { .. });
    let request = WireDirectoryMetadata {
        path: encode_relative_path(path.as_path())?,
        identity: *expected.as_bytes(),
        action,
    };
    let mut inbox = sender.open_stream()?;
    let id = inbox.stream_id();
    sender
        .send(Frame::new(
            FrameKind::Metadata,
            FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            id,
            request.encode()?,
        )?)
        .await?;
    let routed = inbox
        .recv()
        .await?
        .ok_or(RemoteMetadataError::UnexpectedStreamEnd {
            stream_id: id.get(),
        })?;
    let frame = routed.frame();
    if frame.stream_id() != id
        || frame.kind() != FrameKind::Ack
        || !frame.flags().is_empty()
        || (!reading && !frame.payload().is_empty())
    {
        return Err(RemoteMetadataError::InvalidAck);
    }
    Ok(frame.payload().clone())
}
pub async fn read(
    sender: &RouterSender,
    path: &RelativePath,
    expected: EntryIdentity,
    policy: DirectoryPreservationRequest,
    peer: PlatformOs,
) -> Result<DirectoryPreservation> {
    let bytes = request(
        sender,
        path,
        expected,
        WireDirectoryAction::Read {
            xattrs: policy.xattrs,
            acl: policy.acl,
            flags: policy.bsd_flags,
        },
        peer,
    )
    .await?;
    let preservation = WireDirectoryPreservation::decode(&bytes)?;
    if preservation.xattrs.is_some() != policy.xattrs
        || preservation.acl.is_some() != policy.acl
        || preservation.flags.is_some() != policy.bsd_flags
    {
        return Err(ProtocolError::InvalidMessage(
            "directory read omitted or added preservation fields",
        )
        .into());
    }
    from_wire(preservation)
}
pub async fn finalize(
    sender: &RouterSender,
    path: &RelativePath,
    expected: EntryIdentity,
    mode: Option<u32>,
    modified: Option<Timestamp>,
    preservation: &DirectoryPreservation,
    peer: PlatformOs,
) -> Result<()> {
    request(
        sender,
        path,
        expected,
        WireDirectoryAction::Finalize {
            mode,
            modified: modified.map(|value| (value.seconds(), value.nanoseconds())),
            preservation: to_wire(preservation)?,
        },
        peer,
    )
    .await?;
    Ok(())
}
pub async fn serve(
    rooted: RootedFs,
    incoming: IncomingStream,
    sender: &RouterSender,
    peer: PlatformOs,
) -> Result<()> {
    ensure_compatible_path_encoding(peer)?;
    let frame = incoming.first.frame();
    if frame.kind() != FrameKind::Metadata
        || frame.flags() != FrameFlags::FINAL | FrameFlags::ACK_REQUIRED
    {
        return Err(RemoteMetadataError::InvalidAck);
    }
    let id = frame.stream_id();
    let request = WireDirectoryMetadata::decode(frame.payload())?;
    let path = decode_relative_path(request.path, peer)?;
    let expected = EntryIdentity::from_bytes(request.identity);
    drop(incoming);
    let result = tokio::task::spawn_blocking(move || -> Result<Bytes> {
        match request.action {
            WireDirectoryAction::Read { xattrs, acl, flags } => {
                let value = rooted.read_directory_preservation_blocking(
                    &path,
                    expected,
                    DirectoryPreservationRequest {
                        xattrs,
                        acl,
                        bsd_flags: flags,
                    },
                )?;
                Ok(to_wire(&value)?.encode()?)
            }
            WireDirectoryAction::Finalize {
                mode,
                modified,
                preservation,
            } => {
                let modified = modified
                    .map(|(s, n)| Timestamp::new(s, n))
                    .transpose()
                    .map_err(|_| ProtocolError::InvalidMessage("directory timestamp"))?;
                rooted.finalize_directory_blocking(
                    &path,
                    expected,
                    mode,
                    modified,
                    &from_wire(preservation)?,
                )?;
                Ok(Bytes::new())
            }
        }
    })
    .await
    .map_err(|error| RemoteMetadataError::Worker(error.to_string()))??;
    sender
        .send(Frame::new(FrameKind::Ack, FrameFlags::empty(), id, result)?)
        .await?;
    Ok(())
}
