#[cfg(feature = "s3")]
use super::s3::S3Transport;
#[cfg(feature = "ssh")]
use super::ssh::SshTransport;
use super::{dual::DualTransport, local::LocalTransport, TransferResult, Transport};
use crate::error::Result;
use crate::integrity::{ChecksumType, IntegrityVerifier};
use crate::path::SyncPath;
use crate::retry::RetryConfig;
#[cfg(feature = "ssh")]
use crate::ssh::config::{parse_ssh_config, SshConfig};
use crate::sync::scanner::ScanOptions;
use async_trait::async_trait;
use std::path::Path;

/// Router that dispatches to the appropriate transport based on path types
///
/// This allows SyncEngine to work with both local, remote, and S3 paths seamlessly.
pub enum TransportRouter {
    Local(LocalTransport),
    Dual(DualTransport),
    #[cfg(feature = "s3")]
    S3(S3Transport),
}

impl TransportRouter {
    /// Create a transport router based on source and destination paths
    ///
    /// Rules:
    /// - Local → Local: Use LocalTransport
    /// - Remote → Local: Use DualTransport (SSH for source, Local for dest)
    /// - Local → Remote: Use DualTransport (Local for source, SSH for dest)
    /// - Remote → Remote: Use DualTransport (SSH for source, SSH for dest)
    ///
    /// `pool_size` controls the number of SSH connections in the pool for parallel transfers.
    /// Should typically match the number of parallel workers.
    ///
    /// `retry_config` configures network interruption recovery behavior for SSH operations.
    pub async fn new(
        source: &SyncPath,
        destination: &SyncPath,
        checksum_type: ChecksumType,
        verify_on_write: bool,
        pool_size: usize,
        retry_config: RetryConfig,
    ) -> Result<Self> {
        let verifier = IntegrityVerifier::new(checksum_type, verify_on_write);

        match (source, destination) {
            (SyncPath::Local { .. }, SyncPath::Local { .. }) => {
                // Both local: use local transport
                Ok(TransportRouter::Local(LocalTransport::with_verifier(
                    verifier,
                )))
            }
            #[cfg(feature = "ssh")]
            (SyncPath::Local { .. }, SyncPath::Remote { host, user, .. }) => {
                // Local → Remote: use DualTransport
                let config = if let Some(user) = user {
                    SshConfig {
                        hostname: host.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    parse_ssh_config(host)?
                };

                let source_transport = Box::new(LocalTransport::with_verifier(verifier.clone()));
                let dest_transport = Box::new(
                    SshTransport::with_retry_config(&config, pool_size, retry_config.clone())
                        .await?,
                );
                let dual = DualTransport::new(source_transport, dest_transport);
                Ok(TransportRouter::Dual(dual))
            }
            #[cfg(feature = "ssh")]
            (SyncPath::Remote { host, user, .. }, SyncPath::Local { .. }) => {
                // Remote → Local: use DualTransport
                let config = if let Some(user) = user {
                    SshConfig {
                        hostname: host.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    parse_ssh_config(host)?
                };

                let source_transport = Box::new(
                    SshTransport::with_retry_config(&config, pool_size, retry_config.clone())
                        .await?,
                );
                let dest_transport = Box::new(LocalTransport::with_verifier(verifier));
                let dual = DualTransport::new(source_transport, dest_transport);
                Ok(TransportRouter::Dual(dual))
            }
            #[cfg(feature = "ssh")]
            (
                SyncPath::Remote {
                    host: source_host,
                    user: source_user,
                    ..
                },
                SyncPath::Remote {
                    host: dest_host,
                    user: dest_user,
                    ..
                },
            ) => {
                // Remote → Remote: use DualTransport with two SSH connections
                let source_config = if let Some(user) = source_user {
                    SshConfig {
                        hostname: source_host.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    parse_ssh_config(source_host)?
                };

                let dest_config = if let Some(user) = dest_user {
                    SshConfig {
                        hostname: dest_host.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    parse_ssh_config(dest_host)?
                };

                let source_transport = Box::new(
                    SshTransport::with_retry_config(
                        &source_config,
                        pool_size,
                        retry_config.clone(),
                    )
                    .await?,
                );
                let dest_transport = Box::new(
                    SshTransport::with_retry_config(&dest_config, pool_size, retry_config.clone())
                        .await?,
                );
                let dual = DualTransport::new(source_transport, dest_transport);
                Ok(TransportRouter::Dual(dual))
            }
            #[cfg(not(feature = "ssh"))]
            (SyncPath::Remote { .. }, _) | (_, SyncPath::Remote { .. }) => {
                Err(crate::error::SyncError::Io(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "SSH support is disabled. Install with: cargo install sy --features ssh",
                )))
            }
            #[cfg(feature = "s3")]
            (
                SyncPath::Local { .. },
                SyncPath::S3 {
                    bucket,
                    key,
                    region,
                    endpoint,
                    ..
                },
            ) => {
                // Local → S3: use S3Transport for destination
                let s3_transport = S3Transport::new(
                    bucket.clone(),
                    key.clone(),
                    region.clone(),
                    endpoint.clone(),
                )
                .await?;
                Ok(TransportRouter::S3(s3_transport))
            }
            #[cfg(feature = "s3")]
            (
                SyncPath::S3 {
                    bucket,
                    key,
                    region,
                    endpoint,
                    ..
                },
                SyncPath::Local { .. },
            ) => {
                // S3 → Local: use S3Transport for source
                let s3_transport = S3Transport::new(
                    bucket.clone(),
                    key.clone(),
                    region.clone(),
                    endpoint.clone(),
                )
                .await?;
                Ok(TransportRouter::S3(s3_transport))
            }
            #[cfg(feature = "s3")]
            (SyncPath::S3 { .. }, SyncPath::S3 { .. }) => {
                // S3 → S3: not yet supported
                Err(crate::error::SyncError::Io(std::io::Error::other(
                    "S3-to-S3 sync not yet supported",
                )))
            }
            #[cfg(feature = "s3")]
            (SyncPath::S3 { .. }, SyncPath::Remote { .. })
            | (SyncPath::Remote { .. }, SyncPath::S3 { .. }) => {
                // S3 ↔ Remote SSH: not yet supported
                Err(crate::error::SyncError::Io(std::io::Error::other(
                    "S3-to-SSH sync not yet supported",
                )))
            }
            #[cfg(not(feature = "s3"))]
            (SyncPath::S3 { .. }, _) | (_, SyncPath::S3 { .. }) => {
                Err(crate::error::SyncError::Io(std::io::Error::other(
                    "S3 support not enabled. Reinstall with: cargo install sy --features s3",
                )))
            }
        }
    }

    /// Apply scan options to the underlying transport
    pub fn with_scan_options(self, options: ScanOptions) -> Self {
        match self {
            TransportRouter::Local(mut t) => {
                t.set_scan_options(options);
                TransportRouter::Local(t)
            }
            TransportRouter::Dual(mut t) => {
                t.set_scan_options(options);
                TransportRouter::Dual(t)
            }
            #[cfg(feature = "s3")]
            TransportRouter::S3(mut t) => {
                t.set_scan_options(options);
                TransportRouter::S3(t)
            }
        }
    }
}

#[async_trait]
impl Transport for TransportRouter {
    fn set_scan_options(&mut self, options: ScanOptions) {
        match self {
            TransportRouter::Local(t) => t.set_scan_options(options),
            TransportRouter::Dual(t) => t.set_scan_options(options),
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.set_scan_options(options),
        }
    }

    async fn prepare_for_transfer(&self, file_count: usize) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.prepare_for_transfer(file_count).await,
            TransportRouter::Dual(t) => t.prepare_for_transfer(file_count).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.prepare_for_transfer(file_count).await,
        }
    }

    async fn scan(&self, path: &Path) -> Result<Vec<crate::sync::scanner::FileEntry>> {
        match self {
            TransportRouter::Local(t) => t.scan(path).await,
            TransportRouter::Dual(t) => t.scan(path).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.scan(path).await,
        }
    }

    async fn scan_streaming(
        &self,
        path: &Path,
    ) -> Result<futures::stream::BoxStream<'static, Result<crate::sync::scanner::FileEntry>>> {
        match self {
            TransportRouter::Local(t) => t.scan_streaming(path).await,
            TransportRouter::Dual(t) => t.scan_streaming(path).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.scan_streaming(path).await,
        }
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match self {
            TransportRouter::Local(t) => t.exists(path).await,
            TransportRouter::Dual(t) => t.exists(path).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.exists(path).await,
        }
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        match self {
            TransportRouter::Local(t) => t.metadata(path).await,
            TransportRouter::Dual(t) => t.metadata(path).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.metadata(path).await,
        }
    }

