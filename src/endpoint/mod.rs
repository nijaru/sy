#[cfg(feature = "gcs")]
pub mod gcs;
pub mod io;
pub mod local;
mod local_scan;
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "ssh")]
pub mod ssh;
pub mod transfer;

use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, ScanOptions};
use async_trait::async_trait;
use futures::Stream;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, SystemTime};

pub use io::{BoxReader, StagedWriter};

/// Ordered entry stream used by incremental reconciliation.
///
/// Implementations must yield entries in ascending relative-path order. Local
/// endpoints provide a bounded producer; the default implementation is a
/// compatibility fallback for endpoints that have not migrated yet.
pub type EntryStream = Pin<Box<dyn Stream<Item = Result<FileEntry>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    Local,
    // These become live as their v0.5 endpoint implementations replace the
    // current special-case transport paths.
    #[allow(dead_code)]
    Ssh,
    #[allow(dead_code)]
    S3,
    #[allow(dead_code)]
    Gcs,
}

/// Operations an endpoint can perform efficiently and safely.
///
/// Transfer strategy selection is based on these capabilities rather than on
/// endpoint-type branches. Endpoint type remains useful for diagnostics and
/// protocol negotiation.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// A staged object can atomically replace the destination path.
    pub atomic_rename: bool,
    /// Files can be consumed incrementally without whole-file buffering.
    pub streaming_read: bool,
    /// Writes can be staged incrementally and committed transactionally.
    pub staged_write: bool,
    /// Staged bytes can be hashed before commit.
    pub staged_verify: bool,
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
    pub preserve_flags: bool,
    pub modtime_precision: Duration,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            atomic_rename: false,
            streaming_read: false,
            staged_write: false,
            staged_verify: false,
            random_read: false,
            random_write: false,
            reflink: false,
            sparse: false,
            server_side_copy: false,
            preserve_xattrs: false,
            preserve_acls: false,
            preserve_hardlinks: false,
            preserve_flags: false,
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
            staged_verify: true,
            random_read: true,
            random_write: true,
            // Filesystem-specific support is probed before strategy selection.
            reflink: true,
            sparse: true,
            server_side_copy: false,
            preserve_xattrs: cfg!(unix),
            preserve_acls: cfg!(all(unix, feature = "acl")),
            preserve_hardlinks: true,
            preserve_flags: cfg!(target_os = "macos"),
            modtime_precision: Duration::from_nanos(1),
        }
    }
}

/// Metadata required to stage a regular-file transfer.
///
/// Ownership and link topology live at the reconciliation/preservation layer;
/// they are intentionally not part of the byte-transfer contract.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    pub is_symlink: bool,
    #[cfg(unix)]
    pub mode: u32,
}

#[async_trait]
pub trait Endpoint: Send + Sync {
    fn endpoint_type(&self) -> EndpointType;
    fn capabilities(&self) -> &Capabilities;
    fn root(&self) -> &Path;

    /// Return a directly addressable native filesystem path when the endpoint
    /// can safely expose one. Transfer policy must still consult capabilities.
    fn native_path(&self, _path: &Path) -> Option<PathBuf> {
        None
    }

    async fn scan(&self, opts: ScanOptions) -> Result<Vec<FileEntry>>;

    /// Scan in ascending relative-path order for merge reconciliation.
    async fn scan_ordered(&self, opts: ScanOptions) -> Result<EntryStream> {
        let mut entries = self.scan(opts).await?;
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(Box::pin(futures::stream::iter(entries.into_iter().map(Ok))))
    }

    async fn exists(&self, path: &Path) -> Result<bool>;
    async fn metadata(&self, path: &Path) -> Result<FileMetadata>;

    /// Read extended attributes only when preservation policy requests them.
    async fn read_xattrs(&self, path: &Path) -> Result<Vec<(OsString, Vec<u8>)>> {
        Err(SyncError::Config(format!(
            "{:?} endpoint cannot read xattrs for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    async fn write_xattrs(&self, path: &Path, _xattrs: &[(OsString, Vec<u8>)]) -> Result<()> {
        Err(SyncError::Config(format!(
            "{:?} endpoint cannot write xattrs for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    /// Portable textual ACL representation used at the endpoint boundary.
    async fn read_acl(&self, path: &Path) -> Result<Option<String>> {
        Err(SyncError::Config(format!(
            "{:?} endpoint cannot read ACLs for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    async fn write_acl(&self, path: &Path, _acl: &str) -> Result<()> {
        Err(SyncError::Config(format!(
            "{:?} endpoint cannot write ACLs for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    async fn read_bsd_flags(&self, path: &Path) -> Result<Option<u32>> {
        Err(SyncError::Config(format!(
            "{:?} endpoint cannot read BSD flags for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    async fn write_bsd_flags(&self, path: &Path, _flags: u32) -> Result<()> {
        Err(SyncError::Config(format!(
            "{:?} endpoint cannot write BSD flags for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    /// Open a file for incremental reads.
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

    async fn remove(&self, path: &Path, recursive: bool) -> Result<()>;
    async fn create_dir_all(&self, path: &Path) -> Result<()>;
    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()>;
    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()>;
}
