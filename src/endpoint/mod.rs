pub mod local;
#[cfg(feature = "ssh")]
pub mod ssh;
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "gcs")]
pub mod gcs;

use crate::error::Result;
use crate::sync::scanner::{FileEntry, ScanOptions};
use async_trait::async_trait;
use std::path::Path;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    Local,
    Ssh,
    S3,
    Gcs,
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub delta_sync: bool,
    pub streaming_read: bool,
    pub batch_mkdir: bool,
    pub cow_writes: bool,
    pub preserve_xattrs: bool,
    pub preserve_acls: bool,
    pub modtime_precision: Duration,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            delta_sync: true,
            streaming_read: true,
            batch_mkdir: true,
            cow_writes: true,
            preserve_xattrs: false,
            preserve_acls: false,
            modtime_precision: Duration::ZERO,
        }
    }
}

impl Capabilities {
    pub fn local() -> Self {
        Self {
            modtime_precision: Duration::from_nanos(1),
            cow_writes: true,
            delta_sync: true,
            streaming_read: true,
            batch_mkdir: true,
            preserve_xattrs: true,
            preserve_acls: cfg!(all(unix, feature = "acl")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub is_symlink: bool,
    #[cfg(unix)]
    pub mode: u32,
    #[cfg(unix)]
    pub uid: u32,
    #[cfg(unix)]
    pub gid: u32,
    #[cfg(unix)]
    pub nlink: u64,
}

#[async_trait]
pub trait Endpoint: Send + Sync {
    fn endpoint_type(&self) -> EndpointType;
    fn capabilities(&self) -> &Capabilities;
    fn root(&self) -> &Path;

    async fn scan(&self, opts: ScanOptions) -> Result<Vec<FileEntry>>;
    async fn exists(&self, path: &Path) -> Result<bool>;
    async fn metadata(&self, path: &Path) -> Result<FileMetadata>;
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    async fn write_file(&self, path: &Path, data: &[u8], meta: &FileMetadata) -> Result<()>;
    async fn remove(&self, path: &Path, recursive: bool) -> Result<()>;
    async fn create_dir_all(&self, path: &Path) -> Result<()>;
    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()>;
    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()>;
    async fn set_mtime(&self, path: &Path, mtime: SystemTime) -> Result<()>;
    async fn set_permissions(&self, path: &Path, mode: u32) -> Result<()>;

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<u64> {
        let data = self.read_file(source).await?;
        let meta = self.metadata(source).await?;
        self.write_file(dest, &data, &meta).await?;
        Ok(data.len() as u64)
    }
}
