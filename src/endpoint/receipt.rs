//! Destination publication and parity receipts.
//!
//! Under the 0.5 transactional architecture, mutations to the destination
//! (staging, verification, commit, post-commit finalization) produce an
//! unforgeable `PublishedDestinationReceipt`.
//!
//! The source filesystem is read-only by default. `--remove-source-files` is the
//! single authorized exception, and it strictly requires a verified receipt:
//! - a `PublishedDestinationReceipt` proving destination staging, verification,
//!   atomic commit, and post-commit finalization succeeded; OR
//! - a `VerifiedExistingDestinationReceipt` proving strong content and
//!   preservation parity on an existing destination entry.
//!
//! Quick-check equality (size and mtime alone) NEVER authorizes source removal.

use crate::endpoint::io::VerificationStatus;
use crate::error::{Result, SyncError};
use sy::engine::domain::{Entry, EntryIdentity, RelativePath};

/// Proof that an entry has been published to the destination endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDestinationReceipt {
    path: RelativePath,
    source_identity: Option<EntryIdentity>,
    verified: bool,
    preservation_applied: bool,
    finalized: bool,
}

impl PublishedDestinationReceipt {
    /// Construct a publication receipt for a transferred and committed file.
    pub fn for_file(
        path: RelativePath,
        source_identity: Option<EntryIdentity>,
        verification: &VerificationStatus,
        preservation_applied: bool,
        finalized: bool,
    ) -> Self {
        let verified = !matches!(verification, VerificationStatus::Failed { .. });
        Self {
            path,
            source_identity,
            verified,
            preservation_applied,
            finalized,
        }
    }

    /// Construct a publication receipt for an atomically replaced symlink.
    pub fn for_symlink(path: RelativePath, source_identity: Option<EntryIdentity>) -> Self {
        Self {
            path,
            source_identity,
            verified: true,
            preservation_applied: true,
            finalized: true,
        }
    }

    /// Construct a publication receipt for a hardlinked member.
    pub fn for_hardlink(path: RelativePath, source_identity: Option<EntryIdentity>) -> Self {
        Self {
            path,
            source_identity,
            verified: true,
            preservation_applied: true,
            finalized: true,
        }
    }

    pub fn path(&self) -> &RelativePath {
        &self.path
    }

    pub fn source_identity(&self) -> Option<EntryIdentity> {
        self.source_identity
    }

    pub fn is_verified(&self) -> bool {
        self.verified
    }

    pub fn is_preservation_applied(&self) -> bool {
        self.preservation_applied
    }

    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Mark post-commit finalization as complete (e.g. after platform/BSD flags).
    pub fn mark_finalized(&mut self) {
        self.finalized = true;
    }

    /// Validate that this receipt authorizes source removal for `source`.
    ///
    /// Requires:
    /// - Matching relative path.
    /// - Matching source identity (if scanned).
    /// - Successful verification (not failed).
    /// - Completed finalization.
    pub fn validate_source_removal(&self, source: &Entry) -> Result<()> {
        if self.path != source.path {
            return Err(SyncError::Config(format!(
                "receipt path '{}' does not match source entry '{}'",
                self.path.as_path().display(),
                source.path.as_path().display()
            )));
        }
        if let (Some(expected), Some(actual)) = (self.source_identity, source.identity) {
            if expected != actual {
                return Err(SyncError::SourceChanged {
                    path: source.path.as_path().to_path_buf(),
                });
            }
        }
        if !self.verified {
            return Err(SyncError::Config(format!(
                "cannot authorize source removal for '{}': destination verification failed",
                source.path.as_path().display()
            )));
        }
        if !self.finalized {
            return Err(SyncError::Config(format!(
                "cannot authorize source removal for '{}': post-commit finalization incomplete",
                source.path.as_path().display()
            )));
        }
        Ok(())
    }
}

/// Proof that an existing destination entry is in verified parity with the source.
///
/// Strong content verification (e.g. checksum comparison) and preservation parity
/// are required. Quick-check (size/mtime) equality alone CANNOT produce this receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExistingDestinationReceipt {
    path: RelativePath,
    source_identity: Option<EntryIdentity>,
    content_verified: bool,
}

impl VerifiedExistingDestinationReceipt {
    pub fn new(
        path: RelativePath,
        source_identity: Option<EntryIdentity>,
        content_verified: bool,
    ) -> Result<Self> {
        if !content_verified {
            return Err(SyncError::Config(format!(
                "quick-check equality alone cannot authorize source removal for '{}'",
                path.as_path().display()
            )));
        }
        Ok(Self {
            path,
            source_identity,
            content_verified,
        })
    }

    pub fn path(&self) -> &RelativePath {
        &self.path
    }

    pub fn source_identity(&self) -> Option<EntryIdentity> {
        self.source_identity
    }

    pub fn is_content_verified(&self) -> bool {
        self.content_verified
    }

