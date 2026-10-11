use crate::sync::session::SyncSession;
use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

// Only a dirty notification or the first watcher failure is retained. Event paths
// are deliberately discarded: every notification schedules a full sync.
type Change = std::result::Result<(), Arc<notify::Error>>;

pub struct WatchSession {
    session: SyncSession,
    source: PathBuf,
    debounce: Duration,
}

impl WatchSession {
    pub fn new(session: SyncSession, source: PathBuf, debounce: Duration) -> Self {
        Self {
            session,
            source,
            debounce,
        }
    }

    pub async fn watch(&self) -> Result<()> {
        self.watch_until(tokio::signal::ctrl_c()).await
    }

    async fn watch_until(&self, shutdown: impl Future<Output = std::io::Result<()>>) -> Result<()> {
        let (sender, receiver) = watch::channel(Ok(()));
        let source = self.source.clone();
        let watcher = tokio::task::spawn_blocking(move || -> Result<_> {
            // FSEvents reports canonical paths; register the same root spelling.
            let source = source.canonicalize().context("Resolving watched source")?;
            let mut watcher =
                notify::recommended_watcher(move |event| signal_change(&sender, event))
                    .context("Creating file watcher")?;
            // Subscribe before the initial sync so changes during it are not lost.
            watcher
                .watch(&source, RecursiveMode::Recursive)
                .context("Watching source")?;
            Ok(watcher)
        })
        .await
        .context("Starting file watcher")??;
        println!(
            "Watching {} for changes (Ctrl+C to stop)...",
            self.source.display()
        );

        // The watcher remains scope-owned on future cancellation. Normal shutdown
        // and errors also join its backend threads off the Tokio worker.
        let result = self.run(receiver, shutdown).await;
        tokio::task::spawn_blocking(move || drop(watcher))
            .await
            .context("Stopping file watcher")?;
        result
    }

