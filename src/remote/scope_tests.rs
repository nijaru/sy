//! Real routed entry sessions must reject sibling authority before dispatch.
use super::{ClientRemoteSession, RemoteSessionError, ScopeError};
use crate::endpoint::existing::FingerprintOptions;
use crate::engine::domain::{EntryKind, RelativePath};
use crate::protocol::*;
use crate::remote::operand::RemoteOperand;
use crate::remote::router::RouterConfig;
use crate::remote::serve::{serve_transport, ServeError};
use crate::rooted_fs::operand::SourceShape;
use bytes::Bytes;
use futures::TryStreamExt;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

fn native_name() -> OsString {
    if cfg!(target_os = "linux") {
        OsString::from_vec(b"chosen-\xff".to_vec())
    } else {
        OsString::from("chosen-日")
    }
}

fn wire_path(path: &Path) -> RelativeWirePath {
    crate::remote::path::encode_relative_path(path).unwrap()
}

fn scan(scope: WireScanScope) -> Bytes {
    WireScanRequest {
        scope,
        respect_gitignore: false,
        include_git_dir: false,
        follow_symlinks: false,
        max_depth: None,
        unix_mode: true,
        symlink_target: true,
        identity: true,
        hardlink_group: false,
    }
    .encode()
}

fn identity(path: &Path, kind: EntryKind) -> [u8; 32] {
    *crate::endpoint::local_identity::metadata_identity(
        &std::fs::symlink_metadata(path).unwrap(),
        kind,
    )
    .unwrap()
    .as_bytes()
}

async fn reject(selected: &Path, operation: Operation, kind: FrameKind, payload: Bytes) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(server_io);
        let server = tokio::spawn(serve_transport(reader, writer, RouterConfig::default()));
        let (reader, writer) = tokio::io::split(client_io);
        let operand = if operation == Operation::Pull {
            RemoteOperand::Source { contents: false }
        } else {
            RemoteOperand::Destination {
                source: SourceShape::Leaf {
                    name: RelativePath::new("physical-source").unwrap(),
                },
            }
        };
        let mut client = ClientRemoteSession::connect_operand(
            reader,
            writer,
            operation,
            selected,
            &operand,
            RouterConfig::default(),
        )
        .await
        .unwrap();
        assert!(matches!(client.request_handle().binding().resolved, ResolvedOperand::Entry { .. }));
        let sender = client.sender();
        let mut inbox = sender.open_stream().unwrap();
        let flags = match kind {
            FrameKind::Metadata | FrameKind::Mutation => FrameFlags::FINAL | FrameFlags::ACK_REQUIRED,
            FrameKind::HashRequest | FrameKind::XattrRequest | FrameKind::AclRequest
            | FrameKind::BsdFlagsRequest => FrameFlags::FINAL,
            _ => FrameFlags::empty(),
        };
        sender
            .send(Frame::new(kind, flags, inbox.stream_id(), payload).unwrap())
            .await
            .unwrap();
        // No scan entries, bytes, signatures, metadata or mutation ACK can be
        // emitted: authorization fails before a producer/worker is admitted.
        assert!(!matches!(inbox.recv().await, Ok(Some(_))), "{operation:?} {kind:?} emitted a response");
        let error = server.await.unwrap().unwrap_err();
        assert!(matches!(error,
            ServeError::Session(RemoteSessionError::Scope(ScopeError::ScopeMismatch { kind: actual, .. })) if actual == kind),
            "unexpected refusal: {error}");
        let _ = client.shutdown().await;
    })
    .await
    .expect("scope refusal must close actors and drain owned work");
}

