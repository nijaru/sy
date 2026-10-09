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

/// Hash exactly the observed byte count and reject either truncation or growth.
/// The caller owns initial/final handle and namespace observation validation.
/// A growing file cannot turn this into an unbounded read-until-EOF workload.
pub(crate) fn hash_observed_bytes(
    reader: &mut impl Read,
    size: u64,
    path: &RelativePath,
) -> Result<[u8; 32]> {
    let mut content = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut remaining = size;
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| ExistingDestinationError::ObservationChanged(path.clone()))?;
        let read = match reader.read(&mut buffer[..limit]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Err(ExistingDestinationError::ObservationChanged(path.clone()));
        }
        content.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let mut extra = [0];
    loop {
        match reader.read(&mut extra) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Ok(0) => return Ok(*content.finalize().as_bytes()),
            Ok(_) => return Err(ExistingDestinationError::ObservationChanged(path.clone())),
            Err(error) => return Err(error.into()),
        }
    }
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
    let content = hash_observed_bytes(&mut file, entry.size, &entry.path)?;

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
        content,
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
    if !source.is_file() || !destination.is_file() {
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
    if source_fingerprint.binding == destination_fingerprint.binding {
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
        destination.path.clone(),
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
    if receipt.source_path() != &source.path || Some(receipt.source_identity()) != source.identity {
        return Err(ExistingDestinationError::ObservationChanged(
            source.path.clone(),
        ));
    }
    tokio::task::spawn_blocking(move || {
        // Local destination validation belongs in the unlink worker, not before
        // it is queued. Remote fingerprints perform this check on the peer;
        // neither case promises cross-endpoint atomic compare-and-swap.
        if let Some(destination) = local_destination {
            if rooted.entry_binding_blocking(receipt.source_path())?
                == destination.entry_binding_blocking(receipt.destination_path())?
            {
                return Err(ExistingDestinationError::SameBinding(
                    receipt.source_path().clone(),
                ));
            }
            if destination.path_identity_blocking(receipt.destination_path())?
                != Some((EntryKind::File, receipt.destination_identity()))
            {
                return Err(ExistingDestinationError::ObservationChanged(
                    receipt.destination_path().clone(),
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

    #[test]
    fn observed_byte_hash_rejects_truncation_and_unbounded_growth() {
        let path = RelativePath::new("file").unwrap();
        for (payload, size, valid) in [
            (&b""[..], 0, true),
            (&b"bytes"[..], 5, true),
            (&b"bytes"[..], 4, false),
            (&b"bytes"[..], 6, false),
        ] {
            let result = hash_observed_bytes(&mut std::io::Cursor::new(payload), size, &path);
            if valid {
                assert_eq!(result.unwrap(), *blake3::hash(payload).as_bytes());
            } else {
                assert!(matches!(
                    result,
                    Err(ExistingDestinationError::ObservationChanged(_))
                ));
            }
        }
        struct GrowingReader {
            bytes: u64,
        }
        impl std::io::Read for GrowingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.bytes += buffer.len() as u64;
                // Bound failed-test cleanup too: a read-until-EOF regression
                // fails this assertion instead of stranding a worker forever.
                assert!(
                    self.bytes <= 1024 * 1024 + 1,
                    "hashing chased growth beyond the observed extent"
                );
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }
        let mut growing = GrowingReader { bytes: 0 };
        assert!(matches!(
            hash_observed_bytes(&mut growing, 1024 * 1024, &path),
            Err(ExistingDestinationError::ObservationChanged(_))
        ));
        assert_eq!(growing.bytes, 1024 * 1024 + 1);
    }

    fn observe_file(root: &std::path::Path, name: &str) -> Entry {
        let metadata = std::fs::metadata(root.join(name)).unwrap();
        let mut entry = Entry::file(
            RelativePath::new(name).unwrap(),
            metadata.len(),
            Timestamp::UNIX_EPOCH,
        );
        entry.identity = metadata_identity(&metadata, EntryKind::File);
        entry
    }

    #[tokio::test]
    async fn renamed_parity_receipt_keeps_source_and_destination_addresses_distinct() {
        for hardlinked in [false, true] {
            let root = tempfile::TempDir::new().unwrap();
            std::fs::write(root.path().join("input"), b"keep at destination").unwrap();
            if hardlinked {
                std::fs::hard_link(root.path().join("input"), root.path().join("renamed")).unwrap();
            } else {
                std::fs::write(root.path().join("renamed"), b"keep at destination").unwrap();
            }
            let source = observe_file(root.path(), "input");
            let destination = observe_file(root.path(), "renamed");
            let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
            let options = FingerprintOptions::default();
            let proof = receipt(
                &source,
                &destination,
                fingerprint(rooted.clone(), source.clone(), options)
                    .await
                    .unwrap(),
                fingerprint(rooted.clone(), destination.clone(), options)
                    .await
                    .unwrap(),
            )
            .unwrap();
            // A hardlink has the same identity; that still cannot turn the
            // destination address into the source this receipt may unlink.
            assert!(remove_verified_source(
                rooted.clone(),
                destination,
                proof.clone(),
                Some(rooted.clone()),
            )
            .await
            .is_err());
            remove_verified_source(rooted.clone(), source, proof, Some(rooted))
                .await
                .unwrap();
            assert!(!root.path().join("input").exists());
            assert_eq!(
                std::fs::read(root.path().join("renamed")).unwrap(),
                b"keep at destination"
            );
        }
    }

    #[tokio::test]
    async fn renamed_parity_receipt_revalidates_destination_before_unlink() {
        let root = tempfile::TempDir::new().unwrap();
        for name in ["input", "renamed"] {
            std::fs::write(root.path().join(name), b"verified contents").unwrap();
        }
        let source = observe_file(root.path(), "input");
        let destination = observe_file(root.path(), "renamed");
        let rooted = RootedFs::open(root.path().to_path_buf()).await.unwrap();
        let options = FingerprintOptions::default();
        let proof = receipt(
            &source,
            &destination,
            fingerprint(rooted.clone(), source.clone(), options)
                .await
                .unwrap(),
            fingerprint(rooted.clone(), destination.clone(), options)
                .await
                .unwrap(),
        )
        .unwrap();
        std::fs::write(root.path().join("replacement"), b"raced destination").unwrap();
        std::fs::rename(root.path().join("replacement"), root.path().join("renamed")).unwrap();
        assert!(matches!(
            remove_verified_source(rooted.clone(), source, proof, Some(rooted)).await,
            Err(ExistingDestinationError::ObservationChanged(path))
                if path.as_path() == std::path::Path::new("renamed")
        ));
        assert_eq!(
            std::fs::read(root.path().join("input")).unwrap(),
            b"verified contents"
        );
        assert_eq!(
            std::fs::read(root.path().join("renamed")).unwrap(),
            b"raced destination"
        );
    }

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
