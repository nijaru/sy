//! Destination publication and parity receipts.
//!
//! Under the 0.5 transactional architecture, mutations to the destination
//! (staging, verification, commit, post-commit finalization) produce a
//! `PublishedDestinationReceipt`.
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
    source_path: RelativePath,
    destination_path: RelativePath,
    source_identity: Option<EntryIdentity>,
    verification_completed: bool,
    preservation_applied: bool,
    finalized: bool,
}

impl PublishedDestinationReceipt {
    /// Construct a publication receipt for a transferred and committed file.
    pub(crate) fn for_file(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: Option<EntryIdentity>,
        verification: &VerificationStatus,
        preservation_applied: bool,
        finalized: bool,
    ) -> Self {
        let verification_completed = !matches!(verification, VerificationStatus::Failed { .. });
        Self {
            source_path,
            destination_path,
            source_identity,
            verification_completed,
            preservation_applied,
            finalized,
        }
    }

    /// Construct a publication receipt for an atomically replaced symlink.
    pub(crate) fn for_symlink(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: Option<EntryIdentity>,
    ) -> Self {
        Self {
            source_path,
            destination_path,
            source_identity,
            verification_completed: true,
            preservation_applied: true,
            finalized: true,
        }
    }

    /// Construct a publication receipt for a hardlinked member.
    pub(crate) fn for_hardlink(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: Option<EntryIdentity>,
    ) -> Self {
        Self {
            source_path,
            destination_path,
            source_identity,
            verification_completed: true,
            preservation_applied: true,
            finalized: true,
        }
    }

    pub fn source_path(&self) -> &RelativePath {
        &self.source_path
    }

    pub fn destination_path(&self) -> &RelativePath {
        &self.destination_path
    }

    pub fn source_identity(&self) -> Option<EntryIdentity> {
        self.source_identity
    }

    /// Required checks completed, including native copies without requested hashing.
    pub fn required_verification_completed(&self) -> bool {
        self.verification_completed
    }

    pub fn is_preservation_applied(&self) -> bool {
        self.preservation_applied
    }

    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Mark post-commit finalization as complete (e.g. after platform/BSD flags).
    pub(crate) fn mark_finalized(&mut self) {
        self.finalized = true;
    }

    /// Validate that this receipt authorizes source removal for `source`.
    ///
    /// Requires:
    /// - Matching source address (independent of the destination name).
    /// - Matching observed source identity.
    /// - Successful verification (not failed).
    /// - Completed finalization.
    pub(crate) fn validate_source_removal(&self, source: &Entry) -> Result<()> {
        if self.source_path != source.path {
            return Err(SyncError::Config(format!(
                "receipt source path '{}' does not match source entry '{}'",
                self.source_path.as_path().display(),
                source.path.as_path().display()
            )));
        }
        if self.source_identity.is_none() || self.source_identity != source.identity {
            return Err(SyncError::SourceChanged {
                path: source.path.as_path().to_path_buf(),
            });
        }
        if !self.verification_completed {
            return Err(SyncError::Config(format!(
                "cannot authorize source removal for '{}': destination verification failed",
                source.path.as_path().display()
            )));
        }
        if !self.preservation_applied {
            return Err(SyncError::Config(format!(
                "cannot authorize source removal for '{}': required preservation incomplete",
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
    source_path: RelativePath,
    destination_path: RelativePath,
    source_identity: EntryIdentity,
    destination_identity: EntryIdentity,
}

impl VerifiedExistingDestinationReceipt {
    // Only the endpoint proof owner may mint an existing-destination receipt.
    pub(super) fn new(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: EntryIdentity,
        destination_identity: EntryIdentity,
    ) -> Self {
        Self {
            source_path,
            destination_path,
            source_identity,
            destination_identity,
        }
    }

    pub fn source_path(&self) -> &RelativePath {
        &self.source_path
    }
    pub fn destination_path(&self) -> &RelativePath {
        &self.destination_path
    }
    pub fn source_identity(&self) -> EntryIdentity {
        self.source_identity
    }
    pub fn destination_identity(&self) -> EntryIdentity {
        self.destination_identity
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
    fn publication_receipt_binds_source_independently_of_destination_name() {
        let source = sample_entry("input", 42);
        let target = RelativePath::new("renamed").unwrap();
        // Native copies do not require an extra BLAKE3 pass unless requested.
        for verification in [
            VerificationStatus::Verified,
            VerificationStatus::NotRequested,
        ] {
            let receipt = PublishedDestinationReceipt::for_file(
                source.path.clone(),
                target.clone(),
                source.identity,
                &verification,
                true,
                true,
            );
            assert_eq!(receipt.source_path(), &source.path);
            assert_eq!(receipt.destination_path(), &target);
            assert!(receipt.validate_source_removal(&source).is_ok());
            assert!(receipt
                .validate_source_removal(&sample_entry("renamed", 42))
                .is_err());
        }
    }

    #[test]
    fn source_removal_requires_identity_preservation_and_completed_verification() {
        let source = sample_entry("input", 42);
        let complete = PublishedDestinationReceipt::for_file(
            source.path.clone(),
            RelativePath::new("renamed").unwrap(),
            source.identity,
            &VerificationStatus::Verified,
            true,
            true,
        );
        assert!(complete
            .validate_source_removal(&sample_entry("input", 99))
            .is_err());
        let mut unidentified = source.clone();
        unidentified.identity = None;
        assert!(complete.validate_source_removal(&unidentified).is_err());

        for incomplete in [
            PublishedDestinationReceipt {
                source_identity: None,
                ..complete.clone()
            },
            PublishedDestinationReceipt {
                preservation_applied: false,
                ..complete.clone()
            },
            PublishedDestinationReceipt {
                finalized: false,
                ..complete.clone()
            },
        ] {
            assert!(incomplete.validate_source_removal(&source).is_err());
        }
        let failed = PublishedDestinationReceipt::for_file(
            source.path.clone(),
            RelativePath::new("renamed").unwrap(),
            source.identity,
            &VerificationStatus::Failed {
                expected: blake3::hash(b"a"),
                actual: blake3::hash(b"b"),
            },
            true,
            true,
        );
        assert!(failed.validate_source_removal(&source).is_err());
        let mut finalized = PublishedDestinationReceipt {
            finalized: false,
            ..complete
        };
        finalized.mark_finalized();
        assert!(finalized.validate_source_removal(&source).is_ok());
    }
}