    pub fn validate_source_removal(&self, source: &Entry) -> Result<()> {
        if self.path != source.path {
            return Err(SyncError::Config(format!(
                "receipt path '{}' does not match source entry '{}'",
                self.path.as_path().display(),
                source.path.as_path().display()
            )));
        }
        if let (Some(expected), Some(actual)) = (self.source_identity, source.identity) {
            if expected != actual {
                return Err(SyncError::SourceChanged {
                    path: source.path.as_path().to_path_buf(),
                });
            }
        }
        if !self.content_verified {
            return Err(SyncError::Config(format!(
                "cannot authorize source removal for '{}': content was not verified",
                source.path.as_path().display()
            )));
        }
        Ok(())
    }
}

/// Unified receipt authorizing source removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationReceipt {
    Published(PublishedDestinationReceipt),
    VerifiedExisting(VerifiedExistingDestinationReceipt),
}

impl DestinationReceipt {
    pub fn path(&self) -> &RelativePath {
        match self {
            Self::Published(receipt) => receipt.path(),
            Self::VerifiedExisting(receipt) => receipt.path(),
        }
    }

    pub fn validate_source_removal(&self, source: &Entry) -> Result<()> {
        match self {
            Self::Published(receipt) => receipt.validate_source_removal(source),
            Self::VerifiedExisting(receipt) => receipt.validate_source_removal(source),
        }
    }
}

impl From<PublishedDestinationReceipt> for DestinationReceipt {
    fn from(receipt: PublishedDestinationReceipt) -> Self {
        Self::Published(receipt)
    }
}

impl From<VerifiedExistingDestinationReceipt> for DestinationReceipt {
    fn from(receipt: VerifiedExistingDestinationReceipt) -> Self {
        Self::VerifiedExisting(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use sy::engine::domain::Timestamp;

    fn sample_entry(path: &str, identity: u64) -> Entry {
        let mut entry = Entry::file(
            RelativePath::new(PathBuf::from(path)).unwrap(),
            10,
            Timestamp::UNIX_EPOCH,
        );
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&identity.to_le_bytes());
        entry.identity = Some(EntryIdentity::from_bytes(bytes));
        entry
    }

    #[test]
    fn published_receipt_validates_matching_source() {
        let entry = sample_entry("foo/bar.txt", 42);
        let receipt = PublishedDestinationReceipt::for_file(
            entry.path.clone(),
            entry.identity,
            &VerificationStatus::Verified,
            true,
            true,
        );
        assert!(receipt.validate_source_removal(&entry).is_ok());
    }

    #[test]
    fn published_receipt_rejects_path_mismatch() {
        let entry = sample_entry("foo/bar.txt", 42);
        let other = sample_entry("foo/other.txt", 42);
        let receipt = PublishedDestinationReceipt::for_file(
            entry.path.clone(),
            entry.identity,
            &VerificationStatus::Verified,
            true,
            true,
        );
        assert!(receipt.validate_source_removal(&other).is_err());
    }

    #[test]
    fn published_receipt_rejects_identity_mismatch() {
        let entry = sample_entry("foo/bar.txt", 42);
        let modified = sample_entry("foo/bar.txt", 99);
        let receipt = PublishedDestinationReceipt::for_file(
            entry.path.clone(),
            entry.identity,
            &VerificationStatus::Verified,
            true,
            true,
        );
        assert!(receipt.validate_source_removal(&modified).is_err());
    }

    #[test]
    fn published_receipt_rejects_failed_verification() {
        let entry = sample_entry("foo/bar.txt", 42);
        let receipt = PublishedDestinationReceipt::for_file(
            entry.path.clone(),
            entry.identity,
            &VerificationStatus::Failed {
                expected: blake3::hash(b"a"),
                actual: blake3::hash(b"b"),
            },
            true,
            true,
        );
        assert!(receipt.validate_source_removal(&entry).is_err());
    }

    #[test]
    fn published_receipt_rejects_incomplete_finalization() {
        let entry = sample_entry("foo/bar.txt", 42);
        let mut receipt = PublishedDestinationReceipt::for_file(
            entry.path.clone(),
            entry.identity,
            &VerificationStatus::Verified,
            true,
            false,
        );
        assert!(receipt.validate_source_removal(&entry).is_err());
        receipt.mark_finalized();
        assert!(receipt.validate_source_removal(&entry).is_ok());
    }

    #[test]
    fn verified_existing_receipt_requires_content_verification() {
        let entry = sample_entry("foo/bar.txt", 42);
        assert!(
            VerifiedExistingDestinationReceipt::new(entry.path.clone(), entry.identity, false)
                .is_err()
        );

        let receipt =
            VerifiedExistingDestinationReceipt::new(entry.path.clone(), entry.identity, true)
                .unwrap();
        assert!(receipt.validate_source_removal(&entry).is_ok());
    }
}
