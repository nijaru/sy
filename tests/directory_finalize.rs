#![cfg(unix)]

use futures::TryStreamExt;
use std::num::NonZeroUsize;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use sy::endpoint::local_entry_scan::local_entry_stream;
use sy::engine::controller::{preflight_sync, SyncController, SyncPlanExecutor};
use sy::engine::delete_plan::{DeleteAction, DeleteLimit, DeletePolicy};
use sy::engine::domain::{Entry, SyncOp};
use sy::engine::finalize_journal::FinalizeMetadata;
use sy::engine::planner::{ComparisonPolicy, ExecutionPolicy};
use sy::engine::scan::ScanRequest;
use sy::engine::scheduler::{ResourceBudget, Scheduler};
use sy::engine::work::{WorkItem, WorkResult};
use sy::protocol::Operation;
use sy::remote::local_executor::LocalSyncExecutor;
use sy::remote::pull::RemotePullExecutor;
use sy::remote::push::RemotePushExecutor;
use sy::remote::router::RouterConfig;
use sy::remote::runtime::ClientRemoteSession;
use sy::transfer::delta::BasisIndexLimits;

#[derive(Clone, Copy, Debug)]
enum Direction {
    Local,
    Push,
    Pull,
}
#[derive(Clone, Copy, Debug)]
enum Mutation {
    None,
    SourceDirectorySwap,
    DestinationDirectorySwap,
    DestinationSymlink,
}

struct BeforeFinalize<E> {
    inner: E,
    source: PathBuf,
    destination: PathBuf,
    outside: PathBuf,
    mutation: Mutation,
}
impl<E: SyncPlanExecutor> SyncPlanExecutor for BeforeFinalize<E> {
    type Action = E::Action;
    type Error = E::Error;
    fn lower(
        &self,
        op: SyncOp,
        policy: ExecutionPolicy,
    ) -> Result<Option<WorkItem<E::Action>>, E::Error> {
        self.inner.lower(op, policy)
    }
    fn is_directory_action(&self, action: &E::Action) -> bool {
        self.inner.is_directory_action(action)
    }
    async fn execute(&self, item: WorkItem<E::Action>) -> Result<WorkResult, E::Error> {
        self.inner.execute(item).await
    }
    async fn execute_delete(&self, action: DeleteAction) -> Result<(), E::Error> {
        self.inner.execute_delete(action).await
    }
    async fn execute_finalize(&self, metadata: FinalizeMetadata) -> Result<(), E::Error> {
        if metadata.path.as_path() == Path::new("dir") {
            let root = if matches!(self.mutation, Mutation::SourceDirectorySwap) {
                &self.source
            } else {
                &self.destination
            };
            if !matches!(self.mutation, Mutation::None) {
                std::fs::rename(root.join("dir"), root.join("held-dir")).unwrap();
                if matches!(self.mutation, Mutation::DestinationSymlink) {
                    std::os::unix::fs::symlink(&self.outside, root.join("dir")).unwrap();
                } else {
                    std::fs::create_dir(root.join("dir")).unwrap();
                    std::fs::set_permissions(
                        root.join("dir"),
                        std::fs::Permissions::from_mode(0o755),
                    )
                    .unwrap();
                }
            }
        }
        self.inner.execute_finalize(metadata).await
    }
    async fn finish_deferred_source_removals(&self) -> Result<(), E::Error> {
        self.inner.finish_deferred_source_removals().await
    }
    async fn remove_unchanged_source(
        &self,
        source: &Entry,
        destination: &Entry,
        policy: ExecutionPolicy,
    ) -> Result<(), E::Error> {
        self.inner
            .remove_unchanged_source(source, destination, policy)
            .await
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}
fn xattr_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "user.sy-directory"
    } else {
        "sy-directory"
    }
}