#[tokio::test]
async fn entry_sessions_refuse_tree_sibling_and_descendant_read_openers() {
    let fixture = tempfile::tempdir().unwrap();
    let selected = fixture.path().join(native_name());
    let sibling = fixture.path().join("sibling");
    std::fs::write(&selected, b"selected").unwrap();
    std::fs::write(&sibling, b"secret").unwrap();
    let sibling_identity = identity(&sibling, EntryKind::File);
    let sibling_path = wire_path(Path::new("sibling"));
    let source_metadata =
        WireSourceMetadataRead::new(sibling_path.clone(), WireEntryKind::File, sibling_identity)
            .unwrap()
            .encode()
            .unwrap();
    let mut descendant = selected.file_name().unwrap().to_os_string();
    descendant.push("/.");
    let descendant_native =
        crate::remote::encode_target_root(Path::new(&descendant), Platform::current().os).unwrap();
    // Keep the invalid spelling in ONE wire component, where Path::components
    // would silently normalize it; it must not gain selected-name authority.
    let descendant_wire =
        RelativeWirePath::from_components([descendant_native.as_bytes()]).unwrap();
    for operation in [Operation::Pull, Operation::Push] {
        let requests = [
            (FrameKind::ScanRequest, scan(WireScanScope::Tree)),
            (
                FrameKind::ScanRequest,
                scan(WireScanScope::Entry(
                    WirePath::new(Bytes::from_static(b"sibling")).unwrap(),
                )),
            ),
            (
                FrameKind::ScanRequest,
                scan(WireScanScope::Entry(descendant_native.clone())),
            ),
            (
                FrameKind::HashRequest,
                WireHashRequest::new(
                    sibling_path.clone(),
                    6,
                    sibling_identity,
                    FingerprintOptions::default(),
                )
                .encode()
                .unwrap(),
            ),
            (
                FrameKind::HashRequest,
                WireHashRequest::new(
                    descendant_wire.clone(),
                    8,
                    identity(&selected, EntryKind::File),
                    FingerprintOptions::default(),
                )
                .encode()
                .unwrap(),
            ),
            (
                FrameKind::HashRequest,
                WireHashRequest::new(
                    wire_path(&Path::new(selected.file_name().unwrap()).join("child")),
                    0,
                    sibling_identity,
                    FingerprintOptions::default(),
                )
                .encode()
                .unwrap(),
            ),
            (
                FrameKind::SignatureRequest,
                WireSignatureRequest {
                    path: sibling_path.clone(),
                    block_size: SignatureBlockSize::new(4096).unwrap(),
                    expected_identity: sibling_identity,
                }
                .encode(),
            ),
            (
                FrameKind::Metadata,
                WireObservationRead {
                    path: sibling_path.clone(),
                    kind: WireEntryKind::File,
                    identity: sibling_identity,
                }
                .encode()
                .unwrap(),
            ),
            (FrameKind::XattrRequest, source_metadata.clone()),
            (FrameKind::AclRequest, source_metadata.clone()),
            (FrameKind::BsdFlagsRequest, source_metadata.clone()),
        ];
        for (kind, payload) in requests {
            reject(&selected, operation, kind, payload).await;
        }
    }
    reject(
        &selected,
        Operation::Pull,
        FrameKind::FileFetchRequest,
        WireFileFetchRequest::new(
            sibling_path,
            6,
            sibling_identity,
            WireFetchCompression::None,
        )
        .encode(),
    )
    .await;
    assert_eq!(std::fs::read(&sibling).unwrap(), b"secret");
}

#[tokio::test]
async fn entry_push_refuses_sibling_mutators_without_native_effects() {
    let fixture = tempfile::tempdir().unwrap();
    let selected = fixture.path().join(native_name());
    let sibling = fixture.path().join("sibling");
    let directory = fixture.path().join("directory");
    std::fs::write(&selected, b"selected").unwrap();
    std::fs::write(&sibling, b"original sibling").unwrap();
    std::fs::create_dir(&directory).unwrap();
    let before = std::fs::symlink_metadata(&sibling).unwrap();
    let sibling_identity = identity(&sibling, EntryKind::File);
    let sibling_path = wire_path(Path::new("sibling"));
    let selected_path = wire_path(Path::new(selected.file_name().unwrap()));
    let directory_path = wire_path(Path::new("directory"));
    let metadata = WireMetadata::new(
        sibling_path.clone(),
        WireMetadataTarget::Observed {
            kind: WireEntryKind::File,
            identity: sibling_identity,
        },
        Some(0o600),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let requests = [
        (
            FrameKind::Mutation,
            WireMutation::remove_file_like(sibling_path.clone(), Some(sibling_identity))
                .encode()
                .unwrap(),
        ),
        (
            FrameKind::FileBegin,
            WireFileBegin::delta(
                sibling_path.clone(),
                0,
                WireFileBasis::new(before.len(), sibling_identity),
            )
            .encode(),
        ),
        (FrameKind::Metadata, metadata.encode().unwrap()),
        (
            FrameKind::Mutation,
            WireMutation::verify_publication(
                sibling_path.clone(),
                sibling_identity,
                WireEntryKind::File,
            )
            .encode()
            .unwrap(),
        ),
        (
            FrameKind::Mutation,
            WireMutation::copy_file(
                sibling_path.clone(),
                wire_path(Path::new("backup")),
                sibling_identity,
            )
            .encode()
            .unwrap(),
        ),
        (
            FrameKind::Mutation,
            WireMutation::copy_file(
                selected_path.clone(),
                selected_path.clone(),
                identity(&selected, EntryKind::File),
            )
            .encode()
            .unwrap(),
        ),
        (
            FrameKind::Mutation,
            WireMutation::hardlink(
                sibling_path.clone(),
                selected_path.clone(),
                sibling_identity,
                Some(identity(&selected, EntryKind::File)),
            )
            .encode()
            .unwrap(),
        ),
        (
            FrameKind::Mutation,
            WireMutation::hardlink(
                selected_path,
                sibling_path,
                identity(&selected, EntryKind::File),
                Some(sibling_identity),
            )
            .encode()
            .unwrap(),
        ),
        (
            FrameKind::Metadata,
            WireDirectoryMetadata {
                path: directory_path.clone(),
                identity: identity(&directory, EntryKind::Directory),
                action: WireDirectoryAction::Read {
                    xattrs: false,
                    acl: false,
                    flags: false,
                },
            }
            .encode()
            .unwrap(),
        ),
        (
            FrameKind::Metadata,
            WireDirectoryMetadata {
                path: directory_path,
                identity: identity(&directory, EntryKind::Directory),
                action: WireDirectoryAction::Finalize {
                    mode: Some(0o700),
                    modified: None,
                    preservation: WireDirectoryPreservation::default(),
                },
            }
            .encode()
            .unwrap(),
        ),
    ];
    for (kind, payload) in requests {
        reject(&selected, Operation::Push, kind, payload).await;
        let after = std::fs::symlink_metadata(&sibling).unwrap();
        assert_eq!(
            (
                after.dev(),
                after.ino(),
                after.mode(),
                after.ctime(),
                after.ctime_nsec()
            ),
            (
                before.dev(),
                before.ino(),
                before.mode(),
                before.ctime(),
                before.ctime_nsec()
            )
        );
        assert_eq!(std::fs::read(&sibling).unwrap(), b"original sibling");
        assert_eq!(std::fs::read(&selected).unwrap(), b"selected");
        assert!(!fixture.path().join("backup").exists());
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 3);
    }
}

