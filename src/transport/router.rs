use super::{
    dual::DualTransport, local::LocalTransport, s3::S3Transport, ssh::SshTransport, TransferResult,
    Transport,
};
use crate::error::Result;
use crate::integrity::{ChecksumType, IntegrityVerifier};
use crate::path::SyncPath;
use crate::ssh::config::{parse_ssh_config, SshConfig};
use async_trait::async_trait;
use std::path::Path;

/// Router that dispatches to the appropriate transport based on path types
///
/// This allows SyncEngine to work with both local, remote, and S3 paths seamlessly.
pub enum TransportRouter {
    Local(LocalTransport),
    Dual(DualTransport),
    S3(S3Transport),
}

impl TransportRouter {
    /// Create a transport router based on source and destination paths
    ///
    /// Rules:
    /// - Local → Local: Use LocalTransport
    /// - Remote → Local: Use DualTransport (SSH for source, Local for dest)
    /// - Local → Remote: Use DualTransport (Local for source, SSH for dest)
    /// - Remote → Remote: Not supported yet (would require two SSH connections)
    ///
    /// `pool_size` controls the number of SSH connections in the pool for parallel transfers.
    /// Should typically match the number of parallel workers.
    pub async fn new(
        source: &SyncPath,
        destination: &SyncPath,
        checksum_type: ChecksumType,
        verify_on_write: bool,
        pool_size: usize,
    ) -> Result<Self> {
        let verifier = IntegrityVerifier::new(checksum_type, verify_on_write);

        match (source, destination) {
            (SyncPath::Local(_), SyncPath::Local(_)) => {
                // Both local: use local transport
                Ok(TransportRouter::Local(LocalTransport::with_verifier(
                    verifier,
                )))
            }
            (SyncPath::Local(_), SyncPath::Remote { host, user, .. }) => {
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
                let dest_transport =
                    Box::new(SshTransport::with_pool_size(&config, pool_size).await?);
                let dual = DualTransport::new(source_transport, dest_transport);
                Ok(TransportRouter::Dual(dual))
            }
            (SyncPath::Remote { host, user, .. }, SyncPath::Local(_)) => {
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

                let source_transport =
                    Box::new(SshTransport::with_pool_size(&config, pool_size).await?);
                let dest_transport = Box::new(LocalTransport::with_verifier(verifier));
                let dual = DualTransport::new(source_transport, dest_transport);
                Ok(TransportRouter::Dual(dual))
            }
            (SyncPath::Remote { .. }, SyncPath::Remote { .. }) => {
                // Both remote: not supported yet
                Err(crate::error::SyncError::Io(std::io::Error::other(
                    "Remote-to-remote sync not yet supported",
                )))
            }
            (
                SyncPath::Local(_),
                SyncPath::S3 {
                    bucket,
                    key,
                    region,
                    endpoint,
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
            (
                SyncPath::S3 {
                    bucket,
                    key,
                    region,
                    endpoint,
                },
                SyncPath::Local(_),
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
            (SyncPath::S3 { .. }, SyncPath::S3 { .. }) => {
                // S3 → S3: not yet supported
                Err(crate::error::SyncError::Io(std::io::Error::other(
                    "S3-to-S3 sync not yet supported",
                )))
            }
            (SyncPath::S3 { .. }, SyncPath::Remote { .. })
            | (SyncPath::Remote { .. }, SyncPath::S3 { .. }) => {
                // S3 ↔ Remote SSH: not yet supported
                Err(crate::error::SyncError::Io(std::io::Error::other(
                    "S3-to-SSH sync not yet supported",
                )))
            }
        }
    }
}

#[async_trait]
impl Transport for TransportRouter {
    async fn scan(&self, path: &Path) -> Result<Vec<crate::sync::scanner::FileEntry>> {
        match self {
            TransportRouter::Local(t) => t.scan(path).await,
            TransportRouter::Dual(t) => t.scan(path).await,
            TransportRouter::S3(t) => t.scan(path).await,
        }
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match self {
            TransportRouter::Local(t) => t.exists(path).await,
            TransportRouter::Dual(t) => t.exists(path).await,
            TransportRouter::S3(t) => t.exists(path).await,
        }
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        match self {
            TransportRouter::Local(t) => t.metadata(path).await,
            TransportRouter::Dual(t) => t.metadata(path).await,
            TransportRouter::S3(t) => t.metadata(path).await,
        }
    }

    async fn file_info(&self, path: &Path) -> Result<super::FileInfo> {
        match self {
            TransportRouter::Local(t) => t.file_info(path).await,
            TransportRouter::Dual(t) => t.file_info(path).await,
            TransportRouter::S3(t) => t.file_info(path).await,
        }
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_dir_all(path).await,
            TransportRouter::Dual(t) => t.create_dir_all(path).await,
            TransportRouter::S3(t) => t.create_dir_all(path).await,
        }
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        match self {
            TransportRouter::Local(t) => t.copy_file(source, dest).await,
            TransportRouter::Dual(t) => t.copy_file(source, dest).await,
            TransportRouter::S3(t) => t.copy_file(source, dest).await,
        }
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        match self {
            TransportRouter::Local(t) => t.sync_file_with_delta(source, dest).await,
            TransportRouter::Dual(t) => t.sync_file_with_delta(source, dest).await,
            TransportRouter::S3(t) => t.sync_file_with_delta(source, dest).await,
        }
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.remove(path, is_dir).await,
            TransportRouter::Dual(t) => t.remove(path, is_dir).await,
            TransportRouter::S3(t) => t.remove(path, is_dir).await,
        }
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_hardlink(source, dest).await,
            TransportRouter::Dual(t) => t.create_hardlink(source, dest).await,
            TransportRouter::S3(t) => t.create_hardlink(source, dest).await,
        }
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_symlink(target, dest).await,
            TransportRouter::Dual(t) => t.create_symlink(target, dest).await,
            TransportRouter::S3(t) => t.create_symlink(target, dest).await,
        }
    }
}
