use super::*;
use crate::engine::domain::Timestamp;
use crate::engine::namespace::{NamespaceParent, NamespacePreflightError, NamespaceSemantics};
use crate::engine::reconcile::BoxError;
use futures::future::BoxFuture;
use futures::stream;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

struct Qualification {
    root: NamespaceSemantics,
    children: NamespaceSemantics,
    parents: Mutex<Vec<NamespaceParent>>,
    seals: AtomicUsize,
    fail_qualify: bool,
    fail_seal: bool,
    scan_finished: Option<Arc<AtomicBool>>,
}

impl Qualification {
    fn new(root: NamespaceSemantics, children: NamespaceSemantics) -> Self {
        Self {
            root,
            children,
            parents: Mutex::new(Vec::new()),
            seals: AtomicUsize::new(0),
            fail_qualify: false,
            fail_seal: false,
            scan_finished: None,
        }
    }
}

impl NamespaceQualification for Qualification {
    fn qualify<'a>(
        &'a self,
        parent: NamespaceParent,
    ) -> BoxFuture<'a, crate::engine::namespace::Result<NamespaceSemantics>> {
        Box::pin(async move {
            let semantics = match parent {
                NamespaceParent::Root => self.root,
                NamespaceParent::Directory(_) => self.children,
            };
            self.parents.lock().unwrap().push(parent);
            if self.fail_qualify {
                return Err(std::io::Error::other("injected qualification failure").into());
            }
            Ok(semantics)
        })
    }

    fn validate_backup_address(
        &self,
        _path: RelativePath,
    ) -> BoxFuture<'_, crate::engine::namespace::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn seal(&self) -> BoxFuture<'_, crate::engine::namespace::Result<()>> {
        Box::pin(async {
            if let Some(finished) = &self.scan_finished {
                assert!(
                    finished.load(Ordering::Acquire),
                    "sealed before scan joined"
                );
            }
            self.seals.fetch_add(1, Ordering::Relaxed);
            if self.fail_seal {
                return Err(std::io::Error::other("injected seal failure").into());
            }
            Ok(())
        })
    }
}

fn path(value: &str) -> RelativePath {
    RelativePath::new(value).unwrap()
}

fn file(value: &str, size: u64) -> Entry {
    Entry::file(path(value), size, Timestamp::UNIX_EPOCH)
}

fn entries(values: Vec<Entry>) -> EntryStream {
    EntryStream::new(stream::iter(values.into_iter().map(Ok::<_, BoxError>)))
}

