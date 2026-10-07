#![allow(dead_code)]
pub mod io;
pub mod local;
pub mod local_entry_scan;
pub(crate) mod local_identity;
pub mod receipt;
pub mod transfer;

use crate::error::{Result, SyncError};
use async_trait::async_trait;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use io::{BoxReader, ExpectedDestination, StagedWriter};
#[allow(unused_imports)]
pub use receipt::{
    DestinationReceipt, PublishedDestinationReceipt, VerifiedExistingDestinationReceipt,
};

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

/// Policy-facing endpoint and session capabilities.
///
/// Wire capability bits are translated into this semantic representation at
/// negotiation, so transfer and executor policy do not depend on protocol
/// encoding. Endpoint type remains useful for diagnostics, not feature tests.
#[derive(Debug, Clone, Copy)]
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
    /// BLAKE3 content verification is available through this endpoint.
    pub blake3: bool,
    /// The endpoint can provide rolling block signatures for delta planning.
    pub rolling_signatures: bool,
    /// The endpoint participates in multiplexed requests.
    pub multiplexing: bool,
    /// Native path bytes can be represented without lossy conversion.
    pub raw_paths: bool,
    /// Compressed transfer frames are supported.
    pub zstd: bool,
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
            blake3: false,
            rolling_signatures: false,
            multiplexing: false,
            raw_paths: false,
            zstd: false,
            modtime_precision: Duration::ZERO,
        }
    }
}

impl Capabilities {
    /// Translate negotiated v3 wire flags into endpoint semantics once, at
    /// the protocol boundary. Transfer and executor code must not depend on
    /// wire-level capability bits.
    pub fn from_negotiated_wire(
        wire: sy::protocol::CapabilitySet,
        modtime_precision_ns: u64,
    ) -> Self {
        use sy::protocol::CapabilitySet as Wire;

        let staged_write = wire.contains(Wire::STAGED_WRITE);
        let blake3 = wire.contains(Wire::BLAKE3);
        Self {
            atomic_rename: wire.contains(Wire::ATOMIC_REPLACE),
            streaming_read: true,
            staged_write,
            staged_verify: staged_write && blake3,
            random_read: wire.contains(Wire::RANDOM_READ),
            random_write: wire.contains(Wire::RANDOM_WRITE),
            reflink: wire.contains(Wire::REFLINK),
            sparse: wire.contains(Wire::SPARSE),
            server_side_copy: false,
            preserve_xattrs: wire.contains(Wire::XATTR),
            preserve_acls: wire.contains(Wire::ACL),
            preserve_hardlinks: wire.contains(Wire::HARDLINK),
            preserve_flags: wire.contains(Wire::BSD_FLAGS),
            blake3,
            rolling_signatures: wire.contains(Wire::ROLLING_SIGNATURES),
            multiplexing: wire.contains(Wire::MULTIPLEXING),
            raw_paths: wire.contains(Wire::RAW_PATHS),
            zstd: wire.contains(Wire::ZSTD),
            modtime_precision: Duration::from_nanos(modtime_precision_ns),
        }
    }

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
            blake3: true,
            rolling_signatures: true,
            multiplexing: false,
            raw_paths: cfg!(unix),
            zstd: true,
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

    /// Open a regular file for a same-host native transfer. Implementations
    /// must resolve the path through their rooted authority and return the
    /// held file descriptor; transfer code validates its identity before and
    /// after copying. `None` selects the generic streaming path.
    async fn open_native_file(&self, _path: &Path) -> Result<Option<std::fs::File>> {
        Ok(None)
    }

    /// Open a transfer source while following symlinks according to the
    /// caller's explicit copy-links policy. Implementations must return the
    /// held target file so bytes and preservation can share its identity.
    async fn open_native_file_following(&self, _path: &Path) -> Result<Option<std::fs::File>> {
        Ok(None)
    }

