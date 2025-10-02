use super::{local::LocalTransport, ssh::SshTransport, Transport};
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
    Ssh(SshTransport),
}

impl TransportRouter {
    /// Create a transport router based on source and destination paths
    ///
    /// Rules:
    /// - Local → Local: Use LocalTransport
    /// - Remote → Local: Use SshTransport (pull)
    /// - Local → Remote: Use SshTransport (push)
    /// - Remote → Remote: Not supported yet (would require two SSH connections)
    pub async fn new(source: &SyncPath, destination: &SyncPath) -> Result<Self> {
        match (source, destination) {
            (SyncPath::Local(_), SyncPath::Local(_)) => {
                // Both local: use local transport
                Ok(TransportRouter::Local(LocalTransport::new()))
            }
            (SyncPath::Remote { host, user, .. }, SyncPath::Local(_))
            | (SyncPath::Local(_), SyncPath::Remote { host, user, .. }) => {
                // One is remote: use SSH transport
                let config = if let Some(user) = user {
                    // Explicit user provided
                    SshConfig {
                        hostname: host.clone(),
                        user: user.clone(),
                        ..Default::default()
                    }
                } else {
                    // Parse from SSH config
                    parse_ssh_config(host)?
                };

                let ssh_transport = SshTransport::new(&config).await?;
                Ok(TransportRouter::Ssh(ssh_transport))
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
            TransportRouter::Ssh(t) => t.scan(path).await,
        }
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match self {
            TransportRouter::Local(t) => t.exists(path).await,
            TransportRouter::Ssh(t) => t.exists(path).await,
        }
    }

    async fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        match self {
            TransportRouter::Local(t) => t.metadata(path).await,
            TransportRouter::Ssh(t) => t.metadata(path).await,
        }
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.create_dir_all(path).await,
            TransportRouter::Ssh(t) => t.create_dir_all(path).await,
        }
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.copy_file(source, dest).await,
            TransportRouter::Ssh(t) => t.copy_file(source, dest).await,
        }
    }

    async fn remove(&self, path: &Path, is_dir: bool) -> Result<()> {
        match self {
            TransportRouter::Local(t) => t.remove(path, is_dir).await,
            TransportRouter::Ssh(t) => t.remove(path, is_dir).await,
        }
    }
}
