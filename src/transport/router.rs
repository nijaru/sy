use super::{dual::DualTransport, local::LocalTransport, ssh::SshTransport, Transport, TransferResult};
use crate::error::Result;
use crate::path::SyncPath;
use crate::ssh::config::{parse_ssh_config, SshConfig};
use async_trait::async_trait;
use std::path::Path;

/// Router that dispatches to the appropriate transport based on path types
///
/// This allows SyncEngine to work with both local and remote paths seamlessly.
pub enum TransportRouter {
    Local(LocalTransport),
    Dual(DualTransport),
}

impl TransportRouter {
    /// Create a transport router based on source and destination paths
    ///
    /// Rules:
    /// - Local → Local: Use LocalTransport
    /// - Remote → Local: Use DualTransport (SSH for source, Local for dest)
    /// - Local → Remote: Use DualTransport (Local for source, SSH for dest)
    /// - Remote → Remote: Not supported yet (would require two SSH connections)
    pub async fn new(source: &SyncPath, destination: &SyncPath) -> Result<Self> {
        match (source, destination) {
            (SyncPath::Local(_), SyncPath::Local(_)) => {
                // Both local: use local transport
                Ok(TransportRouter::Local(LocalTransport::new()))
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

                let source_transport = Box::new(LocalTransport::new());
                let dest_transport = Box::new(SshTransport::new(&config).await?);
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

                let source_transport = Box::new(SshTransport::new(&config).await?);
                let dest_transport = Box::new(LocalTransport::new());
                let dual = DualTransport::new(source_transport, dest_transport);
                Ok(TransportRouter::Dual(dual))
            }
            (SyncPath::Remote { .. }, SyncPath::Remote { .. }) => {
                // Both remote: not supported yet
                Err(crate::error::SyncError::Io(std::io::Error::other(
                    "Remote-to-remote sync not yet supported",
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
        }
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match self {
            TransportRouter::Local(t) => t.exists(path).await,
            TransportRouter::Dual(t) => t.exists(path).await,
        }
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        match self {
            TransportRouter::Local(t) => t.metadata(path).await,
            TransportRouter::Dual(t) => t.metadata(path).await,
        }
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_dir_all(path).await,
            TransportRouter::Dual(t) => t.create_dir_all(path).await,
        }
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        match self {
            TransportRouter::Local(t) => t.copy_file(source, dest).await,
            TransportRouter::Dual(t) => t.copy_file(source, dest).await,
        }
    }

    async fn sync_file_with_delta(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        match self {
            TransportRouter::Local(t) => t.sync_file_with_delta(source, dest).await,
            TransportRouter::Dual(t) => t.sync_file_with_delta(source, dest).await,
        }
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.remove(path, is_dir).await,
            TransportRouter::Dual(t) => t.remove(path, is_dir).await,
        }
    }
}
