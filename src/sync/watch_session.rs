use crate::sync::session::{EndpointPair, SyncSession};
use crate::sync::SyncConfig;
use anyhow::Result;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};
use tokio::signal;

/// Watch mode using SyncSession (v0.4)
///
/// Watches a directory for changes and syncs to destination on each change.
/// Uses SyncSession for strategy dispatch instead of SyncEngine + Transport.
pub struct WatchSession {
    session: SyncSession,
    source: PathBuf,
    destination: PathBuf,
    debounce: Duration,
}

impl WatchSession {
    pub fn new(
        session: SyncSession,
        source: PathBuf,
        destination: PathBuf,
        debounce: Duration,
    ) -> Self {
        Self {
            session,
            source,
            destination,
            debounce,
        }
    }

    /// Create a WatchSession from source/dest paths and config
    pub fn from_paths(
        source: &crate::path::SyncPath,
        dest: &crate::path::SyncPath,
        config: SyncConfig,
        debounce: Duration,
    ) -> Result<Self> {
        let source_endpoint = EndpointPair::from_sync_path(source);
        let dest_endpoint = EndpointPair::from_sync_path(dest);
        let session = SyncSession::new(source_endpoint, dest_endpoint, config);

        Ok(Self {
            session,
            source: source.path().to_path_buf(),
            destination: dest.path().to_path_buf(),
            debounce,
        })
    }

    pub async fn watch(&self) -> Result<()> {
        // Initial sync
        tracing::info!("Running initial sync...");
        let stats = self.session.sync().await?;
        tracing::info!(
            "Initial sync complete: {} files, {} bytes",
            stats.files_created + stats.files_updated,
            stats.bytes_transferred
        );

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
                    // Force sync on error to ensure consistency
                    pending_changes.push(Event::new(notify::EventKind::Other));
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Check if we should sync (debounce timeout reached)
                    if !pending_changes.is_empty() && last_sync.elapsed() >= self.debounce {
                        tracing::info!("Detected {} changes, syncing...", pending_changes.len());
                        println!("📝 Changes detected, syncing...");

                        match self.session.sync().await {
                            Ok(stats) => {
                                println!(
                                    "✓ Sync complete ({} files, {} bytes)\n",
                                    stats.files_created + stats.files_updated,
                                    stats.bytes_transferred
                                );
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
                    tracing::error!("File watcher disconnected unexpectedly");
                    eprintln!("❌ File watcher stopped. Exiting.");
                    break;
                }
            }
        }

        Ok(())
    }

    fn should_sync_event(&self, event: &Event) -> bool {
        use notify::EventKind;

        match event.kind {
            // File created, modified, or removed
            EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Other => true,
            // Ignore metadata-only changes (access time, etc.)
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::session::EndpointPair;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_watch_session_creation() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();

        let source_endpoint = EndpointPair::Local {
            path: source.clone(),
        };
        let dest_endpoint = EndpointPair::Local {
            path: destination.clone(),
        };
        let config = SyncConfig::default();

        let session = SyncSession::new(source_endpoint, dest_endpoint, config);
        let watch_session = WatchSession::new(
            session,
            source.clone(),
            destination.clone(),
            Duration::from_millis(500),
        );

        assert_eq!(watch_session.source, source);
        assert_eq!(watch_session.destination, destination);
        assert_eq!(watch_session.debounce, Duration::from_millis(500));
    }

    #[test]
    fn test_should_sync_event() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("src");
        let destination = temp.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();

        let source_endpoint = EndpointPair::Local {
            path: source.clone(),
        };
        let dest_endpoint = EndpointPair::Local {
            path: destination.clone(),
        };
        let config = SyncConfig::default();

        let session = SyncSession::new(source_endpoint, dest_endpoint, config);
        let watch_session = WatchSession::new(
            session,
            source,
            destination,
            Duration::from_millis(500),
        );

        // Create events
        let create_event = Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File,
        ));
        let modify_event = Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Data(notify::event::DataChange::Content),
        ));
        let remove_event = Event::new(notify::EventKind::Remove(
            notify::event::RemoveKind::File,
        ));
        let access_event = Event::new(notify::EventKind::Access(
            notify::event::AccessKind::Read,
        ));

        assert!(watch_session.should_sync_event(&create_event));
        assert!(watch_session.should_sync_event(&modify_event));
        assert!(watch_session.should_sync_event(&remove_event));
        assert!(!watch_session.should_sync_event(&access_event));
    }
}
