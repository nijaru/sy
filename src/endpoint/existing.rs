//! Handle-bound proof for removing a source whose destination already exists.
use crate::endpoint::local_identity::metadata_identity;
use crate::endpoint::receipt::VerifiedExistingDestinationReceipt;
use crate::engine::domain::{Entry, EntryKind, RelativePath};
use crate::rooted_fs::{RootedFs, RootedFsError};
use std::io::Read;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FingerprintOptions {
    pub permissions: bool,
    pub times: bool,
    pub xattrs: bool,
    pub acls: bool,
    pub bsd_flags: bool,
}

impl FingerprintOptions {
    pub(crate) const fn buffered_bytes(self) -> u64 {
        // Include bounded metadata sets, encoding validation and container overhead.
        64 * 1024
            + if self.xattrs {
                4 * crate::protocol::MAX_FRAME_PAYLOAD as u64
            } else {
                0
            }
            + if self.acls {
                4 * crate::protocol::MAX_ACL_TEXT_BYTES as u64
            } else {
                0
            }
    }

    pub(crate) const fn bits(self) -> u8 {
        self.permissions as u8
            | (self.times as u8) << 1
            | (self.xattrs as u8) << 2
            | (self.acls as u8) << 3
            | (self.bsd_flags as u8) << 4
    }

