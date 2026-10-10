//! Native/wire operand boundary; classification belongs to the session owner.
use super::{decode_native_root, encode_target_root, RemoteError, Result};
use crate::engine::domain::RelativePath;
use crate::protocol::{OperandRequest, PlatformOs, SourceShape as WireShape, WirePath};
use crate::rooted_fs::operand::SourceShape;

#[derive(Debug, Clone)]
pub enum RemoteOperand {
    Source { contents: bool },
    Destination { source: SourceShape },
}

impl RemoteOperand {
    pub(super) fn encode(&self, peer: PlatformOs) -> Result<OperandRequest> {
        Ok(match self {
            Self::Source { contents } => OperandRequest::Source {
                contents: *contents,
            },
            Self::Destination { source } => OperandRequest::Destination {
                source: encode_shape(source, peer)?,
            },
        })
    }
}

pub(super) fn encode_shape(shape: &SourceShape, peer: PlatformOs) -> Result<WireShape> {
    Ok(match shape {
        SourceShape::Directory { basename } => WireShape::Directory {
            basename: basename
                .as_ref()
                .map(|name| encode_target_root(name.as_path(), peer))
                .transpose()?,
        },
        SourceShape::Leaf { name } => WireShape::Leaf {
            name: encode_target_root(name.as_path(), peer)?,
        },
    })
}

pub fn decode_name(name: WirePath) -> Result<RelativePath> {
    let path = decode_native_root(name)?;
    let mut components = path.components();
    if !matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) || path.as_os_str() != path.file_name().unwrap_or_default()
    {
        return Err(RemoteError::InvalidRoot(
            "operand name is not exactly one normal component",
        ));
    }
    RelativePath::new(path).map_err(|_| RemoteError::InvalidRoot("invalid operand name"))
}

pub fn decode_shape(shape: WireShape) -> Result<SourceShape> {
    Ok(match shape {
        WireShape::Directory { basename } => SourceShape::Directory {
            basename: basename.map(decode_name).transpose()?,
        },
        WireShape::Leaf { name } => SourceShape::Leaf {
            name: decode_name(name)?,
        },
    })
}

// Keep root/operand paths native; no display-string round-trip.
pub(super) fn native_shape(shape: &SourceShape) -> Result<WireShape> {
    encode_shape(shape, crate::protocol::Platform::current().os)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::engine::namespace::NamespaceSemantics;
    use crate::protocol::Operation;
    use crate::remote::router::RouterConfig;
    use crate::remote::runtime::ClientRemoteSession;
    use futures::TryStreamExt;
    use std::path::Path;

    #[tokio::test]
    async fn pending_root_acquisition_is_selected_and_all_handlers_retain_its_fd() {
        for wrong_target in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let root = fixture.path().join("missing/nested");
            let destination = root.join("renamed");
            let name = RelativePath::new("original").unwrap();
            let selected = RelativePath::new("renamed").unwrap();
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (reader, writer) = tokio::io::split(client_io);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let server = tokio::spawn(crate::remote::serve::serve_transport(
                server_reader,
                server_writer,
                RouterConfig::default(),
            ));
            let mut client = ClientRemoteSession::connect_operand(
                reader,
                writer,
                Operation::Push,
                &destination,
                &RemoteOperand::Destination {
                    source: SourceShape::Leaf { name: name.clone() },
                },
                RouterConfig::default(),
            )
            .await
            .unwrap();
            let remote = client.request_handle();
            assert!(remote.binding().pending);
            assert_eq!(
                remote.namespace_semantics(),
                NamespaceSemantics::UNSPECIFIED
            );
            assert_eq!(remote.binding().modtime_precision_ns, 0);
            let scan = remote
                .scan_entry(Default::default(), selected.clone())
                .await
                .unwrap();
            assert!(scan.try_collect::<Vec<_>>().await.unwrap().is_empty());
            assert!(!root.parent().unwrap().exists());
            if wrong_target {
                assert!(remote.acquire_root_for_create(&name).await.is_err());
                assert!(server.await.unwrap().is_err());
                assert!(!root.parent().unwrap().exists());
                let _ = client.shutdown().await;
            } else {
                remote.acquire_root_for_create(&selected).await.unwrap();
                assert!(!remote.binding().pending);
                let actual = crate::rooted_fs::RootedFs::open(root.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    remote.namespace_semantics(),
                    actual.namespace_semantics().await.unwrap()
                );
                let held = fixture.path().join("held");
                std::fs::rename(&root, &held).unwrap();
                let foreign = fixture.path().join("foreign");
                std::fs::create_dir(&foreign).unwrap();
                std::os::unix::fs::symlink(&foreign, &root).unwrap();
                remote
                    .replace_symlink(&selected, Path::new("opaque-target"), None, None)
                    .await
                    .unwrap();
                let scan = remote
                    .scan_entry(Default::default(), selected)
                    .await
                    .unwrap();
                let entries: Vec<_> = scan.try_collect().await.unwrap();
                assert_eq!(entries.len(), 1);
                assert_eq!(
                    std::fs::read_link(held.join("renamed")).unwrap(),
                    Path::new("opaque-target")
                );
                assert!(!foreign.join("renamed").exists());
                client.finish().await.unwrap();
                server.await.unwrap().unwrap();
            }
        }
    }
}