#[tokio::test]
async fn unrelated_update_cannot_admit_collision_below_byte_exact_root() {
    let qualification = Qualification::new(
        NamespaceSemantics::BYTE_EXACT,
        NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED,
    );
    // Existing destination-only names must protect unrelated updates too.
    let result = preflight_sync(
        entries(vec![file("z-update", 2)]),
        entries(vec![
            file("child/A", 1),
            file("child/a", 1),
            file("z-update", 1),
        ]),
        ComparisonPolicy::default(),
        None,
        crate::engine::namespace::NamespacePreflight::new(&qualification),
        |_| true,
    )
    .await;
    assert!(matches!(result,
        Err(ControllerError::Namespace(NamespacePreflightError::Collision(collision)))
            if collision.existing == path("child/A") && collision.colliding == path("child/a")));
    assert_eq!(qualification.seals.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn excluded_matched_source_cannot_hide_kept_destination_alias() {
    let qualification = Qualification::new(
        NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED,
        NamespaceSemantics::BYTE_EXACT,
    );
    let result = preflight_sync_scoped(
        entries(vec![file("FILE", 2), file("file", 0)]),
        entries(vec![file("file", 1)]),
        ComparisonPolicy::default(),
        None,
        crate::engine::namespace::NamespacePreflight::new(&qualification),
        |entry| entry.size > 0,
        |_| true,
    )
    .await;
    assert!(matches!(result,
        Err(ControllerError::Namespace(NamespacePreflightError::Collision(collision)))
            if collision.existing == path("FILE") && collision.colliding == path("file")));
    assert_eq!(qualification.seals.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn exact_records_still_qualify_every_containing_parent_and_seal() {
    let qualification = Qualification::new(
        NamespaceSemantics::BYTE_EXACT,
        NamespaceSemantics::BYTE_EXACT,
    );
    let plan = preflight_sync_scoped(
        entries(vec![file("a/b/c", 1)]),
        entries(vec![]),
        ComparisonPolicy::default(),
        None,
        crate::engine::namespace::NamespacePreflight::new(&qualification),
        |_| true,
        |_| true,
    )
    .await
    .unwrap();
    assert_eq!(plan.operations(), 1);
    assert_eq!(
        *qualification.parents.lock().unwrap(),
        vec![
            NamespaceParent::Directory(path("a/b")),
            NamespaceParent::Directory(path("a")),
            NamespaceParent::Root,
        ]
    );
    assert_eq!(qualification.seals.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn content_wrapper_seals_only_after_native_scan_completion() {
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = Arc::clone(&finished);
    let source = EntryStream::spawn_blocking(1, move |sender| {
        sender.blocking_send(Ok(file("a", 1))).unwrap();
        worker_finished.store(true, Ordering::Release);
        Ok(())
    });
    let mut qualification = Qualification::new(
        NamespaceSemantics::BYTE_EXACT,
        NamespaceSemantics::BYTE_EXACT,
    );
    qualification.scan_finished = Some(finished);
    let mut plan = preflight_sync_scoped_with_content(
        OrderedReconciler::new(source, entries(vec![file("a", 1)])),
        ComparisonPolicy {
            mode: crate::engine::planner::ComparisonMode::Checksum,
            ..Default::default()
        },
        None,
        crate::engine::namespace::NamespacePreflight::new(&qualification),
        |_| true,
        |_| futures::future::ready(Ok(true)),
        |_, _| async { Ok(false) },
    )
    .await
    .unwrap();
    assert!(matches!(
        plan.reader.next().await.unwrap(),
        Some(SyncOp::Update { .. })
    ));
    assert_eq!(qualification.seals.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn failed_qualification_and_scan_drain_never_seal_partial_set() {
    let source = EntryStream::spawn_blocking(1, move |sender| {
        while sender.blocking_send(Ok(file("a", 1))).is_ok() {}
        Err(Box::new(std::io::Error::other("injected scan drain failure")) as BoxError)
    });
    let mut qualification = Qualification::new(
        NamespaceSemantics::BYTE_EXACT,
        NamespaceSemantics::BYTE_EXACT,
    );
    qualification.fail_qualify = true;
    let result = preflight_sync(
        source,
        entries(vec![]),
        ComparisonPolicy::default(),
        None,
        crate::engine::namespace::NamespacePreflight::new(&qualification),
        |_| true,
    )
    .await;
    assert!(
        matches!(result, Err(ControllerError::PreflightDrain { operation, .. })
        if matches!(*operation, ControllerError::Namespace(NamespacePreflightError::Io(_))))
    );
    assert_eq!(qualification.seals.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn late_scan_failure_prevents_qualification_seal() {
    let qualification = Qualification::new(
        NamespaceSemantics::BYTE_EXACT,
        NamespaceSemantics::BYTE_EXACT,
    );
    let source = EntryStream::new(stream::iter([
        Ok(file("a", 1)),
        Err(Box::new(std::io::Error::other("injected late scan failure")) as BoxError),
    ]));
    let result = preflight_sync(
        source,
        entries(vec![]),
        ComparisonPolicy::default(),
        None,
        crate::engine::namespace::NamespacePreflight::new(&qualification),
        |_| true,
    )
    .await;
    assert!(matches!(result, Err(ControllerError::Reconcile(_))));
    assert_eq!(qualification.seals.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn failed_seal_prevents_plan_return() {
    let mut qualification = Qualification::new(
        NamespaceSemantics::BYTE_EXACT,
        NamespaceSemantics::BYTE_EXACT,
    );
    qualification.fail_seal = true;
    let result = preflight_sync(
        entries(vec![file("a", 1)]),
        entries(vec![]),
        ComparisonPolicy::default(),
        None,
        crate::engine::namespace::NamespacePreflight::new(&qualification),
        |_| true,
    )
    .await;
    assert!(matches!(
        result,
        Err(ControllerError::Namespace(NamespacePreflightError::Io(_)))
    ));
    assert_eq!(qualification.seals.load(Ordering::Relaxed), 1);
}