#[tokio::test]
async fn pending_entry_sessions_cannot_scan_the_held_ancestor() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("sibling"), b"secret").unwrap();
    let selected = fixture.path().join("missing/parent").join(native_name());
    for operation in [Operation::Push, Operation::PreviewPush] {
        for scope in [
            WireScanScope::Tree,
            WireScanScope::Entry(WirePath::new(Bytes::from_static(b"sibling")).unwrap()),
        ] {
            reject(&selected, operation, FrameKind::ScanRequest, scan(scope)).await;
        }
    }
    assert!(!fixture.path().join("missing").exists());
    assert_eq!(
        std::fs::read(fixture.path().join("sibling")).unwrap(),
        b"secret"
    );
}

#[tokio::test]
async fn selected_entry_reads_and_backup_retain_original_parent_fd_after_relocation() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for operation in [Operation::Pull, Operation::Push] {
            let fixture = tempfile::tempdir().unwrap();
            let root = fixture.path().join("root");
            let moved = fixture.path().join("moved");
            std::fs::create_dir(&root).unwrap();
            let name = native_name();
            std::fs::write(root.join(&name), b"original selected").unwrap();
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (reader, writer) = tokio::io::split(server_io);
            let server = tokio::spawn(serve_transport(reader, writer, RouterConfig::default()));
            let (reader, writer) = tokio::io::split(client_io);
            let operand = if operation == Operation::Pull {
                RemoteOperand::Source { contents: false }
            } else {
                RemoteOperand::Destination {
                    source: SourceShape::Leaf {
                        name: RelativePath::new("physical-source").unwrap(),
                    },
                }
            };
            let mut client = ClientRemoteSession::connect_operand(
                reader,
                writer,
                operation,
                &root.join(&name),
                &operand,
                RouterConfig::default(),
            )
            .await
            .unwrap();
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join(&name), b"replacement root victim").unwrap();
            let entries = client
                .request_handle()
                .scan_entry(Default::default(), RelativePath::new(&name).unwrap())
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(
                client.content_hash(&entries[0]).await.unwrap(),
                *blake3::hash(b"original selected").as_bytes()
            );
            if operation == Operation::Push {
                client
                    .copy_file(
                        &entries[0].path,
                        &RelativePath::new("backups/selected~").unwrap(),
                        entries[0].identity.unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    std::fs::read(moved.join("backups/selected~")).unwrap(),
                    b"original selected"
                );
                assert!(!root.join("backups").exists());
            }
            client.finish().await.unwrap();
            server.await.unwrap().unwrap();
            assert_eq!(
                std::fs::read(root.join(&name)).unwrap(),
                b"replacement root victim"
            );
        }
    })
    .await
    .expect("selected requests and owned shutdown must complete");
}