async fn run_case(
    direction: Direction,
    existing: bool,
    mutation: Mutation,
    policy: ComparisonPolicy,
    acl: bool,
    flags: bool,
) {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(source.path().join("dir")).unwrap();
    std::fs::write(source.path().join("dir/child"), b"child content").unwrap();
    std::fs::set_permissions(
        source.path().join("dir"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    filetime::set_file_mtime(
        source.path().join("dir"),
        filetime::FileTime::from_unix_time(1_600_000_000, 123),
    )
    .unwrap();
    xattr::set(source.path().join("dir"), xattr_name(), b"source-directory").unwrap();
    if existing {
        std::fs::create_dir(destination.path().join("dir")).unwrap();
        std::fs::write(destination.path().join("dir/obsolete"), b"delete me").unwrap();
        std::fs::set_permissions(
            destination.path().join("dir"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        filetime::set_file_mtime(
            destination.path().join("dir"),
            filetime::FileTime::from_unix_time(1_700_000_000, 456),
        )
        .unwrap();
        xattr::set(
            destination.path().join("dir"),
            xattr_name(),
            b"destination-directory",
        )
        .unwrap();
    }
    std::fs::write(outside.path().join("sentinel"), b"outside untouched").unwrap();
    let outside_mode = mode(outside.path());
    #[cfg(all(target_os = "linux", feature = "acl"))]
    if acl {
        let dir = source.path().join("dir");
        assert!(std::process::Command::new("setfacl")
            .args([
                "-m",
                "u:12345:rwx,m::r-x,d:u::rwx,d:g::r-x,d:o::---,d:u:12345:rwx,d:m::r-x"
            ])
            .arg(&dir)
            .status()
            .unwrap()
            .success());
        // New destination directories inherit a different ACL. Final replay
        // must replace it, including defaults, without losing the mode mask.
        let target = if existing {
            destination.path().join("dir")
        } else {
            destination.path().to_path_buf()
        };
        assert!(std::process::Command::new("setfacl")
            .args(["-m", "d:u::rwx,d:g::rwx,d:o::---,d:u:12346:rwx,d:m::rwx"])
            .arg(target)
            .status()
            .unwrap()
            .success());
    }
    #[cfg(target_os = "macos")]
    if flags {
        // SAFETY: path is a live NUL-terminated directory name.
        let path = std::ffi::CString::new(source.path().join("dir").as_os_str().as_encoded_bytes())
            .unwrap();
        assert_eq!(
            unsafe { libc::chflags(path.as_ptr(), libc::UF_IMMUTABLE) },
            0
        );
    }
    let mut request = ScanRequest::default();
    request.metadata.unix_mode = true;
    let source_entries = local_entry_stream(source.path().to_path_buf(), request)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let destination_entries = local_entry_stream(destination.path().to_path_buf(), request)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let stream = |entries: Vec<Entry>| -> sy::engine::reconcile::EntryStream {
        sy::engine::reconcile::EntryStream::new(futures::stream::iter(entries.into_iter().map(Ok)))
    };
    let plan = preflight_sync(
        stream(source_entries),
        stream(destination_entries),
        policy,
        Some(DeletePolicy {
            limit: DeleteLimit::Percentage(100),
            force: false,
        }),
        |_| true,
    )
    .await
    .unwrap();
    let scheduler = Scheduler::new(ResourceBudget::default()).unwrap();
    let wrap = |inner| BeforeFinalize {
        inner,
        source: source.path().to_path_buf(),
        destination: destination.path().to_path_buf(),
        outside: outside.path().to_path_buf(),
        mutation,
    };
    let result = match direction {
        Direction::Local => {
            SyncController::new(
                wrap(
                    LocalSyncExecutor::new(
                        source.path().to_path_buf(),
                        destination.path().to_path_buf(),
                        scheduler,
                    )
                    .with_xattrs(true)
                    .with_acls(acl)
                    .with_bsd_flags(flags),
                ),
                NonZeroUsize::new(2).unwrap(),
            )
            .execute(plan)
            .await
        }
        Direction::Push | Direction::Pull => {
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (cr, cw) = tokio::io::split(client_io);
            let (sr, sw) = tokio::io::split(server_io);
            let server = tokio::spawn(sy::remote::serve::serve_transport(
                sr,
                sw,
                RouterConfig::default(),
            ));
            let (operation, root) = if matches!(direction, Direction::Push) {
                (Operation::Push, destination.path())
            } else {
                (Operation::Pull, source.path())
            };
            let client =
                ClientRemoteSession::connect(cr, cw, operation, root, RouterConfig::default())
                    .await
                    .unwrap();
            let result = if matches!(direction, Direction::Push) {
                let executor = RemotePushExecutor::new(
                    source.path().to_path_buf(),
                    client.request_handle(),
                    scheduler,
                    BasisIndexLimits::default(),
                )
                .with_xattrs(true)
                .with_acls(acl)
                .with_bsd_flags(flags);
                SyncController::new(
                    BeforeFinalize {
                        inner: executor,
                        source: source.path().to_path_buf(),
                        destination: destination.path().to_path_buf(),
                        outside: outside.path().to_path_buf(),
                        mutation,
                    },
                    NonZeroUsize::new(2).unwrap(),
                )
                .execute(plan)
                .await
            } else {
                let executor = RemotePullExecutor::new(
                    destination.path().to_path_buf(),
                    client.request_handle(),
                    client.sender(),
                    scheduler,
                )
                .with_xattrs(true)
                .with_acls(acl)
                .with_bsd_flags(flags);
                SyncController::new(
                    BeforeFinalize {
                        inner: executor,
                        source: source.path().to_path_buf(),
                        destination: destination.path().to_path_buf(),
                        outside: outside.path().to_path_buf(),
                        mutation,
                    },
                    NonZeroUsize::new(2).unwrap(),
                )
                .execute(plan)
                .await
            };
            server.abort();
            let _ = server.await;
            result
        }
    };
    assert_eq!(
        result.is_ok(),
        matches!(mutation, Mutation::None),
        "{direction:?}/{existing}/{mutation:?}: {result:?}"
    );
    assert_eq!(mode(outside.path()), outside_mode);
    assert_eq!(
        std::fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside untouched"
    );
    let published = if matches!(
        mutation,
        Mutation::DestinationDirectorySwap | Mutation::DestinationSymlink
    ) {
        destination.path().join("held-dir")
    } else {
        destination.path().join("dir")
    };
    assert_eq!(
        std::fs::read(published.join("child")).unwrap(),
        b"child content"
    );
    assert!(!published.join("obsolete").exists());
    if matches!(mutation, Mutation::None) {
        let skipped = existing && (policy.ignore_existing || policy.update_only);
        assert_eq!(
            mode(&published),
            if policy.preserve_permissions {
                if skipped {
                    0o755
                } else {
                    0o750
                }
            } else {
                0o755
            }
        );
        assert_eq!(
            xattr::get(&published, xattr_name()).unwrap().unwrap(),
            if skipped {
                b"destination-directory".as_slice()
            } else {
                b"source-directory".as_slice()
            }
        );
        if policy.preserve_times {
            let modified = filetime::FileTime::from_last_modification_time(
                &std::fs::metadata(&published).unwrap(),
            );
            assert_eq!(
                (modified.unix_seconds(), modified.nanoseconds()),
                if skipped {
                    (1_700_000_000, 456)
                } else {
                    (1_600_000_000, 123)
                }
            );
        }
        #[cfg(target_os = "macos")]
        if flags {
            use std::os::macos::fs::MetadataExt;
            assert_ne!(
                std::fs::metadata(&published).unwrap().st_flags() & libc::UF_IMMUTABLE,
                0
            );
        }
        #[cfg(all(target_os = "linux", feature = "acl"))]
        if acl {
            let entries = exacl::getfacl(&published, None).unwrap();
            assert!(entries
                .iter()
                .any(|entry| entry.flags.contains(exacl::Flag::DEFAULT)));
            assert!(entries.iter().any(|entry| entry.name == "12345"));
            assert!(!entries.iter().any(|entry| entry.name == "12346"));
            let mask = entries
                .iter()
                .find(|entry| {
                    entry.kind == exacl::AclEntryKind::Mask
                        && !entry.flags.contains(exacl::Flag::DEFAULT)
                })
                .unwrap();
            assert_eq!(mask.perms, exacl::Perm::READ | exacl::Perm::EXECUTE);
        }
    } else {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("committed directory finalization"));
        if matches!(mutation, Mutation::DestinationDirectorySwap) {
            assert_eq!(mode(&destination.path().join("dir")), 0o755);
        }
    }
    #[cfg(target_os = "macos")]
    if flags {
        for path in [source.path().join("dir"), published] {
            let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: path is live and NUL-terminated; clear fixture flags for cleanup.
            assert_eq!(unsafe { libc::chflags(path.as_ptr(), 0) }, 0);
        }
    }
}

#[tokio::test]
async fn directory_finalization_refuses_destination_substitutions() {
    for direction in [Direction::Local, Direction::Push, Direction::Pull] {
        for existing in [false, true] {
            for mutation in [
                Mutation::None,
                Mutation::DestinationDirectorySwap,
                Mutation::DestinationSymlink,
            ] {
                run_case(
                    direction,
                    existing,
                    mutation,
                    ComparisonPolicy {
                        preserve_permissions: true,
                        preserve_times: true,
                        ..Default::default()
                    },
                    false,
                    false,
                )
                .await;
            }
        }
    }
}
#[tokio::test]
async fn source_directory_swap_is_refused_before_required_final_preservation() {
    for direction in [Direction::Local, Direction::Push, Direction::Pull] {
        for existing in [false, true] {
            run_case(
                direction,
                existing,
                Mutation::SourceDirectorySwap,
                ComparisonPolicy {
                    preserve_permissions: true,
                    preserve_times: true,
                    ..Default::default()
                },
                false,
                false,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn existing_directory_optional_preservation_is_not_lost_without_mode_or_time_changes() {
    for direction in [Direction::Local, Direction::Push, Direction::Pull] {
        run_case(
            direction,
            true,
            Mutation::None,
            ComparisonPolicy::default(),
            false,
            false,
        )
        .await;
    }
}
#[tokio::test]
async fn skipped_directory_policy_restores_destination_not_source_metadata() {
    for direction in [Direction::Local, Direction::Push, Direction::Pull] {
        for update in [false, true] {
            run_case(
                direction,
                true,
                Mutation::None,
                ComparisonPolicy {
                    ignore_existing: !update,
                    update_only: update,
                    preserve_permissions: true,
                    preserve_times: true,
                    ..Default::default()
                },
                false,
                false,
            )
            .await;
        }
    }
}
#[cfg(all(target_os = "linux", feature = "acl"))]
#[tokio::test]
async fn directory_default_and_inherited_acls_preserve_requested_mode_mask() {
    for direction in [Direction::Local, Direction::Push, Direction::Pull] {
        for existing in [false, true] {
            run_case(
                direction,
                existing,
                Mutation::None,
                ComparisonPolicy {
                    preserve_permissions: true,
                    ..Default::default()
                },
                true,
                false,
            )
            .await;
        }
    }
}
#[cfg(target_os = "macos")]
#[tokio::test]
async fn immutable_directory_flags_follow_child_work_and_deletion() {
    for direction in [Direction::Local, Direction::Push, Direction::Pull] {
        for existing in [false, true] {
            run_case(
                direction,
                existing,
                Mutation::None,
                ComparisonPolicy {
                    preserve_permissions: true,
                    ..Default::default()
                },
                false,
                true,
            )
            .await;
        }
    }
}
