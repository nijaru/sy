use super::Transport;
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

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<()> {
        // Special handling: need to read from source, write to dest
        // For now, assume source is local (will need enhancement for remote→remote)
        self.dest.copy_file(source, dest).await
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        // Remove from destination
        self.dest.remove(path, is_dir).await
    }
}
