//! Cutoff tests at the namespace/inode authority, using a negotiated session's
//! actual root binding rather than a separate cancellation snapshot.
use super::*;
use crate::protocol::Operation;
use crate::remote::router::{RouterConfig, RouterError};
use crate::remote::runtime::{ClientRemoteSession, ServerRemoteSession};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Directory,
    Ancestors,
    SymlinkNew,
    SymlinkReplace,
    SymlinkExchange,
    HardlinkPrepare,
    HardlinkPublish,
    Backup,
    RemoveFile,
    RemoveDirectory,
    FileMetadata,
    SymlinkMetadata,
    DirectoryMetadata,
    DirectoryFinalize,
    Xattrs,
    #[cfg(feature = "acl")]
    Acl,
    #[cfg(target_os = "macos")]
    Flags,
}

const MUTATIONS: &[Mutation] = &[
    Mutation::Directory,
    Mutation::Ancestors,
    Mutation::SymlinkNew,
    Mutation::SymlinkReplace,
    Mutation::SymlinkExchange,
    Mutation::HardlinkPrepare,
    Mutation::HardlinkPublish,
    Mutation::Backup,
    Mutation::RemoveFile,
    Mutation::RemoveDirectory,
    Mutation::FileMetadata,
    Mutation::SymlinkMetadata,
    Mutation::DirectoryMetadata,
    Mutation::DirectoryFinalize,
    Mutation::Xattrs,
    #[cfg(feature = "acl")]
    Mutation::Acl,
    #[cfg(target_os = "macos")]
    Mutation::Flags,
];

fn relative(path: &str) -> RelativePath {
    RelativePath::new(path).unwrap()
}

