use crate::integrity::ChecksumType;
use crate::sync::SyncEngine;
use crate::transport::Transport;
use anyhow::Result;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};
use tokio::signal;

pub struct WatchMode<T: Transport> {
    engine: SyncEngine<T>,
    source: PathBuf,
    destination: PathBuf,
    debounce: Duration,
}

impl<T: Transport + 'static> WatchMode<T> {
    pub fn new(
        engine: SyncEngine<T>,
        source: PathBuf,
        destination: PathBuf,
        debounce: Duration,
    ) -> Self {
        Self {
            engine,
            source,
            destination,
            debounce,
        }
    }

    pub async fn watch(&self) -> Result<()> {
        // Initial sync
        tracing::info!("Running initial sync...");
        self.engine.sync(&self.source, &self.destination).await?;

        // Set up file watcher
        let (tx, rx) = channel();
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(tx)?;
        watcher.watch(&self.source, RecursiveMode::Recursive)?;

        println!(
            "\n🔍 Watching {} for changes (Ctrl+C to stop)...\n",
            self.source.display()
        );

        // Event loop with debouncing
        let mut pending_changes = Vec::new();
        let mut last_sync = Instant::now();

        // Set up Ctrl+C handler
        let ctrl_c = signal::ctrl_c();
        tokio::pin!(ctrl_c);

        loop {
            // Check for Ctrl+C
            tokio::select! {
                _ = &mut ctrl_c => {
                    println!("\n⏹️  Stopping watch mode...");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    // Continue to check file events
                }
            }

            // Process file system events
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(event)) => {
                    // Filter out events we don't care about
                    if self.should_sync_event(&event) {
                        pending_changes.push(event);
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!("Watch error: {}", e);
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Check if we should sync (debounce timeout reached)
                    if !pending_changes.is_empty()
                        && last_sync.elapsed() >= self.debounce
                    {
                        tracing::info!("Detected {} changes, syncing...", pending_changes.len());
                        println!("📝 Changes detected, syncing...");

                        match self.engine.sync(&self.source, &self.destination).await {
                            Ok(_) => {
                                println!("✓ Sync complete\n");
                            }
                            Err(e) => {
                                eprintln!("✗ Sync failed: {}\n", e);
                            }
                        }

                        pending_changes.clear();
                        last_sync = Instant::now();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    break; // Watcher dropped
                }
            }
        }

        Ok(())
    }

    fn should_sync_event(&self, event: &Event) -> bool {
        use notify::EventKind;

        match event.kind {
            // File created, modified, or removed
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
            // Ignore metadata-only changes (access time, etc.)
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::local::LocalTransport;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_watch_mode_creation() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();

        let transport = LocalTransport::new();
        let engine = SyncEngine::new(
            transport,
            false, // dry_run
            false, // delete
            true,  // quiet
            10,    // parallel
            None,  // min_size
            None,  // max_size
            vec![], // exclude
            None,  // bwlimit
            false, // resume
            10,    // checkpoint_files
            100,   // checkpoint_bytes
            false, // json
            ChecksumType::None, // verification_mode
            false, // verify_on_write
        );

        let watch_mode = WatchMode::new(
            engine,
            source.clone(),
            destination.clone(),
            Duration::from_millis(500),
        );

        assert_eq!(watch_mode.source, source);
        assert_eq!(watch_mode.destination, destination);
        assert_eq!(watch_mode.debounce, Duration::from_millis(500));
    }

    #[test]
    fn test_should_sync_event() {
        use notify::{Event, EventKind};

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();

        let transport = LocalTransport::new();
        let engine = SyncEngine::new(
            transport,
            false,
            false,
            true,
            10,
            None,
            None,
            vec![],
            None,
            false,
            10,
            100,
            false,
            ChecksumType::None,
            false,
        );

        let watch_mode = WatchMode::new(
            engine,
            source,
            destination,
            Duration::from_millis(500),
        );

        // Should sync on create, modify, remove
        let create_event = Event::new(EventKind::Create(notify::event::CreateKind::File));
        assert!(watch_mode.should_sync_event(&create_event));

        let modify_event = Event::new(EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )));
        assert!(watch_mode.should_sync_event(&modify_event));

        let remove_event = Event::new(EventKind::Remove(notify::event::RemoveKind::File));
        assert!(watch_mode.should_sync_event(&remove_event));

        // Should not sync on access events
        let access_event = Event::new(EventKind::Access(notify::event::AccessKind::Read));
        assert!(!watch_mode.should_sync_event(&access_event));
    }
}
