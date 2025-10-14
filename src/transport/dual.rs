use super::{Transport, TransferResult};
use crate::error::Result;
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use std::path::Path;

/// DualTransport handles operations that span two different transports
///
/// This is used for mixed local/remote operations where the source and
/// destination are on different systems (e.g., local→remote or remote→local).
///
/// Operations are routed based on the context:
/// - scan() operates on source
/// - exists(), create_dir_all(), copy_file(), remove() operate on destination
pub struct DualTransport {
    source: Box<dyn Transport>,
    dest: Box<dyn Transport>,
}

impl DualTransport {
    pub fn new(source: Box<dyn Transport>, dest: Box<dyn Transport>) -> Self {
        Self { source, dest }
    }
}

#[async_trait]
impl Transport for DualTransport {
    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>> {
        // Always scan from source
        self.source.scan(path).await
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        // Check existence on destination
        self.dest.exists(path).await
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        // Get metadata from destination
        self.dest.metadata(path).await
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        // Create on destination
        self.dest.create_dir_all(path).await
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Cross-transport copy: read from source, write to dest
        // Use streaming for files > 10MB to avoid memory issues
        const STREAMING_THRESHOLD: u64 = 10 * 1024 * 1024; // 10MB

        tracing::debug!("DualTransport: copying {} to {}", source.display(), dest.display());

        // Check file size to decide streaming vs buffered
        // Note: metadata() may fail for remote sources, so we default to streaming
        // for safety (better to use more memory-efficient approach than OOM)
        let file_size = match self.source.metadata(source).await {
            Ok(meta) => meta.len(),
            Err(_) => {
                // metadata() not available (e.g., remote source) - use streaming to be safe
                tracing::debug!("Metadata not available, using streaming with progress");

                // Create progress callback for remote sources
                let progress_callback = {
                    let source_display = source.display().to_string();
                    std::sync::Arc::new(move |transferred: u64, total: u64| {
                        if total > 0 {
                            let percent = (transferred as f64 / total as f64 * 100.0) as u64;
                            // Log every 10MB transferred
                            if transferred > 0 && transferred % (10 * 1_048_576) < 65536 {
                                tracing::info!(
                                    "Streaming {}: {}% ({:.1} / {:.1} MB)",
                                    source_display,
                                    percent,
                                    transferred as f64 / 1_048_576.0,
                                    total as f64 / 1_048_576.0
                                );
                            }
                        }
                    })
                };

                return self.source.copy_file_streaming(source, dest, Some(progress_callback)).await;
            }
        };

        if file_size > STREAMING_THRESHOLD {
            tracing::debug!("Using streaming for large file ({} bytes)", file_size);

            // Create progress callback that logs periodically
            let progress_callback = {
                let source_display = source.display().to_string();
                std::sync::Arc::new(move |transferred: u64, total: u64| {
                    if total > 0 {
                        let percent = (transferred as f64 / total as f64 * 100.0) as u64;
                        // Log every 10MB transferred
                        if transferred > 0 && transferred % (10 * 1_048_576) < 65536 {
                            tracing::info!(
                                "Streaming {}: {}% ({:.1} / {:.1} MB)",
                                source_display,
                                percent,
                                transferred as f64 / 1_048_576.0,
                                total as f64 / 1_048_576.0
                            );
                        }
                    }
                })
            };

            // Use streaming for large files with progress logging
            return self.source.copy_file_streaming(source, dest, Some(progress_callback)).await;
        }

        // For small files, use buffered approach
        tracing::debug!("Using buffered copy for small file ({} bytes)", file_size);

        // Read file data from source
        let data = self.source.read_file(source).await?;
        let bytes_written = data.len() as u64;

        // Get source mtime
        let mtime = self.source.get_mtime(source).await?;

        // Write to destination
        self.dest.write_file(dest, &data, mtime).await?;

        tracing::debug!("DualTransport: copied {} bytes", bytes_written);

        Ok(TransferResult::new(bytes_written))
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Cross-transport delta sync would require rsync protocol over network
        // For now, fall back to full file copy
        // TODO: Implement proper delta sync for cross-transport operations
        tracing::debug!("DualTransport: delta sync not yet implemented for cross-transport, using full copy");
        self.copy_file(source, dest).await
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        // Remove from destination
        self.dest.remove(path, is_dir).await
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        // Create hardlink on destination
        self.dest.create_hardlink(source, dest).await
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        // Create symlink on destination
        self.dest.create_symlink(target, dest).await
    }
}
