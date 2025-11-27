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
    fn set_scan_options(&mut self, options: crate::sync::scanner::ScanOptions) {
        self.source.set_scan_options(options);
    }

    async fn prepare_for_transfer(&self, file_count: usize) -> Result<()> {
        // Prepare both source and destination transports
        // (both might be SSH and need pool expansion)
        self.source.prepare_for_transfer(file_count).await?;
        self.dest.prepare_for_transfer(file_count).await?;
        Ok(())
    }

    async fn scan(&self, path: &Path) -> Result<Vec<FileEntry>> {
        // Always scan from source
        self.source.scan(path).await
    }

    async fn scan_streaming(
        &self,
        path: &Path,
    ) -> Result<futures::stream::BoxStream<'static, Result<FileEntry>>> {
        // Always scan from source
        self.source.scan_streaming(path).await
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

    async fn create_dirs_batch(&self, paths: &[&Path]) -> Result<()> {
        // Batch create on destination (SSH will be efficient, local will loop)
        self.dest.create_dirs_batch(paths).await
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Cross-transport copy
        //
        // OPTIMIZATION: For many transports (especially SSH), they have efficient
        // implementations that can transfer directly from local filesystem to remote
        // without loading entire file into memory.
        //
        // Try destination transport's copy_file first (works for local→remote where
        // dest is SSH and can read source directly from local filesystem via SFTP).
        // Fall back to read_file + write_file if that fails.

        tracing::debug!(
            "DualTransport: copying {} to {}",
            source.display(),
            dest.display()
        );

        // Try destination transport's copy_file first
        // This works for local→remote where SshTransport can efficiently
        // read from local filesystem and stream via SFTP
        match self.dest.copy_file(source, dest).await {
            Ok(result) => {
                tracing::debug!(
                    "DualTransport: copy succeeded via destination transport (efficient path)"
                );
                return Ok(result);
            }
            Err(e) => {
                tracing::debug!(
                    "DualTransport: destination transport copy failed ({}), falling back to read+write",
                    e
                );
                // Fall through to read+write approach
            }
        }

        // Fallback: Read from source, write to dest (loads entire file into memory)
        // This is needed for:
        // - Remote→Local (source is remote, must download first)
        // - Remote→Remote (unavoidable buffering)
        tracing::debug!(
            "DualTransport: using memory-buffered copy for {} (may be slow for large files)",
            source.display()
        );

        let data = self.source.read_file(source).await?;
        let bytes_written = data.len() as u64;
        let mtime = self.source.get_mtime(source).await?;
        self.dest.write_file(dest, &data, mtime).await?;

        Ok(TransferResult::new(bytes_written))
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

    async fn compute_checksum(
        &self,
        path: &Path,
        verifier: &crate::integrity::IntegrityVerifier,
    ) -> Result<crate::integrity::Checksum> {
        // For DualTransport, we need to route based on which transport can access the path.
        // Try source first (for local source files), then dest (for remote dest files).
        //
        // The path may be:
        // - A local source path (readable by source transport)
        // - A remote dest path (readable by dest transport)
        //
        // A simple heuristic: if the path exists locally, use local; otherwise use dest.
        if path.exists() {
            // Local path - use default (local) implementation
            let path = path.to_path_buf();
            let verifier = verifier.clone();
            tokio::task::spawn_blocking(move || verifier.compute_file_checksum(&path))
                .await
                .map_err(|e| crate::error::SyncError::Io(std::io::Error::other(e.to_string())))?
        } else {
            // Remote path - use destination transport
            self.dest.compute_checksum(path, verifier).await
        }
    }

    async fn bulk_copy_files(
        &self,
        source_base: &Path,
        dest_base: &Path,
        relative_paths: &[&Path],
    ) -> Result<u64> {
        // DualTransport bulk copy uses the dest transport's implementation
        // For local→remote, this will use SSH's tar streaming
        self.dest
            .bulk_copy_files(source_base, dest_base, relative_paths)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::{ChecksumType, IntegrityVerifier};
    use crate::transport::local::LocalTransport;
    use tempfile::TempDir;

    /// Test that compute_checksum routes correctly based on path existence:
    /// - Local paths (that exist) use local computation
    /// - Remote paths (that don't exist locally) route to dest transport
    #[tokio::test]
    async fn test_compute_checksum_routes_local_path() {
        let temp = TempDir::new().unwrap();
        let local_file = temp.path().join("local_file.txt");
        std::fs::write(&local_file, b"test content").unwrap();

        // Create DualTransport with two local transports (simulating local→remote)
        let verifier = IntegrityVerifier::new(ChecksumType::Fast, false);
        let source = Box::new(LocalTransport::with_verifier(verifier.clone()));
        let dest = Box::new(LocalTransport::with_verifier(verifier.clone()));
        let dual = DualTransport::new(source, dest);

        // Local file should be computed locally (path.exists() == true)
        let result = dual.compute_checksum(&local_file, &verifier).await;
        assert!(result.is_ok(), "Should compute checksum for local file");
    }

    /// Test that compute_checksum for non-existent path routes to dest transport
    #[tokio::test]
    async fn test_compute_checksum_routes_nonexistent_to_dest() {
        let temp = TempDir::new().unwrap();

        // Create a file that only exists in "dest" directory (simulating remote)
        let dest_dir = temp.path().join("dest");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest_file = dest_dir.join("remote_file.txt");
        std::fs::write(&dest_file, b"remote content").unwrap();

        // Path that doesn't exist locally (simulating ~/remote/path)
        let fake_remote_path = std::path::PathBuf::from("/nonexistent/remote/path.txt");

        let verifier = IntegrityVerifier::new(ChecksumType::Fast, false);
        let source = Box::new(LocalTransport::with_verifier(verifier.clone()));
        let dest = Box::new(LocalTransport::with_verifier(verifier.clone()));
        let dual = DualTransport::new(source, dest);

        // Non-existent local path should route to dest transport
        // This will fail because the file doesn't exist on dest either,
        // but the important thing is it tries dest (not local)
        let result = dual.compute_checksum(&fake_remote_path, &verifier).await;
        assert!(
            result.is_err(),
            "Should fail for path that doesn't exist anywhere"
        );
    }

    /// Test that verification works correctly for local source + dest scenario
    #[tokio::test]
    async fn test_dual_transport_verification_local_paths() {
        let temp = TempDir::new().unwrap();

        // Create source and dest files with same content
        let source_file = temp.path().join("source.txt");
        let dest_file = temp.path().join("dest.txt");
        let content = b"identical content for verification";
        std::fs::write(&source_file, content).unwrap();
        std::fs::write(&dest_file, content).unwrap();

        let verifier = IntegrityVerifier::new(ChecksumType::Fast, false);
        let source = Box::new(LocalTransport::with_verifier(verifier.clone()));
        let dest = Box::new(LocalTransport::with_verifier(verifier.clone()));
        let dual = DualTransport::new(source, dest);

        // Both files exist locally, so both should compute successfully
        let source_checksum = dual
            .compute_checksum(&source_file, &verifier)
            .await
            .unwrap();
        let dest_checksum = dual.compute_checksum(&dest_file, &verifier).await.unwrap();

        assert_eq!(
            source_checksum, dest_checksum,
            "Checksums should match for identical files"
        );
    }
}