    async fn file_info(&self, path: &Path) -> Result<super::FileInfo> {
        match self {
            TransportRouter::Local(t) => t.file_info(path).await,
            TransportRouter::Dual(t) => t.file_info(path).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.file_info(path).await,
        }
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_dir_all(path).await,
            TransportRouter::Dual(t) => t.create_dir_all(path).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.create_dir_all(path).await,
        }
    }

    async fn create_dirs_batch(&self, paths: &[&Path]) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_dirs_batch(paths).await,
            TransportRouter::Dual(t) => t.create_dirs_batch(paths).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.create_dirs_batch(paths).await,
        }
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        match self {
            TransportRouter::Local(t) => t.copy_file(source, dest).await,
            TransportRouter::Dual(t) => t.copy_file(source, dest).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.copy_file(source, dest).await,
        }
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        match self {
            TransportRouter::Local(t) => t.sync_file_with_delta(source, dest).await,
            TransportRouter::Dual(t) => t.sync_file_with_delta(source, dest).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.sync_file_with_delta(source, dest).await,
        }
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.remove(path, is_dir).await,
            TransportRouter::Dual(t) => t.remove(path, is_dir).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.remove(path, is_dir).await,
        }
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_hardlink(source, dest).await,
            TransportRouter::Dual(t) => t.create_hardlink(source, dest).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.create_hardlink(source, dest).await,
        }
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_symlink(target, dest).await,
            TransportRouter::Dual(t) => t.create_symlink(target, dest).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.create_symlink(target, dest).await,
        }
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        match self {
            TransportRouter::Local(t) => t.read_file(path).await,
            TransportRouter::Dual(t) => t.read_file(path).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.read_file(path).await,
        }
    }

    async fn check_disk_space(&self, path: &Path, bytes_needed: u64) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.check_disk_space(path, bytes_needed).await,
            TransportRouter::Dual(t) => t.check_disk_space(path, bytes_needed).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.check_disk_space(path, bytes_needed).await,
        }
    }

    async fn set_xattrs(&self, path: &Path, xattrs: &[(String, Vec<u8>)]) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.set_xattrs(path, xattrs).await,
            TransportRouter::Dual(t) => t.set_xattrs(path, xattrs).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.set_xattrs(path, xattrs).await,
        }
    }

    async fn set_acls(&self, path: &Path, acls_text: &str) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.set_acls(path, acls_text).await,
            TransportRouter::Dual(t) => t.set_acls(path, acls_text).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.set_acls(path, acls_text).await,
        }
    }

    async fn set_bsd_flags(&self, path: &Path, flags: u32) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.set_bsd_flags(path, flags).await,
            TransportRouter::Dual(t) => t.set_bsd_flags(path, flags).await,
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => t.set_bsd_flags(path, flags).await,
        }
    }

    async fn bulk_copy_files(
        &self,
        source_base: &Path,
        dest_base: &Path,
        relative_paths: &[&Path],
    ) -> Result<u64> {
        match self {
            TransportRouter::Local(t) => {
                t.bulk_copy_files(source_base, dest_base, relative_paths)
                    .await
            }
            TransportRouter::Dual(t) => {
                t.bulk_copy_files(source_base, dest_base, relative_paths)
                    .await
            }
            #[cfg(feature = "s3")]
            TransportRouter::S3(t) => {
                t.bulk_copy_files(source_base, dest_base, relative_paths)
                    .await
            }
        }
    }
}