    /// Read requested preservation from the held file that will supply the
    /// transfer bytes. Endpoints that cannot bind metadata reads to that file
    /// must refuse rather than silently reopening the visible path.
    async fn read_open_file_preservation(
        &self,
        path: &Path,
        _file: &std::fs::File,
        request: io::PreservationRequest,
    ) -> Result<io::Preservation> {
        if request.xattrs || request.acl {
            return Err(SyncError::Config(format!(
                "{:?} endpoint cannot bind requested preservation to an open source file",
                self.endpoint_type()
            )));
        }
        let _ = path;
        Ok(io::Preservation::default())
    }

    /// Check whether a held native file exposes useful sparse extents. This
    /// probe must use the supplied descriptor rather than reopening its path.
    async fn native_file_has_sparse_holes(&self, _file: &std::fs::File) -> Result<bool> {
        Ok(false)
    }

    async fn exists(&self, path: &Path) -> Result<bool>;
    async fn metadata(&self, path: &Path) -> Result<FileMetadata>;

    /// Stat through symlinks (--copy-links: the entry is the link's target).
    /// Endpoints without follow-capable resolution must refuse rather than
    /// return link metadata that would misdescribe the transfer.
    async fn metadata_following(&self, path: &Path) -> Result<FileMetadata> {
        Err(SyncError::Config(format!(
            "{:?} endpoint cannot resolve symlink targets for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

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

    /// Begin a transactional incremental write against the scanned destination
    /// state. Implementations must reject a mismatch before staging and recheck
    /// the expectation immediately before commit. A separate stat and rename
    /// remain weaker than an atomic compare-and-swap against hostile writers.
    ///
    /// The returned writer owns staging state. `commit` is the only operation
    /// that may make the new object visible at `path`.
    async fn begin_write(
        &self,
        path: &Path,
        _expected_destination: io::ExpectedDestination,
    ) -> Result<Box<dyn StagedWriter>> {
        Err(SyncError::Config(format!(
            "{:?} endpoint does not implement staged writes for {}",
            self.endpoint_type(),
            path.display()
        )))
    }

    async fn remove(&self, path: &Path, recursive: bool) -> Result<()>;
    async fn create_dir_all(&self, path: &Path) -> Result<()>;
    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        self.replace_symlink(target, dest, ExpectedDestination::Absent, None)
            .await
    }

    /// Stage link metadata privately and validate the scanned destination before publication.
    async fn replace_symlink(
        &self,
        _target: &Path,
        _dest: &Path,
        _expected: ExpectedDestination,
        _modified: Option<crate::engine::domain::Timestamp>,
    ) -> Result<()> {
        Err(crate::error::SyncError::Config(
            "transactional symlink replacement is unsupported".into(),
        ))
    }
    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::Capabilities;
    use std::time::Duration;
    use sy::protocol::CapabilitySet;

    #[test]
    fn negotiated_wire_capabilities_map_to_endpoint_semantics() {
        let wire = CapabilitySet::all();
        let capabilities = Capabilities::from_negotiated_wire(wire, 17);

        assert!(capabilities.atomic_rename);
        assert!(capabilities.streaming_read);
        assert!(capabilities.staged_write);
        assert!(capabilities.staged_verify);
        assert!(capabilities.random_read);
        assert!(capabilities.random_write);
        assert!(capabilities.reflink);
        assert!(capabilities.sparse);
        assert!(!capabilities.server_side_copy);
        assert!(capabilities.preserve_xattrs);
        assert!(capabilities.preserve_acls);
        assert!(capabilities.preserve_hardlinks);
        assert!(capabilities.preserve_flags);
        assert!(capabilities.blake3);
        assert!(capabilities.rolling_signatures);
        assert!(capabilities.multiplexing);
        assert!(capabilities.raw_paths);
        assert!(capabilities.zstd);
        assert_eq!(capabilities.modtime_precision, Duration::from_nanos(17));
    }

    #[test]
    fn staged_verification_requires_staging_and_blake3() {
        let capabilities = Capabilities::from_negotiated_wire(CapabilitySet::STAGED_WRITE, 0);
        assert!(capabilities.staged_write);
        assert!(!capabilities.staged_verify);
        assert!(!capabilities.blake3);
    }
}