    pub(crate) fn from_bits(bits: u8) -> Result<Self> {
        if bits & !31 != 0 {
            return Err(ExistingDestinationError::InvalidSelection);
        }
        Ok(Self {
            permissions: bits & 1 != 0,
            times: bits & 2 != 0,
            xattrs: bits & 4 != 0,
            acls: bits & 8 != 0,
            bsd_flags: bits & 16 != 0,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingFingerprint {
    pub content: [u8; 32],
    pub preservation: [u8; 32],
    pub binding: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub enum ExistingDestinationError {
    #[error(transparent)]
    Rooted(#[from] RootedFsError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("existing-destination verification worker failed: {0}")]
    Worker(String),
    #[error("source and destination namespace bindings are indistinguishable at {0}; refusing source removal")]
    SameBinding(RelativePath),
    #[error("invalid preservation selection")]
    InvalidSelection,
    #[error("source removal requires matching regular-file observations with identities at {0}")]
    MissingObservation(RelativePath),
    #[error("entry changed since observation at {0}")]
    ObservationChanged(RelativePath),
    #[error("destination content no longer matches source at {0}")]
    ContentMismatch(RelativePath),
    #[error("destination does not preserve the requested source metadata at {0}")]
    PreservationMismatch(RelativePath),
}

pub type Result<T> = std::result::Result<T, ExistingDestinationError>;

pub(crate) async fn observed_flags(root: std::path::PathBuf, source: Entry) -> Result<u32> {
    let expected = source
        .identity
        .ok_or_else(|| ExistingDestinationError::MissingObservation(source.path.clone()))?;
    let rooted = RootedFs::open(root).await?;
    tokio::task::spawn_blocking(move || {
        rooted.read_observed_bsd_flags_blocking(&source.path, source.kind, expected)
    })
    .await
    .map_err(|error| ExistingDestinationError::Worker(error.to_string()))?
    .map_err(ExistingDestinationError::from)
}

pub(crate) async fn observed_xattrs(
    root: std::path::PathBuf,
    source: Entry,
) -> Result<Vec<(std::ffi::OsString, Vec<u8>)>> {
    let rooted = RootedFs::open(root).await?;
    tokio::task::spawn_blocking(move || observed_xattrs_blocking(&rooted, &source))
        .await
        .map_err(|error| ExistingDestinationError::Worker(error.to_string()))?
}

pub(crate) async fn observed_acl(
    root: std::path::PathBuf,
    source: Entry,
) -> Result<Option<String>> {
    let rooted = RootedFs::open(root).await?;
    tokio::task::spawn_blocking(move || observed_acl_blocking(&rooted, &source))
        .await
        .map_err(|error| ExistingDestinationError::Worker(error.to_string()))?
}

pub(crate) async fn fingerprint(
    rooted: RootedFs,
    entry: Entry,
    options: FingerprintOptions,
) -> Result<ExistingFingerprint> {
    tokio::task::spawn_blocking(move || fingerprint_blocking(&rooted, &entry, options))
        .await
        .map_err(|error| ExistingDestinationError::Worker(error.to_string()))?
}

fn validate_file(file: &std::fs::File, entry: &Entry) -> Result<std::fs::Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || entry.kind != EntryKind::File
        || entry.identity.is_none()
        || metadata_identity(&metadata, EntryKind::File) != entry.identity
        || metadata.len() != entry.size
    {
        return Err(ExistingDestinationError::ObservationChanged(
            entry.path.clone(),
        ));
    }
    Ok(metadata)
}

pub(crate) fn validate_path(rooted: &RootedFs, entry: &Entry) -> Result<()> {
    if entry.identity.is_none()
        || rooted.path_identity_blocking(&entry.path)? != entry.identity.map(|id| (entry.kind, id))
    {
        return Err(ExistingDestinationError::ObservationChanged(
            entry.path.clone(),
        ));
    }
    Ok(())
}

fn observed_xattrs_blocking(
    rooted: &RootedFs,
    entry: &Entry,
) -> Result<Vec<(std::ffi::OsString, Vec<u8>)>> {
    let file = rooted.open_regular_blocking(&entry.path)?;
    validate_file(&file, entry)?;
    let attrs = rooted.read_open_file_xattrs_blocking(&file, &entry.path)?;
    validate_file(&file, entry)?;
    validate_path(rooted, entry)?;
    Ok(attrs)
}

fn observed_acl_blocking(rooted: &RootedFs, entry: &Entry) -> Result<Option<String>> {
    let file = rooted.open_regular_blocking(&entry.path)?;
    validate_file(&file, entry)?;
    let acl = rooted.read_open_file_acl_blocking(&file, &entry.path)?;
    validate_file(&file, entry)?;
    validate_path(rooted, entry)?;
    Ok(acl)
}

fn fingerprint_blocking(
    rooted: &RootedFs,
    entry: &Entry,
    options: FingerprintOptions,
) -> Result<ExistingFingerprint> {
    let mut file = rooted.open_regular_blocking(&entry.path)?;
    let metadata = validate_file(&file, entry)?;
    #[cfg(not(unix))]
    let _ = &metadata;
    let mut content = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut remaining = entry.size;
    // Read exactly the observed length, not an unbounded concurrently growing file.
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| ExistingDestinationError::ObservationChanged(entry.path.clone()))?;
        let read = match file.read(&mut buffer[..limit]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Err(ExistingDestinationError::ObservationChanged(
                entry.path.clone(),
            ));
        }
        content.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let mut extra = [0];
    loop {
        match file.read(&mut extra) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Ok(0) => break,
            Ok(_) => {
                return Err(ExistingDestinationError::ObservationChanged(
                    entry.path.clone(),
                ))
            }
            Err(error) => return Err(error.into()),
        }
    }

    let mut preservation = blake3::Hasher::new();
    preservation.update(b"sy-existing-preservation-v1\0");
    preservation.update(&[options.bits()]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if options.permissions {
            preservation.update(&(metadata.mode() & 0o7777).to_le_bytes());
        }
        if options.times {
            preservation.update(&metadata.mtime().to_le_bytes());
            preservation.update(&metadata.mtime_nsec().to_le_bytes());
        }
    }
    #[cfg(not(unix))]
    if options.permissions || options.times {
        return Err(RootedFsError::UnsupportedPlatform.into());
    }
    if options.xattrs {
        let mut attrs = rooted.read_open_file_xattrs_blocking(&file, &entry.path)?;
        attrs.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        preservation.update(&(attrs.len() as u64).to_le_bytes());
        for (name, value) in attrs {
            hash_field(
                &mut preservation,
                &crate::engine::native_path::encode(&name),
            );
            hash_field(&mut preservation, &value);
        }
    }
    if options.acls {
        match rooted.read_open_file_acl_blocking(&file, &entry.path)? {
            Some(acl) => {
                preservation.update(&[1]);
                hash_field(&mut preservation, acl.as_bytes());
            }
            None => {
                preservation.update(&[0]);
            }
        }
    }
    if options.bsd_flags {
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            preservation.update(&file.metadata()?.st_flags().to_le_bytes());
        }
        #[cfg(not(target_os = "macos"))]
        return Err(RootedFsError::UnsupportedPlatform.into());
    }
    validate_file(&file, entry)?;
    // A held handle proves the bytes, but not that its inode still owns the name.
    validate_path(rooted, entry)?;
    Ok(ExistingFingerprint {
        content: *content.finalize().as_bytes(),
        preservation: *preservation.finalize().as_bytes(),
        binding: rooted.entry_binding_blocking(&entry.path)?,
    })
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

pub(crate) fn receipt(
    source: &Entry,
    destination: &Entry,
    source_fingerprint: ExistingFingerprint,
    destination_fingerprint: ExistingFingerprint,
) -> Result<VerifiedExistingDestinationReceipt> {
    if source.path != destination.path || !source.is_file() || !destination.is_file() {
        return Err(ExistingDestinationError::MissingObservation(
            source.path.clone(),
        ));
    }
    let source_identity = source
        .identity
        .ok_or_else(|| ExistingDestinationError::MissingObservation(source.path.clone()))?;
    let destination_identity = destination
        .identity
        .ok_or_else(|| ExistingDestinationError::MissingObservation(destination.path.clone()))?;
    if source.identity == destination.identity
        && source_fingerprint.binding == destination_fingerprint.binding
    {
        return Err(ExistingDestinationError::SameBinding(source.path.clone()));
    }
    if source_fingerprint.content != destination_fingerprint.content {
        return Err(ExistingDestinationError::ContentMismatch(
            source.path.clone(),
        ));
    }
    if source_fingerprint.preservation != destination_fingerprint.preservation {
        return Err(ExistingDestinationError::PreservationMismatch(
            source.path.clone(),
        ));
    }
    Ok(VerifiedExistingDestinationReceipt::new(
        source.path.clone(),
        source_identity,
        destination_identity,
    ))
}

pub(crate) async fn remove_verified_source(
    rooted: RootedFs,
    source: Entry,
    receipt: VerifiedExistingDestinationReceipt,
    local_destination: Option<RootedFs>,
) -> Result<()> {
    if receipt.path() != &source.path || Some(receipt.source_identity()) != source.identity {
        return Err(ExistingDestinationError::ObservationChanged(
            source.path.clone(),
        ));
    }
    tokio::task::spawn_blocking(move || {
        // Local destination validation belongs in the unlink worker, not before
        // it is queued. Remote fingerprints perform this check on the peer;
        // neither case promises cross-endpoint atomic compare-and-swap.
        if let Some(destination) = local_destination {
            if rooted.entry_binding_blocking(receipt.path())?
                == destination.entry_binding_blocking(receipt.path())?
            {
                return Err(ExistingDestinationError::SameBinding(
                    receipt.path().clone(),
                ));
            }
            if destination.path_identity_blocking(receipt.path())?
                != Some((EntryKind::File, receipt.destination_identity()))
            {
                return Err(ExistingDestinationError::ObservationChanged(
                    receipt.path().clone(),
                ));
            }
        }
        remove_observed_source_blocking(&rooted, &source)
    })
    .await
    .map_err(|error| ExistingDestinationError::Worker(error.to_string()))?
}

/// The caller owns destination authorization; this helper confines source unlink.
pub(crate) async fn remove_observed_source(rooted: RootedFs, source: Entry) -> Result<()> {
    if source.is_directory() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || remove_observed_source_blocking(&rooted, &source))
        .await
        .map_err(|error| ExistingDestinationError::Worker(error.to_string()))?
}

fn remove_observed_source_blocking(rooted: &RootedFs, source: &Entry) -> Result<()> {
    validate_path(rooted, source)?;
    rooted.remove_blocking(&source.path, false, source.identity)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::engine::domain::Timestamp;

    #[tokio::test]
    async fn source_unlink_refuses_the_original_inode_through_a_raced_symlink_ancestor() {
        let source = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(source.path().join("parent")).unwrap();
        std::fs::write(source.path().join("parent/file"), b"keep").unwrap();
        let metadata = std::fs::metadata(source.path().join("parent/file")).unwrap();
        let mut entry = Entry::file(
            RelativePath::new("parent/file").unwrap(),
            metadata.len(),
            Timestamp::UNIX_EPOCH,
        );
        entry.identity = metadata_identity(&metadata, EntryKind::File);
        let rooted = RootedFs::open(source.path().to_path_buf()).await.unwrap();
        std::fs::rename(source.path().join("parent"), outside.path().join("parent")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("parent"), source.path().join("parent"))
            .unwrap();
        assert_eq!(
            entry.identity,
            metadata_identity(
                &std::fs::metadata(outside.path().join("parent/file")).unwrap(),
                EntryKind::File
            )
        );
        assert!(remove_observed_source(rooted, entry).await.is_err());
        assert_eq!(
            std::fs::read(outside.path().join("parent/file")).unwrap(),
            b"keep"
        );
    }
}