fn prepare(root: &Path, mutation: Mutation) {
    std::fs::write(root.join("source"), b"new bytes").unwrap();
    match mutation {
        Mutation::Directory | Mutation::Ancestors | Mutation::SymlinkNew => {}
        Mutation::SymlinkExchange
        | Mutation::RemoveDirectory
        | Mutation::DirectoryMetadata
        | Mutation::DirectoryFinalize => {
            std::fs::create_dir(root.join("target")).unwrap();
            // Real directory link counts exceed one; finalization must not use
            // regular-file sharing refusal semantics.
            if matches!(
                mutation,
                Mutation::DirectoryMetadata | Mutation::DirectoryFinalize
            ) {
                std::fs::create_dir(root.join("target/child")).unwrap();
            }
        }
        Mutation::SymlinkMetadata => {
            std::os::unix::fs::symlink("source", root.join("target")).unwrap();
        }
        _ => {
            std::fs::write(root.join("target"), b"old bytes").unwrap();
        }
    }
    if root.join("target").exists() && !matches!(mutation, Mutation::SymlinkMetadata) {
        std::fs::set_permissions(root.join("target"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
}

fn mutate(rooted: &RootedFs, mutation: Mutation) -> Result<()> {
    let target = relative("target");
    let source = relative("source");
    let identity = || {
        rooted
            .path_identity_blocking(&target)
            .map(|value| value.unwrap().1)
    };
    match mutation {
        Mutation::Directory => rooted.create_directory_blocking(&target).map(|_| ()),
        Mutation::Ancestors => rooted.create_directories_blocking(&relative("target/child")),
        Mutation::SymlinkNew | Mutation::SymlinkReplace | Mutation::SymlinkExchange => rooted
            .replace_symlink_blocking(
                &target,
                Path::new("source"),
                ExpectedDestination::SnapshotAtOpen,
                None,
            ),
        Mutation::HardlinkPrepare | Mutation::HardlinkPublish => {
            rooted.create_hardlink_blocking(&source, &target)
        }
        Mutation::Backup => rooted.copy_file_blocking(
            &source,
            &target,
            rooted.path_identity_blocking(&source)?.unwrap().1,
        ),
        Mutation::RemoveFile | Mutation::RemoveDirectory => rooted.remove_destination_blocking(
            &target,
            matches!(mutation, Mutation::RemoveDirectory),
            Some(identity()?),
        ),
        Mutation::FileMetadata => rooted.apply_metadata_blocking(
            &target,
            EntryKind::File,
            identity()?,
            Some(0o600),
            Some(Timestamp::new(1_600_000_001, 0).unwrap()),
        ),
        Mutation::SymlinkMetadata => rooted.apply_metadata_blocking(
            &target,
            EntryKind::Symlink,
            identity()?,
            None,
            Some(Timestamp::new(1_600_000_001, 0).unwrap()),
        ),
        Mutation::DirectoryMetadata => rooted.apply_metadata_blocking(
            &target,
            EntryKind::Directory,
            identity()?,
            Some(0o700),
            Some(Timestamp::new(1_600_000_001, 0).unwrap()),
        ),
        Mutation::DirectoryFinalize => rooted.finalize_directory_blocking(
            &target,
            identity()?,
            Some(0o700),
            Some(Timestamp::new(1_600_000_001, 0).unwrap()),
            &DirectoryPreservation {
                xattrs: Some(vec![(OsString::from("user.sy-admission"), b"new".to_vec())]),
                ..Default::default()
            },
        ),
        Mutation::Xattrs => rooted.write_xattrs_blocking(
            &target,
            EntryKind::File,
            &[(OsString::from("user.sy-admission"), b"new".to_vec())],
        ),
        #[cfg(feature = "acl")]
        Mutation::Acl => {
            let mut entries = exacl::getfacl(rooted.root_path().join("target"), None)?;
            entries.push(exacl::AclEntry::allow_user(
                "1",
                exacl::Perm::READ,
                exacl::Flag::empty(),
            ));
            // Only fixture construction uses exacl's ordinary path API; the
            // operation under test must mutate through its held descriptor.
            rooted.write_acl_blocking(&target, EntryKind::File, &exacl::to_string(&entries)?)
        }
        #[cfg(target_os = "macos")]
        Mutation::Flags => {
            rooted.write_bsd_flags_blocking(&target, EntryKind::File, libc::UF_NODUMP)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    identity: Option<(EntryKind, EntryIdentity)>,
    contents: Option<Vec<u8>>,
    target: Option<PathBuf>,
    modified: Option<(i64, i64)>,
    xattrs: Vec<(OsString, Vec<u8>)>,
}

fn observe(rooted: &RootedFs) -> Result<Observation> {
    let identity = rooted.path_identity_blocking(&relative("target"))?;
    let contents = if identity.is_some_and(|(kind, _)| kind == EntryKind::File) {
        Some(std::fs::read(rooted.root_path().join("target"))?)
    } else {
        None
    };
    let target = if identity.is_some_and(|(kind, _)| kind == EntryKind::Symlink) {
        Some(std::fs::read_link(rooted.root_path().join("target"))?)
    } else {
        None
    };
    let modified = if identity.is_some() {
        let stat = std::fs::symlink_metadata(rooted.root_path().join("target"))?;
        Some((stat.mtime(), stat.mtime_nsec()))
    } else {
        None
    };
    let mut xattrs = match identity {
        Some((kind @ (EntryKind::File | EntryKind::Directory), _)) => {
            // Inspect visible state independently of the paused transaction's
            // retirement lock. This is a test observer, not scan authority or
            // a preservation request waiting for a consistent owned version.
            let file = rooted.open_xattr_entry_blocking(Path::new("target"), kind)?;
            read_xattrs_from_file(&file)?
        }
        _ => Vec::new(),
    };
    xattrs.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(Observation {
        identity,
        contents,
        target,
        modified,
        xattrs,
    })
}

async fn session(root: &Path, operation: Operation) -> (ClientRemoteSession, ServerRemoteSession) {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (server_reader, server_writer) = tokio::io::split(server_io);
    let (client, server) = tokio::join!(
        ClientRemoteSession::connect(
            client_reader,
            client_writer,
            operation,
            root,
            RouterConfig::default()
        ),
        ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default()),
    );
    (client.unwrap(), server.unwrap())
}

#[tokio::test]
async fn session_cutoff_fences_rooted_mutations_and_cleans_owned_staging() {
    for &mutation in MUTATIONS {
        for (eof, admitted) in [(true, false), (false, false), (false, true)] {
            // Hardlink preparation admission alone does not authorize its
            // publication. That is exercised separately at HardlinkPublish.
            if admitted && matches!(mutation, Mutation::HardlinkPrepare) {
                continue;
            }
            let root = tempfile::tempdir().unwrap();
            prepare(root.path(), mutation);
            let (client, mut server) = session(root.path(), Operation::Push).await;
            let rooted = server.scan_handler_rooted().unwrap();
            let before = observe(&rooted).unwrap();
            let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            rooted.pause_mutation_at(
                usize::from(matches!(mutation, Mutation::HardlinkPublish)),
                PublicationPause {
                    point: if admitted {
                        PublicationPausePoint::AfterAdmission
                    } else {
                        PublicationPausePoint::BeforeAdmission
                    },
                    reached: reached_tx,
                    resume: resume_rx,
                },
            );
            let worker_root = rooted.clone();
            let worker = tokio::task::spawn_blocking(move || mutate(&worker_root, mutation));
            let paused = tokio::time::timeout(Duration::from_secs(5), reached_rx).await;
            let sender = server.sender();
            let terminal = tokio::time::timeout(Duration::from_secs(5), async {
                if eof {
                    drop(client);
                } else {
                    sender.fail(Arc::new(RouterError::WriterClosed));
                }
                sender.closed().await
            })
            .await;
            let still_private = observe(&rooted).is_ok_and(|observed| observed == before);
            let still_owned = !worker.is_finished();
            // Release every native gate before assertions, including failed
            // reach/terminal waits. Native pauses also have their own deadline.
            let _ = resume_tx.send(());
            let result = worker.await.unwrap();
            server.shutdown().await.unwrap();

            assert!(
                matches!(paused, Ok(Ok(()))),
                "{mutation:?}: never reached gate"
            );
            assert!(
                terminal.is_ok(),
                "{mutation:?}: terminal actor blocked on native I/O"
            );
            assert!(still_private, "{mutation:?}: mutated before native gate");
            assert!(still_owned, "{mutation:?}: lost native worker ownership");
            if admitted {
                if matches!(mutation, Mutation::Ancestors) {
                    // Each mkdir has its own admission. The first may finish;
                    // closure forbids creating the next descendant.
                    assert!(matches!(result, Err(RootedFsError::CommitCancelled)));
                    assert!(root.path().join("target").is_dir());
                    assert!(!root.path().join("target/child").exists());
                } else {
                    assert!(
                        result.is_ok(),
                        "{mutation:?}: admitted work failed: {result:?}"
                    );
                }
                assert_ne!(
                    observe(&rooted).unwrap(),
                    before,
                    "{mutation:?}: no native effect"
                );
            } else {
                assert!(
                    matches!(result, Err(RootedFsError::CommitCancelled)),
                    "{mutation:?}: {result:?}"
                );
                assert_eq!(observe(&rooted).unwrap(), before, "{mutation:?}");
                assert_eq!(
                    std::fs::metadata(root.path().join("source"))
                        .unwrap()
                        .nlink(),
                    1
                );
            }
            assert!(
                std::fs::read_dir(root.path()).unwrap().all(|entry| {
                    let name = entry.unwrap().file_name();
                    name == "source" || name == "target"
                }),
                "{mutation:?}: staging leaked"
            );
            assert_eq!(
                std::fs::read(root.path().join("source")).unwrap(),
                b"new bytes"
            );
        }
    }
}

#[tokio::test]
async fn pull_root_refuses_native_mutations_and_private_staging() {
    for &mutation in MUTATIONS {
        if matches!(mutation, Mutation::HardlinkPublish) {
            continue; // Same native entry point as HardlinkPrepare here.
        }
        let root = tempfile::tempdir().unwrap();
        prepare(root.path(), mutation);
        let (_client, mut server) = session(root.path(), Operation::Pull).await;
        let rooted = server.scan_handler_rooted().unwrap();
        let before = observe(&rooted).unwrap();
        let worker_root = rooted.clone();
        let result = tokio::task::spawn_blocking(move || mutate(&worker_root, mutation))
            .await
            .unwrap();
        assert!(
            matches!(result, Err(RootedFsError::ReadOnlyRoot)),
            "{mutation:?}: {result:?}"
        );
        assert_eq!(observe(&rooted).unwrap(), before);
        if matches!(mutation, Mutation::Directory) {
            assert!(matches!(
                rooted.begin_staged_file_blocking(&relative("new")),
                Err(RootedFsError::ReadOnlyRoot)
            ));
        }
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            name == "source" || name == "target"
        }));
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn hardlink_existing_member_is_noop_and_directory_transition_stays_unsupported() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source"), b"keep").unwrap();
    std::fs::hard_link(root.path().join("source"), root.path().join("target")).unwrap();
    std::fs::create_dir(root.path().join("directory")).unwrap();
    let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
    let before = observe(&rooted).unwrap();
    rooted
        .create_hardlink_blocking(&relative("source"), &relative("target"))
        .unwrap();
    assert_eq!(observe(&rooted).unwrap(), before);
    assert!(rooted
        .create_hardlink_blocking(&relative("source"), &relative("directory"))
        .is_err());
    assert!(root.path().join("directory").is_dir());
}
