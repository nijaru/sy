use super::*;
use crate::protocol::{
    Frame, Platform, RelativeWirePath, WireFetchCompression, WireFileEnd, WireXattr,
};
use crate::remote::group::declarations::add_to_seal;
use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
use bytes::Bytes;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use tempfile::TempDir;
use tokio::task::JoinHandle;

const CANONICAL: &[u8] = b"canonical";
fn path(name: &str) -> RelativeWirePath {
    crate::remote::path::encode_relative_path(Path::new(name)).unwrap()
}
fn open(source_bytes: bool, retain_metadata: bool, mode: u32) -> WireGroupOpen {
    WireGroupOpen {
        role: WireGroupRole::Destination,
        source_bytes,
        retain_metadata,
        xattrs: false,
        acls: false,
        source_flags: false,
        verify_bytes: false,
        compression: WireFetchCompression::None,
        file: WireFileBegin::whole(path("first"), CANONICAL.len() as u64)
            .with_metadata(Some(mode), None)
            .unwrap(),
    }
}
fn old_group() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("first"), b"OLD").unwrap();
    std::fs::set_permissions(
        root.path().join("first"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    std::fs::hard_link(root.path().join("first"), root.path().join("second")).unwrap();
    std::fs::hard_link(root.path().join("first"), root.path().join("outside")).unwrap();
    root
}

struct Session {
    root: TempDir,
    _client: FrameRouter,
    _peer: FrameRouter,
    sender: RouterSender,
    inbox: StreamInbox,
    server: JoinHandle<Result<()>>,
    seal: WireGroupSeal,
    open: WireGroupOpen,
}
impl Session {
    async fn start(
        root: TempDir,
        mut open: WireGroupOpen,
        names: &[&str],
        copy_basis: bool,
    ) -> Self {
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let worker_root = rooted.clone();
        let names: Vec<_> = names.iter().map(|name| path(name)).collect();
        let members = tokio::task::spawn_blocking(move || {
            names
                .into_iter()
                .enumerate()
                .map(|(ordinal, path)| {
                    let relative =
                        decode_relative_path(path.clone(), Platform::current().os).unwrap();
                    let observation = worker_root.path_identity_blocking(&relative).unwrap().map(
                        |(kind, identity)| {
                            let kind = match kind {
                                EntryKind::File => WireEntryKind::File,
                                EntryKind::Directory => WireEntryKind::Directory,
                                EntryKind::Symlink => WireEntryKind::Symlink,
                            };
                            (kind, *identity.as_bytes())
                        },
                    );
                    WireGroupMember {
                        ordinal: ordinal as u64,
                        path,
                        observation,
                    }
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        if copy_basis {
            open.file = WireFileBegin::delta(
                path("first"),
                CANONICAL.len() as u64,
                WireFileBasis::new(3, members[0].observation.unwrap().1),
            )
            .with_metadata(open.file.unix_mode(), open.file.modified())
            .unwrap();
        }
        let mut digest = blake3::Hasher::new();
        for member in &members {
            add_to_seal(&mut digest, member).unwrap();
        }
        let seal = WireGroupSeal {
            count: members.len() as u64,
            digest: *digest.finalize().as_bytes(),
        };
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let client =
            FrameRouter::start(cr, cw, RouterRole::Client, RouterConfig::default()).unwrap();
        let mut peer =
            FrameRouter::start(sr, sw, RouterRole::Server, RouterConfig::default()).unwrap();
        let sender = client.sender();
        let inbox = sender.open_stream().unwrap();
        sender
            .send(
                Frame::new(
                    FrameKind::GroupOpen,
                    FrameFlags::empty(),
                    inbox.stream_id(),
                    open.encode(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let incoming = peer.incoming().recv().await.unwrap().unwrap();
        let peer_sender = peer.sender();
        // Keep the session transport alive beyond the handler: send queues a
        // terminal reply, so dropping its router here can discard that receipt.
        let server = tokio::spawn(async move {
            crate::remote::group::serve_destination(
                rooted,
                incoming,
                &peer_sender,
                Platform::current().os,
                ResolvedOperand::Tree,
            )
            .await
        });
        let mut session = Self {
            root,
            _client: client,
            _peer: peer,
            sender,
            inbox,
            server,
            seal,
            open,
        };
        for member in members {
            session.send(FrameKind::GroupMember, member.encode()).await;
        }
        session
            .send(FrameKind::GroupMembersEnd, seal.encode())
            .await;
        assert_eq!(session.reply().await, WireGroupResult::Declarations(seal));
        session
    }
    async fn send(&self, kind: FrameKind, payload: Bytes) {
        self.send_flags(kind, FrameFlags::empty(), payload).await;
    }
    async fn send_flags(&self, kind: FrameKind, flags: FrameFlags, payload: Bytes) {
        self.sender
            .send(Frame::new(kind, flags, self.inbox.stream_id(), payload).unwrap())
            .await
            .unwrap();
    }
    async fn step(&self, step: WireGroupStep) {
        self.send(FrameKind::GroupStep, step.encode()).await;
    }
    async fn reply(&mut self) -> WireGroupResult {
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), self.inbox.recv())
            .await
            .unwrap()
            .unwrap();
        let routed = match received {
            Some(routed) => routed,
            None => panic!("group ended before reply: {:?}", (&mut self.server).await),
        };
        assert_eq!(routed.frame().kind(), FrameKind::GroupResult);
        WireGroupResult::decode(routed.frame().payload()).unwrap()
    }
    async fn prepare(&mut self) -> WireGroupResult {
        self.step(WireGroupStep::Prepare).await;
        if self.open.source_bytes {
            self.send(FrameKind::FileBegin, self.open.file.encode())
                .await;
            self.send(FrameKind::Data, Bytes::from_static(CANONICAL))
                .await;
            self.send_flags(
                FrameKind::FileEnd,
                FrameFlags::ACK_REQUIRED,
                WireFileEnd::new(CANONICAL.len() as u64, *blake3::hash(CANONICAL).as_bytes())
                    .encode(),
            )
            .await;
        }
        self.reply().await
    }
    async fn publish(&mut self, ordinal: u64) {
        self.step(WireGroupStep::Publish(ordinal)).await;
        assert_eq!(self.reply().await, WireGroupResult::Published(ordinal));
    }
    async fn complete(&mut self) -> EntryIdentity {
        self.step(WireGroupStep::Finish(self.seal)).await;
        let WireGroupResult::DestinationComplete { seal, identity } = self.reply().await else {
            panic!("missing final proof")
        };
        assert_eq!(seal, self.seal);
        (&mut self.server).await.unwrap().unwrap();
        EntryIdentity::from_bytes(identity)
    }
    async fn failure(self) -> GroupError {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
    }
}

#[tokio::test]
async fn canonical_group_publishes_sequentially_before_exact_final_proof() {
    let mut session = Session::start(
        old_group(),
        open(true, true, 0o600),
        &["first", "second"],
        false,
    )
    .await;
    assert_eq!(
        session.prepare().await,
        WireGroupResult::DestinationReady {
            publish: true,
            size: 9,
            digest: Some(*blake3::hash(CANONICAL).as_bytes()),
            literal_bytes: 9,
            reused_bytes: 0,
        }
    );
    assert_eq!(
        std::fs::read(session.root.path().join("first")).unwrap(),
        b"OLD"
    );
    session.publish(0).await;
    assert_eq!(
        std::fs::read(session.root.path().join("first")).unwrap(),
        CANONICAL
    );
    assert_eq!(
        std::fs::read(session.root.path().join("second")).unwrap(),
        b"OLD"
    );
    assert!(!session.server.is_finished());
    session.publish(1).await;
    let first = std::fs::metadata(session.root.path().join("first")).unwrap();
    let second = std::fs::metadata(session.root.path().join("second")).unwrap();
    assert_eq!(first.ino(), second.ino());
    assert_eq!(first.permissions().mode() & 0o7777, 0o600);
    assert_eq!(
        std::fs::metadata(session.root.path().join("outside"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
    assert_eq!(
        std::fs::read(session.root.path().join("outside")).unwrap(),
        b"OLD"
    );
    let identity = session.complete().await;
    let final_metadata = std::fs::metadata(session.root.path().join("first")).unwrap();
    assert_eq!(final_metadata.nlink(), 2);
    assert_eq!(
        Some(identity),
        crate::endpoint::local_identity::metadata_identity(&final_metadata, EntryKind::File)
    );
}

#[tokio::test]
async fn retained_bytes_metadata_effect_isolates_outside_alias_and_reports_actual_size() {
    let root = old_group();
    let outside = std::fs::metadata(root.path().join("outside")).unwrap();
    let mut session =
        Session::start(root, open(false, true, 0o600), &["first", "second"], false).await;
    assert_eq!(
        session.prepare().await,
        WireGroupResult::DestinationReady {
            publish: true,
            size: 3,
            digest: Some(*blake3::hash(b"OLD").as_bytes()),
            literal_bytes: 0,
            reused_bytes: 3,
        }
    );
    session.publish(0).await;
    session.publish(1).await;
    assert_eq!(
        std::fs::read(session.root.path().join("first")).unwrap(),
        b"OLD"
    );
    let first = std::fs::metadata(session.root.path().join("first")).unwrap();
    let outside_after = std::fs::metadata(session.root.path().join("outside")).unwrap();
    assert_ne!(first.ino(), outside.ino());
    assert_eq!(
        outside_after.permissions().mode(),
        outside.permissions().mode()
    );
    assert_eq!(outside_after.mtime(), outside.mtime());
    assert_eq!(outside_after.mtime_nsec(), outside.mtime_nsec());
    assert_eq!(first.permissions().mode() & 0o7777, 0o600);
    session.complete().await;
}

#[tokio::test]
async fn no_op_has_no_setters_or_link_count_ctime_effects_and_optional_hash() {
    for verify in [false, true] {
        let root = old_group();
        let before = std::fs::metadata(root.path().join("first")).unwrap();
        let mut request = open(false, true, 0o640);
        request.verify_bytes = verify;
        let mut session = Session::start(root, request, &["first", "second"], false).await;
        assert_eq!(
            session.prepare().await,
            WireGroupResult::DestinationReady {
                publish: false,
                size: 3,
                digest: verify.then(|| *blake3::hash(b"OLD").as_bytes()),
                literal_bytes: 0,
                reused_bytes: 0,
            }
        );
        session.publish(0).await;
        session.publish(1).await;
        session.complete().await;
        let after = std::fs::metadata(session.root.path().join("first")).unwrap();
        assert_eq!(
            (
                before.ino(),
                before.nlink(),
                before.ctime(),
                before.ctime_nsec()
            ),
            (
                after.ino(),
                after.nlink(),
                after.ctime(),
                after.ctime_nsec()
            )
        );
    }
}

#[tokio::test]
async fn later_failure_or_abort_never_rolls_back_published_names() {
    for abort in [false, true] {
        let mut session = Session::start(
            old_group(),
            open(true, false, 0o600),
            &["first", "second"],
            false,
        )
        .await;
        session.prepare().await;
        session.publish(0).await;
        if abort {
            session.step(WireGroupStep::Abort).await;
        } else {
            std::fs::remove_file(session.root.path().join("second")).unwrap();
            std::fs::write(session.root.path().join("second"), b"FOREIGN").unwrap();
            session.step(WireGroupStep::Publish(1)).await;
        }
        let root_path = session.root.path().to_path_buf();
        // Keep TempDir alive while consuming the server result.
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), &mut session.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error, GroupError::PartialPublication { published: 1, .. }),
            "{error}"
        );
        assert_eq!(std::fs::read(root_path.join("first")).unwrap(), CANONICAL);
        assert_eq!(
            std::fs::metadata(root_path.join("first")).unwrap().nlink(),
            1
        );
        assert_eq!(
            std::fs::read(root_path.join("second")).unwrap(),
            if abort { b"OLD" as &[u8] } else { b"FOREIGN" }
        );
        assert_eq!(std::fs::read(root_path.join("outside")).unwrap(), b"OLD");
        assert_eq!(std::fs::read_dir(&root_path).unwrap().count(), 3);
    }
}

#[tokio::test]
async fn original_stage_survives_relocation_of_held_remote_root() {
    let mut session = Session::start(
        old_group(),
        open(true, false, 0o600),
        &["first", "second"],
        false,
    )
    .await;
    session.prepare().await;
    let container = tempfile::tempdir().unwrap();
    let moved = container.path().join("held");
    std::fs::rename(session.root.path(), &moved).unwrap();
    std::fs::create_dir(session.root.path()).unwrap();
    std::fs::write(session.root.path().join("first"), b"FOREIGN").unwrap();
    session.publish(0).await;
    session.publish(1).await;
    assert_eq!(std::fs::read(moved.join("first")).unwrap(), CANONICAL);
    assert_eq!(
        std::fs::read(session.root.path().join("first")).unwrap(),
        b"FOREIGN"
    );
    session.complete().await;
}

#[tokio::test]
async fn captured_metadata_applies_at_file_end_and_rejects_late_duplicates() {
    for duplicate in [false, true] {
        let mut request = open(true, true, 0o600);
        request.xattrs = true;
        request.acls = cfg!(feature = "acl");
        let root = old_group();
        xattr::set(root.path().join("first"), "user.retained", b"old metadata").unwrap();
        let mut session = Session::start(root, request, &["first", "second"], false).await;
        // A complete mirror must include native required attributes (notably
        // SELinux labels), just as a real original-source FD capture does.
        let source_root = tempfile::tempdir().unwrap();
        std::fs::write(source_root.path().join("source"), CANONICAL).unwrap();
        xattr::set(
            source_root.path().join("source"),
            "user.sy-group",
            b"source",
        )
        .unwrap();
        let source = RootedFs::open(source_root.path().to_path_buf())
            .await
            .unwrap();
        let relative = RelativePath::new("source").unwrap();
        let file = source.open_regular_blocking(&relative).unwrap();
        let attrs = WireXattrResult::new(
            source
                .read_open_file_xattrs_blocking(&file, &relative)
                .unwrap()
                .into_iter()
                .map(|(name, value)| {
                    WireXattr::new(crate::remote::xattr::name_bytes(&name), Bytes::from(value))
                        .unwrap()
                })
                .collect(),
        )
        .unwrap();
        session
            .send(FrameKind::FileXattrs, attrs.encode().unwrap())
            .await;
        if session.open.acls {
            session
                .send(
                    FrameKind::FileAcls,
                    WireAcl::new(String::new()).unwrap().encode(),
                )
                .await;
        }
        if duplicate {
            session.step(WireGroupStep::Prepare).await;
            session
                .send(FrameKind::FileBegin, session.open.file.encode())
                .await;
            session
                .send(FrameKind::FileXattrs, attrs.encode().unwrap())
                .await;
            let root_path = session.root.path().to_path_buf();
            let error =
                tokio::time::timeout(std::time::Duration::from_secs(10), &mut session.server)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
            assert!(matches!(error, GroupError::Transfer(_)), "{error}");
            assert_eq!(std::fs::read(root_path.join("first")).unwrap(), b"OLD");
            assert_eq!(std::fs::read_dir(root_path).unwrap().count(), 3);
        } else {
            session.prepare().await;
            session.publish(0).await;
            session.publish(1).await;
            assert_eq!(
                xattr::get(session.root.path().join("first"), "user.sy-group")
                    .unwrap()
                    .unwrap(),
                b"source"
            );
            assert!(
                xattr::get(session.root.path().join("outside"), "user.sy-group")
                    .unwrap()
                    .is_none()
            );
            assert!(
                xattr::get(session.root.path().join("first"), "user.retained")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                xattr::get(session.root.path().join("outside"), "user.retained")
                    .unwrap()
                    .unwrap(),
                b"old metadata"
            );
            session.complete().await;
        }
    }
}

#[tokio::test]
async fn abort_before_publication_removes_only_private_staging() {
    let mut session = Session::start(
        old_group(),
        open(true, true, 0o600),
        &["first", "second"],
        false,
    )
    .await;
    session.prepare().await;
    session.step(WireGroupStep::Abort).await;
    assert_eq!(session.reply().await, WireGroupResult::Aborted);
    session.server.await.unwrap().unwrap();
    assert_eq!(
        std::fs::read(session.root.path().join("first")).unwrap(),
        b"OLD"
    );
    assert_eq!(std::fs::read_dir(session.root.path()).unwrap().count(), 3);
}

#[tokio::test]
async fn retained_selection_requires_native_coalescence_and_no_op_final_proof() {
    let root = old_group();
    std::fs::remove_file(root.path().join("second")).unwrap();
    std::fs::write(root.path().join("second"), b"OLD").unwrap();
    let session = Session::start(root, open(false, true, 0o640), &["first", "second"], false).await;
    session.step(WireGroupStep::Prepare).await;
    assert!(matches!(session.failure().await, GroupError::Phase(_)));

    let mut session = Session::start(
        old_group(),
        open(false, true, 0o640),
        &["first", "second"],
        false,
    )
    .await;
    session.prepare().await;
    session.publish(0).await;
    session.publish(1).await;
    std::fs::write(session.root.path().join("outside"), b"changed OLD").unwrap();
    session.step(WireGroupStep::Finish(session.seal)).await;
    assert!(matches!(session.failure().await, GroupError::Rooted(_)));
}

#[tokio::test]
async fn canonical_reconstruction_uses_declared_target_not_optional_copy_basis() {
    for kind in ["absent", "symlink", "directory", "single-link"] {
        let root = old_group();
        std::fs::remove_file(root.path().join("first")).unwrap();
        match kind {
            "symlink" => std::os::unix::fs::symlink("outside", root.path().join("first")).unwrap(),
            "directory" => std::fs::create_dir(root.path().join("first")).unwrap(),
            "single-link" => std::fs::write(root.path().join("first"), b"single OLD").unwrap(),
            _ => {}
        }
        let mut session =
            Session::start(root, open(true, false, 0o600), &["first", "second"], false).await;
        session.prepare().await;
        session.publish(0).await;
        session.publish(1).await;
        session.complete().await;
        assert_eq!(
            std::fs::read(session.root.path().join("first")).unwrap(),
            CANONICAL
        );
        assert_eq!(
            std::fs::read(session.root.path().join("outside")).unwrap(),
            b"OLD"
        );
    }
}

#[tokio::test]
async fn copy_basis_reconstruction_is_private_and_counted_once() {
    let mut session = Session::start(
        old_group(),
        open(true, true, 0o600),
        &["first", "second"],
        true,
    )
    .await;
    session.step(WireGroupStep::Prepare).await;
    session
        .send(FrameKind::FileBegin, session.open.file.encode())
        .await;
    session
        .send(
            FrameKind::DeltaCopy,
            crate::protocol::WireDeltaCopy::new(0, 3).unwrap().encode(),
        )
        .await;
    session
        .send(FrameKind::Data, Bytes::from_static(b"suffix"))
        .await;
    let digest = *blake3::hash(b"OLDsuffix").as_bytes();
    session
        .send_flags(
            FrameKind::FileEnd,
            FrameFlags::ACK_REQUIRED,
            WireFileEnd::new(9, digest).encode(),
        )
        .await;
    assert_eq!(
        session.reply().await,
        WireGroupResult::DestinationReady {
            publish: true,
            size: 9,
            digest: Some(digest),
            literal_bytes: 6,
            reused_bytes: 3,
        }
    );
    assert_eq!(
        std::fs::read(session.root.path().join("first")).unwrap(),
        b"OLD"
    );
    session.publish(0).await;
    session.publish(1).await;
    session.complete().await;
    assert_eq!(
        std::fs::read(session.root.path().join("second")).unwrap(),
        b"OLDsuffix"
    );
    assert_eq!(
        std::fs::read(session.root.path().join("outside")).unwrap(),
        b"OLD"
    );
}

#[tokio::test]
async fn invalid_byte_phase_or_changed_original_observation_never_publishes() {
    for defect in ["begin", "digest", "end-flags", "prepare-observation"] {
        let mut session = Session::start(
            old_group(),
            open(true, false, 0o600),
            &["first", "second"],
            false,
        )
        .await;
        if defect == "prepare-observation" {
            std::fs::remove_file(session.root.path().join("second")).unwrap();
            std::fs::write(session.root.path().join("second"), b"FOREIGN").unwrap();
        }
        session.step(WireGroupStep::Prepare).await;
        if defect != "prepare-observation" {
            let begin = if defect == "begin" {
                WireFileBegin::whole(path("first"), 9)
                    .with_metadata(Some(0o644), None)
                    .unwrap()
            } else {
                session.open.file.clone()
            };
            session.send(FrameKind::FileBegin, begin.encode()).await;
            if defect != "begin" {
                session
                    .send(FrameKind::Data, Bytes::from_static(CANONICAL))
                    .await;
                let digest = if defect == "digest" {
                    [0; 32]
                } else {
                    *blake3::hash(CANONICAL).as_bytes()
                };
                let flags = if defect == "end-flags" {
                    FrameFlags::FINAL | FrameFlags::ACK_REQUIRED
                } else {
                    FrameFlags::ACK_REQUIRED
                };
                session
                    .send_flags(
                        FrameKind::FileEnd,
                        flags,
                        WireFileEnd::new(9, digest).encode(),
                    )
                    .await;
            }
        }
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), &mut session.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            !matches!(error, GroupError::PartialPublication { .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read(session.root.path().join("first")).unwrap(),
            b"OLD"
        );
        assert_eq!(std::fs::read_dir(session.root.path()).unwrap().count(), 3);
    }
}

#[tokio::test]
async fn wrong_cursor_or_finish_seal_poison_group_and_cleanup_private_anchor() {
    for defect in [
        "out-of-order",
        "premature-finish",
        "wrong-seal",
        "final-binding",
    ] {
        let mut session = Session::start(
            old_group(),
            open(true, false, 0o600),
            &["first", "second"],
            false,
        )
        .await;
        session.prepare().await;
        if defect != "out-of-order" {
            session.publish(0).await;
        }
        let step = match defect {
            "out-of-order" => WireGroupStep::Publish(1),
            "premature-finish" => WireGroupStep::Finish(session.seal),
            "wrong-seal" => WireGroupStep::Finish(WireGroupSeal {
                count: 2,
                digest: [0; 32],
            }),
            _ => {
                session.publish(1).await;
                std::fs::remove_file(session.root.path().join("second")).unwrap();
                std::fs::write(session.root.path().join("second"), b"FOREIGN").unwrap();
                WireGroupStep::Finish(session.seal)
            }
        };
        session.step(step).await;
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), &mut session.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        if defect == "out-of-order" {
            assert!(matches!(error, GroupError::Phase(_)), "{error}");
            assert_eq!(
                std::fs::read(session.root.path().join("first")).unwrap(),
                b"OLD"
            );
        } else {
            assert!(
                matches!(error, GroupError::PartialPublication { .. }),
                "{error}"
            );
            assert_eq!(
                std::fs::read(session.root.path().join("first")).unwrap(),
                CANONICAL
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn retained_flags_finalize_after_anchor_retirement_and_source_flags_override() {
    use std::os::macos::fs::MetadataExt as _;
    for source_override in [false, true] {
        let root = old_group();
        // Configure fixture metadata before sharing the inode: the production
        // in-place setter correctly refuses changes to outside aliases.
        std::fs::remove_file(root.path().join("second")).unwrap();
        std::fs::remove_file(root.path().join("outside")).unwrap();
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        tokio::task::spawn_blocking(move || {
            rooted.write_bsd_flags_blocking(
                &decode_relative_path(path("first"), Platform::current().os).unwrap(),
                EntryKind::File,
                libc::UF_NODUMP,
            )
        })
        .await
        .unwrap()
        .unwrap();
        std::fs::hard_link(root.path().join("first"), root.path().join("second")).unwrap();
        std::fs::hard_link(root.path().join("first"), root.path().join("outside")).unwrap();
        let mut request = open(true, true, 0o600);
        if source_override {
            request.source_flags = true;
            request.file = request.file.with_bsd_flags(Some(0));
        }
        let mut session = Session::start(root, request, &["first", "second"], false).await;
        session.prepare().await;
        session.publish(0).await;
        session.publish(1).await;
        assert_eq!(
            std::fs::metadata(session.root.path().join("first"))
                .unwrap()
                .st_flags(),
            0
        );
        session.complete().await;
        let final_metadata = std::fs::metadata(session.root.path().join("first")).unwrap();
        assert_eq!(final_metadata.nlink(), 2);
        assert_eq!(
            final_metadata.st_flags(),
            if source_override { 0 } else { libc::UF_NODUMP }
        );
        assert_eq!(
            std::fs::metadata(session.root.path().join("outside"))
                .unwrap()
                .st_flags(),
            libc::UF_NODUMP
        );
    }
}
