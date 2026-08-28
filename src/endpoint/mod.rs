#[cfg(feature = "gcs")]
pub mod gcs;
pub mod io;
pub mod local;
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "ssh")]
pub mod ssh;

use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, ScanOptions};
use async_trait::async_trait;
use std::path::Path;
use std::time::{Duration, SystemTime};

pub use io::{BoxReader, StagedWriter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    Local,
    Ssh,
    S3,
    Gcs,
}

/// Operations an endpoint can perform efficiently and safely.
///
/// Transfer strategy selection should be based on these capabilities rather
/// than endpoint type checks. Endpoint type remains useful for diagnostics and
/// protocol selection, but it should not encode transfer policy.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// A staged object can atomically replace the destination path.
    pub atomic_rename: bool,
    /// Files can be consumed incrementally without whole-file buffering.
    pub streaming_read: bool,
    /// Writes can be staged incrementally and committed transactionally.
    pub staged_write: bool,
    /// Efficient random reads are supported.
    pub random_read: bool,
    /// Efficient random writes to staging state are supported.
    pub random_write: bool,
    /// Copy-on-write cloning/reflinking is available or may be probed.
    pub reflink: bool,
    /// Sparse-file semantics are available.
    pub sparse: bool,
    /// Backend-native server-side copies are available.
    pub server_side_copy: bool,
    pub preserve_xattrs: bool,
    pub preserve_acls: bool,
    pub preserve_hardlinks: bool,
    pub modtime_precision: Duration,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            atomic_rename: false,
            streaming_read: false,
            staged_write: false,
            random_read: false,
            random_write: false,
            reflink: false,
            sparse: false,
            server_side_copy: false,
            preserve_xattrs: false,
            preserve_acls: false,
            preserve_hardlinks: false,
            modtime_precision: Duration::ZERO,
        }
    }
}

impl Capabilities {
    pub fn local() -> Self {
        Self {
            atomic_rename: true,
            streaming_read: true,
            staged_write: true,
            random_read: true,
            random_write: true,
            // Reflink support is filesystem-specific and must still be probed
            // before selecting a reflink transfer strategy.
            reflink: true,
            sparse: true,
            server_side_copy: false,
            preserve_xattrs: cfg!(unix),
            preserve_acls: cfg!(all(unix, feature = "acl")),
            preserve_hardlinks: true,
            modtime_precision: Duration::from_nanos(1),
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

    /// Open a file for incremental reads.
    ///
    /// This is the v0.5 transfer contract. Implementations should avoid
    /// materializing the file in memory. The default exists only to allow the
    /// architecture migration to land endpoint-by-endpoint.
    async fn open_reader(&self, path: &Path) -> Result<BoxReader> {
        Err(SyncError::Config(format!(
            "{:?} endpoint does not implement streaming reads for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    /// Begin a transactional incremental write.
    ///
    /// The returned writer owns staging state. `commit` is the only operation
    /// that may make the new object visible at `path`.
    async fn begin_write(&self, path: &Path) -> Result<Box<dyn StagedWriter>> {
        Err(SyncError::Config(format!(
            "{:?} endpoint does not implement staged writes for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    // Legacy whole-file methods retained during the v0.5 migration. New
    // transfer code should use open_reader/begin_write instead. They can be
    // removed once all endpoint implementations and auxiliary features have
    // moved to the streaming contract.
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
