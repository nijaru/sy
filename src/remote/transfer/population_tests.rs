//! Protection for the new readiness boundary and retained receiver ownership.
use super::population::{prepare_reconstruction_with_stage_blocking, ReconstructionReceiver};
use super::*;
use crate::endpoint::io::PreservationRequest;
use crate::remote::router::{FrameRouter, RouterConfig, RouterRole};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[tokio::test]
async fn supplied_stage_population_retains_creator_and_metadata_until_explicit_commit() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("file");
    std::fs::write(&target, b"old").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    filetime::set_file_mtime(
        &target,
        filetime::FileTime::from_unix_time(1_600_000_040, 0),
    )
    .unwrap();
    xattr::set(&target, "user.sy-retained", b"baseline").unwrap();
    let old = std::fs::metadata(&target).unwrap();
    let expectation = WireFileBasis::new(old.len(), *opened_identity(&old).unwrap().as_bytes());
    let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
    let (prepared, creator_inode) = tokio::task::spawn_blocking(move || {
        let relative = RelativePath::new("file").unwrap();
        let original = rooted.open_regular_blocking(&relative).unwrap();
        let mut staged = rooted
            .begin_staged_file_with_expectation_blocking(
                &relative,
                crate::endpoint::ExpectedDestination::Unchanged(EntryIdentity::from_bytes(
                    expectation.identity(),
                )),
            )
            .unwrap();
        // The parent group owner supplies the retained representative baseline.
        staged
            .copy_retained_metadata_blocking(&original, PreservationRequest::default())
            .unwrap();
        let inode = staged.file_mut().metadata().unwrap().ino();
        let prepared = prepare_reconstruction_with_stage_blocking(
            rooted,
            &relative,
            Some(expectation),
            |_, _, _| Ok(staged),
        )
        .unwrap();
        (prepared, inode)
    })
    .await
    .unwrap();

    let (client_io, server_io) = tokio::io::duplex(4096);
    let (cr, cw) = tokio::io::split(client_io);
    let (sr, sw) = tokio::io::split(server_io);
    let client = FrameRouter::start(cr, cw, RouterRole::Client, RouterConfig::default()).unwrap();
    let mut server =
        FrameRouter::start(sr, sw, RouterRole::Server, RouterConfig::default()).unwrap();
    let sender = client.sender();
    let client_inbox = sender.open_stream().unwrap();
    let id = client_inbox.stream_id();
    // Canonical bytes do not choose metadata policy: the group caller carries
    // the first member's intended mode/time explicitly, since writes change
    // mtime (and can clear set-ID bits) on a retained native baseline.
    let begin = WireFileBegin::delta(
        encode_relative_path(Path::new("file")).unwrap(),
        3,
        expectation,
    )
    .with_metadata(Some(0o640), Some((1_600_000_040, 0)))
    .unwrap();
    sender
        .send(
            Frame::new(
                FrameKind::FileBegin,
                FrameFlags::empty(),
                id,
                begin.encode(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let IncomingStream { first, mut inbox } = server.incoming().recv().await.unwrap().unwrap();
    let (begin, _) =
        decode_reconstruction_begin(first.frame(), id, crate::protocol::Platform::current().os)
            .unwrap();
    drop(first);
    let mut receiver = ReconstructionReceiver::start(prepared, begin);
    sender
        .send(
            Frame::new(
                FrameKind::Data,
                FrameFlags::empty(),
                id,
                Bytes::from_static(b"new"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    // A group ends population, not its stream. Its handler owns this policy.
    sender
        .send(
            Frame::new(
                FrameKind::FileEnd,
                FrameFlags::ACK_REQUIRED,
                id,
                WireFileEnd::new(3, *blake3::hash(b"new").as_bytes()).encode(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let ready = receiver
        .receive(&mut inbox, |frame| {
            if frame.flags() != FrameFlags::ACK_REQUIRED {
                return Err(RemoteTransferError::FrameFlags {
                    kind: frame.kind(),
                    flags: frame.flags().bits(),
                });
            }
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(std::fs::metadata(&target).unwrap().ino(), old.ino());

    let admission = server.sender().publication_admission();
    let summary = tokio::task::spawn_blocking(move || {
        let (mut staged, summary, flags) = ready.into_staged_blocking().unwrap();
        let metadata = staged.file_mut().metadata().unwrap();
        assert_eq!(
            metadata.ino(),
            creator_inode,
            "must not create or reopen another inode"
        );
        assert_eq!(metadata.mode() & 0o7777, 0o640);
        assert_eq!(metadata.mtime(), old.mtime());
        staged
            .commit_with_admission(&admission)
            .unwrap()
            .finalize_blocking(flags)
            .unwrap();
        summary
    })
    .await
    .unwrap();
    assert_eq!(summary.digest, *blake3::hash(b"new").as_bytes());
    assert_eq!(summary.literal_bytes, 3);
    assert_eq!(summary.reused_bytes, 0);
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    assert_eq!(
        xattr::get(&target, "user.sy-retained").unwrap().as_deref(),
        Some(&b"baseline"[..])
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn cancelled_receive_keeps_worker_owned_until_abort_joins_and_cleans_stage() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("file");
    std::fs::write(&target, b"old").unwrap();
    let original = std::fs::metadata(&target).unwrap();
    let expectation = WireFileBasis::new(
        original.len(),
        *opened_identity(&original).unwrap().as_bytes(),
    );
    let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_reconstruction(
            rooted,
            &RelativePath::new("file").unwrap(),
            Some(expectation),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (cr, cw) = tokio::io::split(client_io);
    let (sr, sw) = tokio::io::split(server_io);
    let client = FrameRouter::start(cr, cw, RouterRole::Client, RouterConfig::default()).unwrap();
    let mut server =
        FrameRouter::start(sr, sw, RouterRole::Server, RouterConfig::default()).unwrap();
    let sender = client.sender();
    let client_inbox = sender.open_stream().unwrap();
    let id = client_inbox.stream_id();
    let begin = WireFileBegin::delta(
        encode_relative_path(Path::new("file")).unwrap(),
        3,
        expectation,
    );
    sender
        .send(
            Frame::new(
                FrameKind::FileBegin,
                FrameFlags::empty(),
                id,
                begin.encode(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let IncomingStream { first, mut inbox } = server.incoming().recv().await.unwrap().unwrap();
    drop(first);
    let mut receiver = ReconstructionReceiver::start(prepared, begin);
    {
        let receive = receiver.receive(&mut inbox, require_file_end_flags);
        tokio::pin!(receive);
        assert!(futures::poll!(receive).is_pending());
        // Cancel just the wait, not the lifecycle owner. No FileEnd will arrive.
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), receiver.abort())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(std::fs::metadata(&target).unwrap().ino(), original.ino());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