    async fn run(
        &self,
        mut changes: watch::Receiver<Change>,
        shutdown: impl Future<Output = std::io::Result<()>>,
    ) -> Result<()> {
        tokio::pin!(shutdown);
        loop {
            check_change(&changes.borrow())?;
            if let std::task::Poll::Ready(stopped) = futures::poll!(&mut shutdown) {
                stopped.context("Waiting for watch shutdown")?;
                return Ok(());
            }
            let sync = self.session.sync();
            tokio::pin!(sync);
            let stats = tokio::select! {
                biased;
                // Poll sync before shutdown so the drain branch owns work that
                // has actually started; an already-stopped watch starts no work.
                result = &mut sync => result.context("Syncing watched source")?,
                stopped = &mut shutdown => {
                    // Drain in-flight work rather than dropping a synchronization
                    // future with workers or staged destination state still owned.
                    sync.await.context("Syncing watched source during shutdown")?;
                    check_change(&changes.borrow())?;
                    stopped.context("Waiting for watch shutdown")?;
                    return Ok(());
                }
            };
            check_change(&changes.borrow())?;
            println!(
                "Sync complete ({} files, {} bytes)",
                stats.files_created + stats.files_updated,
                stats.bytes_transferred
            );

            let mut deadline = None;
            loop {
                let timer = async {
                    match deadline {
                        Some(when) => tokio::time::sleep_until(when).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    biased;
                    stopped = &mut shutdown => {
                        check_change(&changes.borrow())?;
                        stopped.context("Waiting for watch shutdown")?;
                        return Ok(());
                    }
                    // Timer priority and a deadline fixed by the first change
                    // guarantee progress even when notifications never stop.
                    () = timer => {
                        check_change(&changes.borrow_and_update())?;
                        break;
                    }
                    changed = changes.changed() => {
                        changed.context("File watcher disconnected")?;
                        check_change(&changes.borrow_and_update())?;
                        deadline.get_or_insert_with(|| Instant::now() + self.debounce);
                    }
                }
            }
        }
    }
}

fn check_change(change: &Change) -> Result<()> {
    change.clone().map_err(anyhow::Error::from)
}

fn signal_change(sender: &watch::Sender<Change>, event: notify::Result<Event>) {
    match event {
        Ok(event) if should_sync_event(&event) => {
            sender.send_if_modified(|change| change.is_ok());
        }
        Err(error) => {
            sender.send_if_modified(|change| {
                if change.is_err() {
                    return false;
                }
                *change = Err(Arc::new(error));
                true
            });
        }
        _ => {}
    }
}

fn should_sync_event(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Other
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::config::SyncConfig;
    use crate::sync::scanner::ScanOptions;
    use crate::sync::session::EndpointPair;
    use std::fs;
    use std::io;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    fn session(
        source: &Path,
        destination: &Path,
        config: SyncConfig,
        options: ScanOptions,
    ) -> WatchSession {
        let source_endpoint = EndpointPair::Local(Box::new(
            crate::endpoint::local::LocalEndpoint::new(source.to_path_buf()),
        ));
        let dest_endpoint = EndpointPair::Local(Box::new(
            crate::endpoint::local::LocalEndpoint::new(destination.to_path_buf()),
        ));
        WatchSession::new(
            SyncSession::new(source_endpoint, dest_endpoint, config).with_scan_options(options),
            source.to_path_buf(),
            Duration::from_millis(30),
        )
    }

    async fn wait_for(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("watch sync did not publish the expected file");
    }

    #[tokio::test]
    async fn notifications_are_coalesced_and_first_error_is_sticky() {
        let (sender, mut receiver) = watch::channel(Ok(()));
        let access = Event::new(EventKind::Access(notify::event::AccessKind::Read));
        signal_change(&sender, Ok(access));
        assert!(!receiver.has_changed().unwrap());
        for _ in 0..100_000 {
            signal_change(
                &sender,
                Ok(Event::new(EventKind::Other).add_path("a".into())),
            );
        }
        receiver.changed().await.unwrap();
        check_change(&receiver.borrow_and_update()).unwrap();
        assert!(
            !receiver.has_changed().unwrap(),
            "only one pending signal is retained"
        );
        signal_change(&sender, Err(notify::Error::generic("first failure")));
        signal_change(&sender, Ok(Event::new(EventKind::Other)));
        signal_change(&sender, Err(notify::Error::generic("second failure")));
        assert!(check_change(&receiver.borrow_and_update())
            .unwrap_err()
            .to_string()
            .contains("first failure"));
    }

    #[tokio::test]
    async fn continuous_notifications_sync_on_deadline_and_keep_changes_during_sync() {
        let source = TempDir::new().unwrap();
        let destination = TempDir::new().unwrap();
        fs::write(source.path().join("initial"), "initial").unwrap();
        let watch = session(
            source.path(),
            destination.path(),
            SyncConfig::test_default(),
            ScanOptions::default(),
        );
        let (sender, receiver) = watch::channel(Ok(()));
        let (stop, stopped) = oneshot::channel();
        let producer = async {
            // This notification is pending during the initial sync. It must not
            // be cleared when that sync completes.
            signal_change(&sender, Ok(Event::new(EventKind::Other)));
            wait_for(&destination.path().join("initial")).await;
            fs::write(source.path().join("during-initial"), "during-initial").unwrap();
            wait_for(&destination.path().join("during-initial")).await;
            fs::write(source.path().join("later"), "later").unwrap();
            let until = Instant::now() + Duration::from_millis(300);
            while Instant::now() < until {
                signal_change(&sender, Ok(Event::new(EventKind::Other)));
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(
                destination.path().join("later").exists(),
                "continuous input postponed synchronization"
            );
            stop.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(
            watch.run(receiver, async { stopped.await.map_err(io::Error::other) }),
            producer
        );
        result.unwrap();
        assert_eq!(
            fs::read(destination.path().join("later")).unwrap(),
            b"later"
        );
    }

    #[tokio::test]
    async fn watcher_lifecycle_preserves_selection_and_shuts_down() {
        for dirs_only in [false, true] {
            let source = TempDir::new().unwrap();
            let destination = TempDir::new().unwrap();
            fs::create_dir(source.path().join(".git")).unwrap();
            fs::create_dir(source.path().join("nested")).unwrap();
            fs::write(source.path().join(".gitignore"), "ignored\n").unwrap();
            fs::write(source.path().join("ignored"), "ignored").unwrap();
            fs::write(source.path().join("excluded"), "excluded").unwrap();
            fs::write(source.path().join("nested/deep"), "deep").unwrap();
            fs::write(source.path().join("initial"), "initial").unwrap();
            let mut config = SyncConfig::test_default();
            config.filter_engine.add_exclude("excluded").unwrap();
            let watch = session(
                source.path(),
                destination.path(),
                config,
                ScanOptions {
                    respect_gitignore: true,
                    include_git_dir: false,
                    dirs_only,
                },
            );
            let (stop, stopped) = oneshot::channel();
            let producer = async {
                wait_for(&destination.path().join("initial")).await;
                stop.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(
                watch.watch_until(async { stopped.await.map_err(io::Error::other) }),
                producer
            );
            result.unwrap();
            assert!(!destination.path().join("ignored").exists());
            assert!(!destination.path().join("excluded").exists());
            assert!(!destination.path().join(".git").exists());
            assert_eq!(destination.path().join("nested/deep").exists(), !dirs_only);
        }
    }

    #[tokio::test]
    async fn errors_and_disconnect_are_propagated() {
        let source = TempDir::new().unwrap();
        let destination = TempDir::new().unwrap();
        let watch = session(
            source.path(),
            destination.path(),
            SyncConfig::test_default(),
            ScanOptions::default(),
        );
        let (sender, receiver) = watch::channel(Ok(()));
        signal_change(&sender, Err(notify::Error::generic("watch failed")));
        let error = watch
            .run(receiver, std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("watch failed"));
        let (sender, receiver) = watch::channel(Ok(()));
        drop(sender);
        let error = watch
            .run(receiver, std::future::pending())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disconnected"));

        let watch = session(
            &source.path().join("missing"),
            destination.path(),
            SyncConfig::test_default(),
            ScanOptions::default(),
        );
        assert!(watch.watch_until(std::future::pending()).await.is_err());
    }

    #[tokio::test]
    async fn sync_and_shutdown_errors_are_propagated() {
        let source = TempDir::new().unwrap();
        let destination = TempDir::new().unwrap();
        fs::write(source.path().join("file"), "data").unwrap();
        let watch = session(
            source.path(),
            &destination.path().join("not-a-directory"),
            SyncConfig::test_default(),
            ScanOptions::default(),
        );
        fs::write(destination.path().join("not-a-directory"), "old").unwrap();
        assert!(watch.watch_until(std::future::pending()).await.is_err());
        assert_eq!(
            fs::read(destination.path().join("not-a-directory")).unwrap(),
            b"old"
        );

        let watch = session(
            source.path(),
            destination.path(),
            SyncConfig::test_default(),
            ScanOptions::default(),
        );
        let (_sender, receiver) = watch::channel(Ok(()));
        let mut shutdown_polled = false;
        let shutdown = std::future::poll_fn(move |cx| {
            if shutdown_polled {
                std::task::Poll::Ready(Err(io::Error::other("shutdown failed")))
            } else {
                shutdown_polled = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        });
        let error = watch.run(receiver, shutdown).await.unwrap_err();
        assert!(error.to_string().contains("shutdown"));
        assert_eq!(
            fs::read(destination.path().join("file")).unwrap(),
            b"data",
            "shutdown must drain the initial synchronization"
        );

        fs::write(source.path().join("after-stop"), "do not copy").unwrap();
        let (_sender, receiver) = watch::channel(Ok(()));
        watch.run(receiver, async { Ok(()) }).await.unwrap();
        assert!(!destination.path().join("after-stop").exists());
    }
}
