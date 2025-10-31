use super::{TransferResult, Transport};
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

    async fn file_info(&self, path: &Path) -> Result<super::FileInfo> {
        // Get file info from destination
        self.dest.file_info(path).await
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        // Create on destination
        self.dest.create_dir_all(path).await
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Cross-transport copy: delegate to destination transport
        // The destination transport (e.g., SshTransport) knows how to copy
        // from a local source path to its destination (local or remote)

        tracing::debug!(
            "DualTransport: copying {} to {}",
            source.display(),
            dest.display()
        );

        // Delegate to destination transport which handles the cross-transport copy
        // For local→remote: dest is SshTransport which reads from local source and writes remote
        // For remote→local: dest is LocalTransport but source should be readable
        self.dest.copy_file(source, dest).await
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Check if destination exists - delta sync requires existing dest
        if !self.dest.exists(dest).await? {
            tracing::debug!("Destination doesn't exist, using full copy");
            return self.copy_file(source, dest).await;
        }

        // Try to use destination transport's delta sync capability
        // This works for local→remote (SshTransport.sync_file_with_delta)
        // where source path is readable from local filesystem
        match self.dest.sync_file_with_delta(source, dest).await {
            Ok(result) => {
                tracing::debug!(
                    "DualTransport: delta sync succeeded via destination transport (likely local→remote)"
                );
                Ok(result)
            }
            Err(e) => {
                // Destination transport doesn't support delta sync for this case
                // This happens for:
                // 1. Remote→local (would need reverse protocol)
                // 2. Any transport that doesn't implement delta sync
                tracing::debug!(
                    "DualTransport: destination transport delta sync failed ({}), trying source transport",
                    e
                );

                // Try source transport's delta sync as fallback
                match self.source.sync_file_with_delta(source, dest).await {
                    Ok(result) => {
                        tracing::debug!("DualTransport: delta sync succeeded via source transport");
                        Ok(result)
                    }
                    Err(e2) => {
                        // Neither transport supports delta sync for this configuration
                        tracing::debug!(
                            "DualTransport: both transports failed delta sync ({}, {}), falling back to full copy",
                            e, e2
                        );
                        self.copy_file(source, dest).await
                    }
                }
            }
        }
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

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        // Read from destination (where the file exists after sync)
        self.dest.read_file(path).await
    }

    async fn check_disk_space(&self, path: &Path, bytes_needed: u64) -> Result<()> {
        // Check disk space on destination
        self.dest.check_disk_space(path, bytes_needed).await
    }

    async fn set_xattrs(&self, path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<()> {
        // Set xattrs on destination
        self.dest.set_xattrs(path, xattrs).await
    }

    async fn set_acls(&self, path: &Path, acls_text: &str) -> Result<()> {
        // Set ACLs on destination
        self.dest.set_acls(path, acls_text).await
    }

    async fn set_bsd_flags(&self, path: &Path, flags: u32) -> Result<()> {
        // Set BSD flags on destination
        self.dest.set_bsd_flags(path, flags).await
    }
}
